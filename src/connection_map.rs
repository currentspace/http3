//! Connection routing by CID, retry-token validation, and session lifecycle
//! management for both HTTP/3 and raw QUIC servers.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{SystemTime, UNIX_EPOCH};

use ring::hmac;
use ring::rand::SecureRandom;

use crate::arc_buf::ArcBufFactory;
use crate::connection::{H3Connection, H3ConnectionInit};
use crate::error::Http3NativeError;

pub const SCID_LEN: usize = crate::cid::SCID_LEN;
const TOKEN_LIFETIME_SECS: u64 = 60;
const DEFAULT_MAX_CONNECTIONS: usize = 10_000;

pub struct ConnectionMap {
    /// Map from DCID (as bytes) to connection handle in the slab.
    /// A single connection may have multiple DCIDs mapped to it.
    by_dcid: HashMap<Vec<u8>, usize>,
    /// Slab-based storage for connections.
    connections: slab::Slab<H3Connection>,
    /// HMAC key for minting/validating retry tokens.
    token_key: hmac::Key,
    /// Maximum number of concurrent connections.
    max_connections: usize,
    /// Strategy for generating server-side SCIDs.
    cid_encoding: crate::cid::CidEncoding,
}

impl ConnectionMap {
    pub fn new() -> Self {
        Self::with_max_connections(DEFAULT_MAX_CONNECTIONS)
    }

    pub fn with_max_connections(max: usize) -> Self {
        Self::with_max_connections_and_cid(max, crate::cid::CidEncoding::random())
    }

    pub fn with_max_connections_and_cid(max: usize, cid_encoding: crate::cid::CidEncoding) -> Self {
        let rng = ring::rand::SystemRandom::new();
        let mut key_bytes = [0u8; 32];
        #[allow(clippy::expect_used)]
        rng.fill(&mut key_bytes)
            .expect("system RNG should not fail");
        Self {
            by_dcid: HashMap::new(),
            connections: slab::Slab::new(),
            token_key: hmac::Key::new(hmac::HMAC_SHA256, &key_bytes),
            max_connections: max,
            cid_encoding,
        }
    }

    /// Generate a new Source Connection ID for server-side use.
    pub fn generate_scid(&self) -> Result<Vec<u8>, Http3NativeError> {
        self.cid_encoding.generate_scid()
    }

    /// Generate a random Source Connection ID (used by client workers).
    pub fn generate_random_scid() -> Result<Vec<u8>, Http3NativeError> {
        crate::cid::CidEncoding::random().generate_scid()
    }

    /// Look up a connection by DCID parsed from an incoming packet.
    pub fn route_packet(&self, dcid: &[u8]) -> Option<usize> {
        self.by_dcid.get(dcid).copied()
    }

    /// Register an additional DCID for an existing connection.
    /// Called when quiche rotates connection IDs.
    pub fn add_dcid(&mut self, handle: usize, dcid: Vec<u8>) {
        if self.connections.contains(handle) {
            self.by_dcid.insert(dcid, handle);
        }
    }

    /// Remove a DCID mapping, if present.
    pub fn remove_dcid(&mut self, dcid: &[u8]) {
        self.by_dcid.remove(dcid);
    }

    /// Mint a stateless retry token for address validation.
    /// Token format: HMAC(peer_addr_bytes || timestamp) || peer_addr_bytes || timestamp
    pub fn mint_token(&self, peer: &SocketAddr, odcid: &[u8]) -> Vec<u8> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let mut payload = Vec::new();
        // Encode peer address
        match peer {
            SocketAddr::V4(v4) => {
                payload.push(4);
                payload.extend_from_slice(&v4.ip().octets());
                payload.extend_from_slice(&v4.port().to_be_bytes());
            }
            SocketAddr::V6(v6) => {
                payload.push(6);
                payload.extend_from_slice(&v6.ip().octets());
                payload.extend_from_slice(&v6.port().to_be_bytes());
            }
        }
        // Encode timestamp
        payload.extend_from_slice(&now.to_be_bytes());
        // Encode original DCID
        payload.push(odcid.len() as u8);
        payload.extend_from_slice(odcid);

