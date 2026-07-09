//! Connection routing by CID, retry-token validation, and session lifecycle
//! management for both HTTP/3 and raw QUIC servers.
//!
//! Always compiled (no `os-runtime` gate on the module itself as of the
//! server-side wasm work — see `docs/WASM_CLIENT_PLAN.md`'s server-support
//! extension): routing/token logic uses only the always-compiled
//! `retry_token` (boring-HMAC) helper. The `ring`-backed native
//! convenience constructors (`new`/`with_max_connections`/
//! `with_max_connections_and_cid`) and the ring-backed
//! `generate_scid`/`generate_random_scid` accessors stay individually
//! `#[cfg(feature = "os-runtime")]`-gated below — `ring` itself is an
//! optional dependency tied to that feature and must stay that way.

#![deny(unsafe_code)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::arc_buf::ArcBufFactory;
use crate::connection::{H3Connection, H3ConnectionInit};
use crate::error::Http3NativeError;
use crate::retry_token::{self, DeterministicScidSource};

pub const SCID_LEN: usize = crate::cid::SCID_LEN;
const TOKEN_LIFETIME_SECS: u64 = 60;
const DEFAULT_MAX_CONNECTIONS: usize = 10_000;

pub struct ConnectionMap {
    /// Map from DCID (as bytes) to connection handle in the slab.
    /// A single connection may have multiple DCIDs mapped to it.
    by_dcid: HashMap<Vec<u8>, usize>,
    /// Slab-based storage for connections.
    connections: slab::Slab<H3Connection>,
    /// HMAC-SHA256 key for minting/validating retry tokens (raw bytes, not
    /// `ring::hmac::Key` — see `retry_token.rs`).
    token_key: [u8; 32],
    /// Maximum number of concurrent connections.
    max_connections: usize,
    /// Strategy for generating server-side SCIDs.
    cid_encoding: crate::cid::CidEncoding,
    /// Sans-IO SCID generator for [`ConnectionMap::generate_scid_direct`]
    /// (keyed off the same `token_key`, domain-separated — see
    /// `retry_token.rs`). Native's own `generate_scid` (ring-backed) is
    /// unaffected by this and doesn't touch it.
    scid_source: DeterministicScidSource,
}

impl ConnectionMap {
    #[cfg(feature = "os-runtime")]
    pub fn new() -> Self {
        Self::with_max_connections(DEFAULT_MAX_CONNECTIONS)
    }

    #[cfg(feature = "os-runtime")]
    pub fn with_max_connections(max: usize) -> Self {
        Self::with_max_connections_and_cid(max, crate::cid::CidEncoding::random())
    }

    /// Native constructor: sources the 32-byte HMAC key from `ring`'s
    /// system RNG. Requires `os-runtime` for the same reason as
    /// `H3ClientHandler::new` (A1 task 1) — every native call site already
    /// runs with `os-runtime` on.
    #[cfg(feature = "os-runtime")]
    pub fn with_max_connections_and_cid(max: usize, cid_encoding: crate::cid::CidEncoding) -> Self {
        use ring::rand::SecureRandom;
        let rng = ring::rand::SystemRandom::new();
        let mut key_bytes = [0u8; 32];
        #[allow(clippy::expect_used)]
        rng.fill(&mut key_bytes)
            .expect("system RNG should not fail");
        Self::with_key_bytes(max, cid_encoding, key_bytes)
    }

    /// Sans-IO constructor for a direct-call caller (a wasm ABI, or the
    /// unit tests below): the caller supplies the 32-byte HMAC key
    /// directly (e.g. from a JS host RNG via `crypto.getRandomValues`)
    /// instead of requiring `ring`. This key doubles as the seed for
    /// [`ConnectionMap::generate_scid_direct`]'s deterministic PRF — see
    /// `retry_token.rs` for why sharing one key across both uses is sound.
    pub fn with_key_bytes(
        max: usize,
        cid_encoding: crate::cid::CidEncoding,
        key_bytes: [u8; 32],
    ) -> Self {
        Self {
            by_dcid: HashMap::new(),
            connections: slab::Slab::new(),
            token_key: key_bytes,
            max_connections: max,
            cid_encoding,
            scid_source: DeterministicScidSource::new(key_bytes),
        }
    }

    /// Generate a new Source Connection ID for server-side use. Native
    /// only: sources entropy from `ring`'s system RNG via `CidEncoding`.
    #[cfg(feature = "os-runtime")]
    pub fn generate_scid(&self) -> Result<Vec<u8>, Http3NativeError> {
        self.cid_encoding.generate_scid()
    }

