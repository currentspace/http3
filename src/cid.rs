//! Connection ID generation: random CIDs for standard use and optional
//! QUIC-LB-compatible CIDs with embedded server IDs.

#![deny(unsafe_code)]

#[cfg(feature = "os-runtime")]
use ring::rand::SecureRandom;

use crate::error::Http3NativeError;
use crate::proof_core::cid_model::{self, CidModelError};

pub const SCID_LEN: usize = quiche::MAX_CONN_ID_LEN;
pub const QUIC_LB_SERVER_ID_LEN: usize = cid_model::QUIC_LB_SERVER_ID_LEN;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CidEncoding {
    Random,
    QuicLbPlaintext {
        server_id: [u8; QUIC_LB_SERVER_ID_LEN],
        config_rotation: u8,
    },
}

impl CidEncoding {
    pub fn random() -> Self {
        Self::Random
    }

    pub fn quic_lb_plaintext(
        server_id: [u8; QUIC_LB_SERVER_ID_LEN],
        config_rotation: u8,
    ) -> Result<Self, Http3NativeError> {
        if !cid_model::valid_config_rotation(config_rotation) {
            return Err(Http3NativeError::Config(
                "quic_lb config_rotation must be in range 0..=6 (0b111 is reserved)".into(),
            ));
        }

        Ok(Self::QuicLbPlaintext {
            server_id,
            config_rotation,
        })
    }

    /// Native path: sources 20 bytes of entropy from `ring`'s system RNG.
    /// Requires `os-runtime` since `ring` is gated behind it (A1 task 1).
    #[cfg(feature = "os-runtime")]
    pub fn generate_scid(&self) -> Result<Vec<u8>, Http3NativeError> {
        let rng = ring::rand::SystemRandom::new();
        let mut scid = vec![0u8; SCID_LEN];
        rng.fill(&mut scid)
            .map_err(|_| Http3NativeError::Config("cryptographic RNG failed".into()))?;
        self.apply_encoding(&mut scid)?;
        Ok(scid)
    }

    /// Sans-IO path: build an SCID from caller-supplied random bytes (e.g.
    /// 20 bytes from a JS host RNG in a future wasm build) instead of
    /// requiring `ring`. `random_bytes` must be exactly [`SCID_LEN`] bytes
    /// of cryptographically-secure entropy — the caller is responsible for
    /// sourcing it.
    pub fn generate_scid_from_bytes(&self, random_bytes: &[u8]) -> Result<Vec<u8>, Http3NativeError> {
        if random_bytes.len() != SCID_LEN {
            return Err(Http3NativeError::Config(format!(
                "random_bytes must be exactly {SCID_LEN} bytes",
            )));
        }
        let mut scid = random_bytes.to_vec();
        self.apply_encoding(&mut scid)?;
        Ok(scid)
    }

    /// Apply this encoding's QUIC-LB transform (if any) to an already
    /// `SCID_LEN`-sized buffer of random bytes. Shared by both entropy
    /// sources above.
    fn apply_encoding(&self, scid: &mut [u8]) -> Result<(), Http3NativeError> {
        if let Self::QuicLbPlaintext {
            server_id,
            config_rotation,
        } = self
        {
            apply_quic_lb_plaintext(scid, *server_id, *config_rotation)?;
        }
        Ok(())
    }
}

impl Default for CidEncoding {
    fn default() -> Self {
        Self::Random
    }
}

pub fn parse_server_id_bytes(
    server_id: &[u8],
) -> Result<[u8; QUIC_LB_SERVER_ID_LEN], Http3NativeError> {
    if server_id.len() != QUIC_LB_SERVER_ID_LEN {
        return Err(Http3NativeError::Config(format!(
            "server_id must be exactly {QUIC_LB_SERVER_ID_LEN} bytes",
        )));
    }

    let mut out = [0u8; QUIC_LB_SERVER_ID_LEN];
    out.copy_from_slice(server_id);
    Ok(out)
}

pub fn apply_quic_lb_plaintext(
    scid: &mut [u8],
    server_id: [u8; QUIC_LB_SERVER_ID_LEN],
    config_rotation: u8,
) -> Result<(), Http3NativeError> {
    if scid.len() != SCID_LEN {
        return Err(Http3NativeError::Config(format!(
            "scid length must be exactly {SCID_LEN} bytes",
        )));
    }

    cid_model::apply_quic_lb_plaintext(scid, SCID_LEN, server_id, config_rotation)
        .map_err(cid_model_error)
}