        let tag = hmac::sign(&self.token_key, &payload);
        let mut token = tag.as_ref().to_vec();
        token.extend_from_slice(&payload);
        token
    }

    /// Validate a retry token. Returns the original DCID if valid.
    pub fn validate_token(&self, token: &[u8], peer: &SocketAddr) -> Option<Vec<u8>> {
        if token.len() < 32 {
            return None; // Too short for HMAC tag
        }

        let (tag_bytes, payload) = token.split_at(32);
        // Verify HMAC
        if hmac::verify(&self.token_key, payload, tag_bytes).is_err() {
            return None;
        }

        // Parse payload
        let mut pos = 0;
        if pos >= payload.len() {
            return None;
        }
        let family = payload[pos];
        pos += 1;

        // Verify peer address matches
        match (family, peer) {
            (4, SocketAddr::V4(v4)) => {
                if payload.len() < pos + 6 {
                    return None;
                }
                if payload[pos..pos + 4] != v4.ip().octets() {
                    return None;
                }
                pos += 4;
                if payload[pos..pos + 2] != v4.port().to_be_bytes() {
                    return None;
                }
                pos += 2;
            }
            (6, SocketAddr::V6(v6)) => {
                if payload.len() < pos + 18 {
                    return None;
                }
                if payload[pos..pos + 16] != v6.ip().octets() {
                    return None;
                }
                pos += 16;
                if payload[pos..pos + 2] != v6.port().to_be_bytes() {
                    return None;
                }
                pos += 2;
            }
            _ => return None,
        }

        // Check timestamp
        if payload.len() < pos + 8 {
            return None;
        }
        let timestamp = u64::from_be_bytes(payload[pos..pos + 8].try_into().ok()?);
        pos += 8;

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if now.saturating_sub(timestamp) > TOKEN_LIFETIME_SECS {
            return None; // Token expired
        }

        // Extract original DCID
        if pos >= payload.len() {
            return None;
        }
        let odcid_len = payload[pos] as usize;
        pos += 1;
        if payload.len() < pos + odcid_len {
            return None;
        }
        Some(payload[pos..pos + odcid_len].to_vec())
    }

    /// Accept a new server-side connection.
    #[allow(clippy::too_many_arguments)]
    pub fn accept_new(
        &mut self,
        scid: &[u8],
        odcid: Option<&quiche::ConnectionId<'_>>,
        peer: SocketAddr,
        local: SocketAddr,
        config: &mut quiche::Config,
        qlog_dir: Option<&str>,
        qlog_level: Option<&str>,
        qpack_max_table_capacity: Option<u64>,
        qpack_blocked_streams: Option<u64>,
    ) -> Result<usize, Http3NativeError> {
        if self.connections.len() >= self.max_connections {
            return Err(Http3NativeError::Config(format!(
                "max connections ({}) reached",
                self.max_connections
            )));
        }

        let scid_owned = scid.to_vec();
        let scid_ref = quiche::ConnectionId::from_ref(scid);

        let quiche_conn =
            quiche::accept_with_buf_factory::<ArcBufFactory>(&scid_ref, odcid, local, peer, config)
                .map_err(Http3NativeError::Quiche)?;

        let conn = H3Connection::new(
            quiche_conn,
            scid_owned.clone(),
            H3ConnectionInit {
                role: "server",
                qlog_dir,
                qlog_level,
                qpack_max_table_capacity,
                qpack_blocked_streams,
            },
        );
        let handle = self.connections.insert(conn);
        self.by_dcid.insert(scid_owned, handle);

        Ok(handle)
    }

    /// Get a connection by handle.
    pub fn get(&self, handle: usize) -> Option<&H3Connection> {
        self.connections.get(handle)
    }

    /// Get a mutable connection by handle.
    pub fn get_mut(&mut self, handle: usize) -> Option<&mut H3Connection> {
        self.connections.get_mut(handle)
    }

    /// Remove a closed connection and all its DCID mappings.
    pub fn remove(&mut self, handle: usize) -> Option<H3Connection> {
        if self.connections.contains(handle) {
            let conn = self.connections.remove(handle);
            // Remove all DCID entries pointing to this handle
            self.by_dcid.retain(|_, &mut h| h != handle);
            Some(conn)
        } else {
            None
        }
    }

    /// Fill a reusable buffer with all connection handles, avoiding allocation.
    pub fn fill_handles(&self, buf: &mut Vec<usize>) {
        buf.clear();
        buf.extend(self.connections.iter().map(|(handle, _)| handle));
    }

    /// Remove all closed connections, returning their handles.
    pub fn drain_closed(&mut self) -> Vec<usize> {
        let closed: Vec<usize> = self
            .connections
            .iter()
            .filter(|(_, conn)| conn.is_closed())
            .map(|(handle, _)| handle)
            .collect();

        for &handle in &closed {
            self.remove(handle);
        }
        closed
    }
}