    /// Sans-IO alternative to [`ConnectionMap::generate_scid`] for the
    /// direct-call / wasm server surface: deterministic HMAC-PRF
    /// derivation from this map's own `token_key`, no `ring` involved.
    pub fn generate_scid_direct(&mut self) -> Result<Vec<u8>, Http3NativeError> {
        self.scid_source.next_scid(&self.cid_encoding)
    }

    /// Generate a random Source Connection ID (used by client workers).
    /// Native only — see [`ConnectionMap::generate_scid`].
    #[cfg(feature = "os-runtime")]
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

        let tag = retry_token::hmac_sha256(&self.token_key, &payload);
        let mut token = tag.to_vec();
        token.extend_from_slice(&payload);
        token
    }

    /// Validate a retry token. Returns the original DCID if valid.
    pub fn validate_token(&self, token: &[u8], peer: &SocketAddr) -> Option<Vec<u8>> {
        if token.len() < 32 {
            return None; // Too short for HMAC tag
        }

        let (tag_bytes, payload) = token.split_at(32);
        // Verify HMAC (constant-time — see retry_token::hmac_sha256_verify)
        if !retry_token::hmac_sha256_verify(&self.token_key, payload, tag_bytes) {
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
        // Audit finding #4: use a signed window so a backwards clock jump
        // doesn't suddenly accept all in-flight tokens (saturating_sub
        // would underflow to 0 there) and a forwards jump doesn't reject
        // freshly minted tokens.
        let skew = (now as i64).saturating_sub(timestamp as i64).abs();
        if skew > TOKEN_LIFETIME_SECS as i64 {
            return None; // Token expired or clock skew too large
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

    /// Number of live connections currently tracked (used by the
    /// direct-call/wasm surface's "is the whole server idle" check).
    pub fn len(&self) -> usize {
        self.connections.len()
    }

    pub fn is_empty(&self) -> bool {
        self.connections.is_empty()
    }

    /// Remove all closed connections, returning their handles and final state.
    pub fn drain_closed(&mut self) -> Vec<(usize, H3Connection)> {
        let closed: Vec<usize> = self
            .connections
            .iter()
            .filter(|(_, conn)| conn.is_closed())
            .map(|(handle, _)| handle)
            .collect();

        closed
            .into_iter()
            .filter_map(|handle| self.remove(handle).map(|conn| (handle, conn)))
            .collect()
    }
}

#[cfg(feature = "os-runtime")]
impl Default for ConnectionMap {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Always-compiled test helper: a fixed, non-secret 32-byte key (this
    /// is a test, not a deployment) via [`ConnectionMap::with_key_bytes`],
    /// so the bulk of this module's coverage (routing, tokens, DCID
    /// bookkeeping) runs identically under `--no-default-features` and
    /// under the native default feature set, instead of depending on the
    /// `os-runtime`-gated, `ring`-backed `new()`.
    fn test_map() -> ConnectionMap {
        ConnectionMap::with_key_bytes(DEFAULT_MAX_CONNECTIONS, crate::cid::CidEncoding::random(), [0x42; 32])
    }

    #[cfg(feature = "os-runtime")]
    #[test]
    fn test_generate_scid() {
        let map = ConnectionMap::new();
        let scid1 = map.generate_scid().expect("should generate");
        let scid2 = map.generate_scid().expect("should generate");
        assert_eq!(scid1.len(), SCID_LEN);
        assert_ne!(scid1, scid2);
    }

    #[cfg(feature = "os-runtime")]
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

    /// Sans-IO / wasm-friendly path: no `ring`, deterministic HMAC-PRF
    /// instead (see `retry_token.rs`).
    #[test]
    fn test_generate_scid_direct() {
        let mut map = test_map();
        let scid1 = map.generate_scid_direct().expect("should generate");
        let scid2 = map.generate_scid_direct().expect("should generate");
        assert_eq!(scid1.len(), SCID_LEN);
        assert_ne!(scid1, scid2);
    }

    #[test]
    fn test_generate_scid_direct_quic_lb_server_id() {
        let server_id = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88];
        let mut map = ConnectionMap::with_key_bytes(
            8,
            crate::cid::CidEncoding::quic_lb_plaintext(server_id, 0).expect("valid cid encoding"),
            [0x99; 32],
        );
        let scid = map.generate_scid_direct().expect("should generate");
        assert_eq!(scid.len(), SCID_LEN);
        assert_eq!(
            crate::cid::extract_plaintext_server_id(&scid),
            Some(server_id)
        );
    }

    #[test]
    fn test_route_not_found() {
        let map = test_map();
        assert!(map.route_packet(&[1, 2, 3]).is_none());
    }

    #[test]
    fn test_token_roundtrip() {
        let map = test_map();
        let peer: SocketAddr = "127.0.0.1:12345".parse().expect("valid addr");
        let odcid = vec![0xab; 16];
        let token = map.mint_token(&peer, &odcid);

        let result = map.validate_token(&token, &peer);
        assert_eq!(result, Some(odcid));
    }

    #[test]
    fn test_token_wrong_address() {
        let map = test_map();
        let peer1: SocketAddr = "127.0.0.1:12345".parse().expect("valid addr");
        let peer2: SocketAddr = "127.0.0.2:12345".parse().expect("valid addr");
        let token = map.mint_token(&peer1, &[0xab; 16]);

        assert!(map.validate_token(&token, &peer2).is_none());
    }

    #[test]
    fn test_token_tampered() {
        let map = test_map();
        let peer: SocketAddr = "127.0.0.1:12345".parse().expect("valid addr");
        let mut token = map.mint_token(&peer, &[0xab; 16]);
        token[0] ^= 0xff; // Tamper with HMAC

        assert!(map.validate_token(&token, &peer).is_none());
    }

    #[test]
    fn test_max_connections() {
        let map = ConnectionMap::with_key_bytes(0, crate::cid::CidEncoding::random(), [0x01; 32]);
        // Can't test accept_new without quiche config, but we can verify the limit field
        assert_eq!(map.max_connections, 0);
    }

    #[test]
    fn test_remove_cleans_all_dcids() {
        let mut map = test_map();
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

    #[test]
    fn test_token_roundtrip_ipv6() {
        let map = test_map();
        let peer: SocketAddr = "[::1]:12345".parse().expect("valid addr");
        let odcid = vec![0xcd; 16];
        let token = map.mint_token(&peer, &odcid);

        let result = map.validate_token(&token, &peer);
        assert_eq!(result, Some(odcid));
    }

    #[test]
    fn test_token_too_short_rejected() {
        let map = test_map();
        let peer: SocketAddr = "127.0.0.1:1234".parse().expect("valid addr");
        assert!(map.validate_token(&[0u8; 16], &peer).is_none());
    }

    #[test]
    fn test_add_dcid_for_nonexistent_handle() {
        let mut map = test_map();
        map.add_dcid(999, vec![1, 2, 3]);
        assert!(map.route_packet(&[1, 2, 3]).is_none());
    }

    #[test]
    fn test_fill_handles_empty_map() {
        let map = test_map();
        let mut buf = Vec::<usize>::new();
        map.fill_handles(&mut buf);
        assert!(buf.is_empty());
        assert_eq!(map.len(), 0);
        assert!(map.is_empty());
    }

    #[test]
    fn test_remove_nonexistent_handle_returns_none() {
        let mut map = test_map();
        assert!(map.remove(999).is_none());
    }

    /// Hand-craft a retry-token payload with a chosen timestamp so we can
    /// exercise the clock-skew window without mocking SystemTime.
    fn craft_token_with_ts(
        map: &ConnectionMap,
        peer: &SocketAddr,
        odcid: &[u8],
        ts: u64,
    ) -> Vec<u8> {
        let mut payload = Vec::new();
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
        payload.extend_from_slice(&ts.to_be_bytes());
        payload.push(odcid.len() as u8);
        payload.extend_from_slice(odcid);
        let tag = retry_token::hmac_sha256(&map.token_key, &payload);
        let mut token = tag.to_vec();
        token.extend_from_slice(&payload);
        token
    }

    /// Audit finding #4: a token whose timestamp is far in the future
    /// (clock-skew style) must be rejected, not silently accepted because
    /// `now.saturating_sub(timestamp)` underflows to 0.
    #[test]
    fn test_token_future_timestamp_rejected() {
        let map = test_map();
        let peer: SocketAddr = "127.0.0.1:443".parse().expect("valid addr");
        let odcid = vec![0xab; 16];

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        // Timestamp 1 hour ahead — well outside ±60s window.
        let token = craft_token_with_ts(&map, &peer, &odcid, now + 3600);

        assert!(map.validate_token(&token, &peer).is_none());
    }

    #[test]
    fn test_token_past_timestamp_rejected() {
        let map = test_map();
        let peer: SocketAddr = "127.0.0.1:443".parse().expect("valid addr");
        let odcid = vec![0xab; 16];

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        // Timestamp 1 hour behind.
        let token = craft_token_with_ts(&map, &peer, &odcid, now - 3600);

        assert!(map.validate_token(&token, &peer).is_none());
    }

    #[test]
    fn test_token_within_window_accepted() {
        let map = test_map();
        let peer: SocketAddr = "127.0.0.1:443".parse().expect("valid addr");
        let odcid = vec![0xab; 16];

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        // Timestamp 30s in the past — within the 60s window.
        let token = craft_token_with_ts(&map, &peer, &odcid, now - 30);

        assert_eq!(map.validate_token(&token, &peer), Some(odcid));
    }
}
