//! Minimal, dependency-free hex/base64 codecs.
//!
//! The crate intentionally depends on nothing beyond `http3` + `serde_json`
//! (see `docs/WASM_CLIENT_PLAN.md` A3's Cargo.toml block), so the two small
//! encodings the ABI's options contract needs (`scidHex`, `sessionTicket`
//! base64) are hand-rolled here rather than pulling in `hex`/`base64`
//! crates for ~30 lines of logic each.

/// Decode a lowercase-hex string into bytes. Strict: rejects uppercase,
/// odd length, and any non-hex-digit byte (the ABI contract requires
/// exactly 40 lowercase hex chars for `scidHex`; being strict here turns a
/// caller mistake into an immediate, clear failure instead of silently
/// accepting input the TS layer would never actually produce).
pub(crate) fn decode_hex(s: &str) -> Option<Vec<u8>> {
    let bytes = s.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut i = 0;
    while i < bytes.len() {
        let hi = hex_val(bytes[i])?;
        let lo = hex_val(bytes[i + 1])?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Some(out)
}

const fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    }
}

const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Decode standard (RFC 4648 §4) base64, with or without `=` padding.
/// Used for the `sessionTicket` option field.
pub(crate) fn decode_base64(s: &str) -> Option<Vec<u8>> {
    let s = s.trim_end_matches('=');
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4 + 3);
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    for &b in bytes {
        let val = base64_val(b)?;
        buf = (buf << 6) | u32::from(val);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((buf >> bits) & 0xFF).unwrap_or(0));
        }
    }
    Some(out)
}

fn base64_val(b: u8) -> Option<u8> {
    BASE64_ALPHABET
        .iter()
        .position(|&c| c == b)
        .and_then(|p| u8::try_from(p).ok())
}

#[cfg(test)]
mod tests {
    use super::{decode_base64, decode_hex};

    #[test]
    fn hex_round_trips_20_bytes() {
        let scid: Vec<u8> = (0u8..20).collect();
        let hex: String = scid.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex.len(), 40);
        assert_eq!(decode_hex(&hex).unwrap(), scid);
    }

    #[test]
    fn hex_rejects_uppercase_and_odd_length() {
        assert_eq!(decode_hex("ABCD"), None);
        assert_eq!(decode_hex("abc"), None);
        assert_eq!(decode_hex("zz"), None);
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(decode_base64("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(decode_base64("aGVsbG8").unwrap(), b"hello");
        assert_eq!(decode_base64("").unwrap(), b"");
        assert_eq!(decode_base64("Zg==").unwrap(), b"f");
        assert_eq!(decode_base64("Zm8=").unwrap(), b"fo");
        assert_eq!(decode_base64("Zm9v").unwrap(), b"foo");
        assert_eq!(decode_base64("Zm9vYg==").unwrap(), b"foob");
        assert_eq!(decode_base64("Zm9vYmE=").unwrap(), b"fooba");
        assert_eq!(decode_base64("Zm9vYmFy").unwrap(), b"foobar");
    }

    #[test]
    fn base64_rejects_invalid_chars() {
        assert_eq!(decode_base64("not valid base64!!"), None);
    }
}