fn cid_model_error(err: CidModelError) -> Http3NativeError {
    match err {
        CidModelError::InvalidScidLen => {
            Http3NativeError::Config(format!("scid length must be exactly {SCID_LEN} bytes",))
        }
        CidModelError::InvalidConfigRotation => Http3NativeError::Config(
            "quic_lb config_rotation must be in range 0..=6 (0b111 is reserved)".into(),
        ),
    }
}

#[cfg(test)]
pub fn extract_plaintext_server_id(scid: &[u8]) -> Option<[u8; QUIC_LB_SERVER_ID_LEN]> {
    if scid.len() < 1 + QUIC_LB_SERVER_ID_LEN {
        return None;
    }
    let mut out = [0u8; QUIC_LB_SERVER_ID_LEN];
    out.copy_from_slice(&scid[1..1 + QUIC_LB_SERVER_ID_LEN]);
    Some(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[cfg(feature = "os-runtime")]
    #[test]
    fn random_cid_encoding_generates_random_scids() {
        let encoding = CidEncoding::random();
        let a = encoding.generate_scid().expect("scid A");
        let b = encoding.generate_scid().expect("scid B");

        assert_eq!(a.len(), SCID_LEN);
        assert_eq!(b.len(), SCID_LEN);
        assert_ne!(a, b);
    }

    #[cfg(feature = "os-runtime")]
    #[test]
    fn quic_lb_plaintext_embeds_server_id() {
        let sid = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88];
        let encoding = CidEncoding::quic_lb_plaintext(sid, 0).expect("valid config");
        let scid = encoding.generate_scid().expect("generated scid");

        assert_eq!(scid.len(), SCID_LEN);
        assert_eq!(extract_plaintext_server_id(&scid), Some(sid));
        assert_eq!(scid[0] >> 5, 0);
    }

    #[cfg(feature = "os-runtime")]
    #[test]
    fn quic_lb_plaintext_sets_config_rotation_bits() {
        let sid = [0xaa; QUIC_LB_SERVER_ID_LEN];
        let encoding = CidEncoding::quic_lb_plaintext(sid, 6).expect("valid config");
        let scid = encoding.generate_scid().expect("generated scid");

        assert_eq!(scid[0] >> 5, 6);
        assert_eq!(extract_plaintext_server_id(&scid), Some(sid));
    }

    /// Sans-IO / wasm-friendly path: caller-supplied bytes instead of `ring`.
    #[test]
    fn generate_scid_from_bytes_uses_caller_supplied_entropy() {
        let encoding = CidEncoding::random();
        let a = encoding
            .generate_scid_from_bytes(&[0x11; SCID_LEN])
            .expect("scid A");
        let b = encoding
            .generate_scid_from_bytes(&[0x22; SCID_LEN])
            .expect("scid B");

        assert_eq!(a, vec![0x11; SCID_LEN]);
        assert_eq!(b, vec![0x22; SCID_LEN]);
    }

    #[test]
    fn generate_scid_from_bytes_rejects_wrong_length() {
        let encoding = CidEncoding::random();
        let err = encoding
            .generate_scid_from_bytes(&[0u8; 4])
            .expect_err("wrong length");
        assert!(
            err.to_string()
                .contains("random_bytes must be exactly 20 bytes")
        );
    }

    #[test]
    fn generate_scid_from_bytes_applies_quic_lb_encoding() {
        let sid = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88];
        let encoding = CidEncoding::quic_lb_plaintext(sid, 3).expect("valid config");
        let scid = encoding
            .generate_scid_from_bytes(&[0xAB; SCID_LEN])
            .expect("generated scid");

        assert_eq!(scid.len(), SCID_LEN);
        assert_eq!(extract_plaintext_server_id(&scid), Some(sid));
        assert_eq!(scid[0] >> 5, 3);
    }

    #[test]
    fn parse_server_id_rejects_wrong_length() {
        let err = parse_server_id_bytes(&[0u8; 7]).expect_err("wrong length");
        assert!(
            err.to_string()
                .contains("server_id must be exactly 8 bytes")
        );
    }

    #[test]
    fn reject_failover_config_rotation_codepoint() {
        let sid = [0u8; QUIC_LB_SERVER_ID_LEN];
        let err = CidEncoding::quic_lb_plaintext(sid, 7).expect_err("invalid config rotation");
        assert!(
            err.to_string()
                .contains("quic_lb config_rotation must be in range 0..=6")
        );
    }
}
