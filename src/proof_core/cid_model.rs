//! Pure QUIC-LB-compatible CID encoding helpers.

#![deny(unsafe_code)]

pub(crate) const QUIC_LB_SERVER_ID_LEN: usize = 8;
pub(crate) const CONFIG_ROTATION_MASK: u8 = 0b1110_0000;
pub(crate) const RANDOM_LOW_BITS_MASK: u8 = 0b0001_1111;
pub(crate) const MAX_CONFIG_ROTATION: u8 = 0b110;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CidModelError {
    InvalidScidLen,
    InvalidConfigRotation,
}

pub(crate) fn valid_config_rotation(config_rotation: u8) -> bool {
    config_rotation <= MAX_CONFIG_ROTATION
}

pub(crate) fn apply_quic_lb_plaintext(
    scid: &mut [u8],
    expected_scid_len: usize,
    server_id: [u8; QUIC_LB_SERVER_ID_LEN],
    config_rotation: u8,
) -> Result<(), CidModelError> {
    if scid.len() != expected_scid_len || expected_scid_len < 1 + QUIC_LB_SERVER_ID_LEN {
        return Err(CidModelError::InvalidScidLen);
    }

    if !valid_config_rotation(config_rotation) {
        return Err(CidModelError::InvalidConfigRotation);
    }

    let random_low_bits = scid[0] & RANDOM_LOW_BITS_MASK;
    scid[0] = ((config_rotation << 5) & CONFIG_ROTATION_MASK) | random_low_bits;
    scid[1..1 + QUIC_LB_SERVER_ID_LEN].copy_from_slice(&server_id);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plaintext_encoding_preserves_random_low_bits() {
        let mut scid = [0xff; 20];
        let sid = [0x42; QUIC_LB_SERVER_ID_LEN];
        apply_quic_lb_plaintext(&mut scid, 20, sid, 6).unwrap();

        assert_eq!(scid[0] & RANDOM_LOW_BITS_MASK, RANDOM_LOW_BITS_MASK);
        assert_eq!(scid[0] >> 5, 6);
        assert_eq!(&scid[1..9], &sid);
    }
}
