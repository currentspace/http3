//! Connection ID generation: random CIDs for standard use and optional
//! QUIC-LB-compatible CIDs with embedded server IDs.

use ring::rand::SecureRandom;

use crate::error::Http3NativeError;

pub const SCID_LEN: usize = quiche::MAX_CONN_ID_LEN;
pub const QUIC_LB_SERVER_ID_LEN: usize = 8;

const CONFIG_ROTATION_MASK: u8 = 0b1110_0000;
const RANDOM_LOW_BITS_MASK: u8 = 0b0001_1111;
const MAX_CONFIG_ROTATION: u8 = 0b110;

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
        if config_rotation > MAX_CONFIG_ROTATION {
            return Err(Http3NativeError::Config(
                "quic_lb config_rotation must be in range 0..=6 (0b111 is reserved)".into(),
            ));
        }

        Ok(Self::QuicLbPlaintext {
            server_id,
            config_rotation,
        })
    }

    pub fn generate_scid(&self) -> Result<Vec<u8>, Http3NativeError> {
        let rng = ring::rand::SystemRandom::new();
        let mut scid = vec![0u8; SCID_LEN];
        rng.fill(&mut scid)
            .map_err(|_| Http3NativeError::Config("cryptographic RNG failed".into()))?;

        if let Self::QuicLbPlaintext {
            server_id,
            config_rotation,
        } = self
        {
            apply_quic_lb_plaintext(&mut scid, *server_id, *config_rotation)?;
        }

        Ok(scid)
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

    if config_rotation > MAX_CONFIG_ROTATION {
        return Err(Http3NativeError::Config(
            "quic_lb config_rotation must be in range 0..=6 (0b111 is reserved)".into(),
        ));
    }

    // QUIC-LB plaintext format (draft): first octet = config rotation bits +
    // random low bits; server ID starts at octet 2 (index 1).
    let random_low_bits = scid[0] & RANDOM_LOW_BITS_MASK;
    scid[0] = ((config_rotation << 5) & CONFIG_ROTATION_MASK) | random_low_bits;
    scid[1..1 + QUIC_LB_SERVER_ID_LEN].copy_from_slice(&server_id);

    Ok(())
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

    #[test]
    fn random_cid_encoding_generates_random_scids() {
        let encoding = CidEncoding::random();
        let a = encoding.generate_scid().expect("scid A");
        let b = encoding.generate_scid().expect("scid B");

        assert_eq!(a.len(), SCID_LEN);
        assert_eq!(b.len(), SCID_LEN);
        assert_ne!(a, b);
    }

    #[test]
    fn quic_lb_plaintext_embeds_server_id() {
        let sid = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88];
        let encoding = CidEncoding::quic_lb_plaintext(sid, 0).expect("valid config");
        let scid = encoding.generate_scid().expect("generated scid");

        assert_eq!(scid.len(), SCID_LEN);
        assert_eq!(extract_plaintext_server_id(&scid), Some(sid));
        assert_eq!(scid[0] >> 5, 0);
    }

    #[test]
    fn quic_lb_plaintext_sets_config_rotation_bits() {
        let sid = [0xaa; QUIC_LB_SERVER_ID_LEN];
        let encoding = CidEncoding::quic_lb_plaintext(sid, 6).expect("valid config");
        let scid = encoding.generate_scid().expect("generated scid");

        assert_eq!(scid[0] >> 5, 6);
        assert_eq!(extract_plaintext_server_id(&scid), Some(sid));
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