impl Default for ConnectionMap {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_scid() {
        let map = ConnectionMap::new();
        let scid1 = map.generate_scid().expect("should generate");
        let scid2 = map.generate_scid().expect("should generate");
        assert_eq!(scid1.len(), SCID_LEN);
        assert_ne!(scid1, scid2);
    }

    #[test]
    fn test_generate_scid_quic_lb_server_id() {
        let server_id = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88];
        let map = ConnectionMap::with_max_connections_and_cid(
            8,
            crate::cid::CidEncoding::quic_lb_plaintext(server_id, 0).expect("valid cid encoding"),
        );
        let scid = map.generate_scid().expect("should generate");
        assert_eq!(scid.len(), SCID_LEN);
        assert_eq!(
            crate::cid::extract_plaintext_server_id(&scid),
            Some(server_id)
        );
    }

    #[test]
    fn test_route_not_found() {
        let map = ConnectionMap::new();
        assert!(map.route_packet(&[1, 2, 3]).is_none());
    }

    #[test]
    fn test_token_roundtrip() {
        let map = ConnectionMap::new();
        let peer: SocketAddr = "127.0.0.1:12345".parse().expect("valid addr");
        let odcid = vec![0xab; 16];
        let token = map.mint_token(&peer, &odcid);

        let result = map.validate_token(&token, &peer);
        assert_eq!(result, Some(odcid));
    }

    #[test]
    fn test_token_wrong_address() {
        let map = ConnectionMap::new();
        let peer1: SocketAddr = "127.0.0.1:12345".parse().expect("valid addr");
        let peer2: SocketAddr = "127.0.0.2:12345".parse().expect("valid addr");
        let token = map.mint_token(&peer1, &[0xab; 16]);

        assert!(map.validate_token(&token, &peer2).is_none());
    }

    #[test]
    fn test_token_tampered() {
        let map = ConnectionMap::new();
        let peer: SocketAddr = "127.0.0.1:12345".parse().expect("valid addr");
        let mut token = map.mint_token(&peer, &[0xab; 16]);
        token[0] ^= 0xff; // Tamper with HMAC

        assert!(map.validate_token(&token, &peer).is_none());
    }

    #[test]
    fn test_max_connections() {
        let map = ConnectionMap::with_max_connections(0);
        // Can't test accept_new without quiche config, but we can verify the limit field
        assert_eq!(map.max_connections, 0);
    }

    #[test]
    fn test_remove_cleans_all_dcids() {
        let mut map = ConnectionMap::new();
        // Simulate adding multiple DCIDs for the same handle
        // We can't do accept_new without quiche, but we can test the DCID map directly
        map.by_dcid.insert(vec![1, 2, 3], 42);
        map.by_dcid.insert(vec![4, 5, 6], 42);
        map.by_dcid.insert(vec![7, 8, 9], 99); // Different handle

        // Manually remove handle 42's entries
        map.by_dcid.retain(|_, &mut h| h != 42);
        assert!(map.route_packet(&[1, 2, 3]).is_none());
        assert!(map.route_packet(&[4, 5, 6]).is_none());
        assert_eq!(map.route_packet(&[7, 8, 9]), Some(99));
    }
}
