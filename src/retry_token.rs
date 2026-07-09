//! Shared HMAC-SHA256 primitive for server-side retry-token mint/validate
//! and deterministic per-connection SCID derivation, built on the
//! already-vendored, already-wasm-compatible `boring` crate instead of
//! `ring` (which is `os-runtime`-gated and must stay that way — see
//! `docs/WASM_CLIENT_PLAN.md` A1 task 1 and `cid.rs`'s
//! `generate_scid_from_bytes` for the established precedent this module
//! follows for the server side).
//!
//! `boring::hash::hmac_sha256` already wraps BoringSSL's `HMAC()` FFI call
//! directly — no hand-rolled `H((key XOR opad) || H((key XOR ipad) ||
//! message))` construction is needed here, since `boring` exposes a
//! correct, tested primitive one level up. This module only adds:
//! constant-time tag comparison (`boring::memcmp::eq`, matching `ring`'s
//! own implicit constant-time `hmac::verify`) and a deterministic
//! keyed-PRF SCID generator for the sans-IO / direct-call server surface
//! (native's `ConnectionMap`/`QuicConnectionMap` keep using
//! `ring::rand::SystemRandom`-backed `CidEncoding::generate_scid`
//! unchanged for their own packet-routing path — see the doc comments on
//! `H3ServerHandler::process_inbound_packet` /
//! `QuicServerHandler::process_inbound_packet` for why the two entropy
//! sources are deliberately kept separate rather than unified).
//!
//! Always compiled: must not pull in `os-runtime` or `node-api` (`cargo
//! check --no-default-features --features wasm-abi` is the proof, same as
//! every other item re-exported via `wasm_exports`).

#![deny(unsafe_code)]

use crate::cid::{CidEncoding, SCID_LEN};
use crate::error::Http3NativeError;

/// Computes HMAC-SHA256(key, data). Only fails on a BoringSSL-internal
/// allocation/FFI error, which is not a case any caller here can usefully
/// recover from (mirrors `cid.rs`'s treatment of `ring::rand::fill`
/// failure as an `expect`-worthy condition, not a `Result` the caller
/// threads through).
#[allow(clippy::expect_used)]
pub fn hmac_sha256(key: &[u8; 32], data: &[u8]) -> [u8; 32] {
    boring::hash::hmac_sha256(key, data).expect("boring HMAC-SHA256 should not fail")
}

/// Constant-time-verifies that `tag` is the HMAC-SHA256(key, data) tag,
/// using `boring::memcmp::eq` (wraps `CRYPTO_memcmp`) rather than a plain
/// `==`, so retry-token validation stays resistant to timing attacks —
/// the same property `ring::hmac::verify` provided implicitly.
pub fn hmac_sha256_verify(key: &[u8; 32], data: &[u8], tag: &[u8]) -> bool {
    if tag.len() != 32 {
        return false;
    }
    let expected = hmac_sha256(key, data);
    boring::memcmp::eq(&expected, tag)
}

/// Deterministic keyed-PRF Source Connection ID generator for the sans-IO
/// / direct-call server surface (no `ring`). Domain-separated from
/// retry-token HMAC by a fixed message prefix (`b"scid:"`) plus a
/// monotonic counter, so reusing the connection map's own 32-byte
/// retry-token key as the PRF key here is sound under the standard HMAC
/// PRF assumption: an attacker who cannot forge retry tokens (i.e.
/// doesn't know the key) also cannot predict future SCIDs, and vice
/// versa — the two uses never share input bytes.
///
/// The key's own entropy (JS `crypto.getRandomValues` on the wasm side,
/// `ring::rand::SystemRandom` on native) is what makes the output
/// sequence unpredictable to a third party; HMAC-SHA256 just expands one
/// secret into arbitrarily many unpredictable-looking, non-colliding
/// (with overwhelming probability) outputs.
pub struct DeterministicScidSource {
    key: [u8; 32],
    counter: u64,
}

impl DeterministicScidSource {
    pub fn new(key: [u8; 32]) -> Self {
        Self { key, counter: 0 }
    }

    /// Produce the next `SCID_LEN`-byte pseudorandom value and run it
    /// through `cid_encoding`'s QUIC-LB transform (if any) — the same
    /// contract as `CidEncoding::generate_scid`/`generate_scid_from_bytes`.
    pub fn next_scid(&mut self, cid_encoding: &CidEncoding) -> Result<Vec<u8>, Http3NativeError> {
        // Wrapping add: a `u64` counter exhausting is not a realistic
        // concern (2^64 connections), but wrapping is preferable to a
        // debug-mode panic on overflow for a value that only needs to be
        // "very unlikely to repeat within the key's lifetime," not
        // monotonically ever-increasing.
        self.counter = self.counter.wrapping_add(1);
        let mut msg = Vec::with_capacity(5 + 8);
        msg.extend_from_slice(b"scid:");
        msg.extend_from_slice(&self.counter.to_be_bytes());
        let digest = hmac_sha256(&self.key, &msg);
        cid_encoding.generate_scid_from_bytes(&digest[..SCID_LEN])
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// RFC 4231 Test Case 1: Key = 20×0x0b, Data = "Hi There". Cross-checked
    /// locally against both `openssl dgst -sha256 -hmac` and Python's
    /// `hmac`/`hashlib` before writing this assertion (not typed from
    /// memory alone).
    #[test]
    fn hmac_sha256_matches_rfc4231_test_case_1() {
        // RFC 4231 case 1's key is only 20 bytes; boring's `hmac_sha256`
        // takes `&[u8]` directly (no fixed-size requirement) — call it
        // directly here rather than through this module's own 32-byte-key
        // `hmac_sha256` wrapper, since this test's purpose is verifying
        // the underlying `boring` primitive itself against the RFC
        // vector (the wrapper's own key size is covered by the
        // `..._with_32_byte_key` test below).
        let key = [0x0bu8; 20];
        let tag = boring::hash::hmac_sha256(&key, b"Hi There").expect("hmac");
        assert_eq!(
            hex_encode(&tag),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    /// RFC 4231 Test Case 2 ("Jefe"/"what do ya want for nothing?" — the
    /// most widely reproduced HMAC-SHA256 test vector). Also cross-checked
    /// locally against `openssl dgst -sha256 -hmac Jefe` and Python's
    /// `hmac.new(b"Jefe", ..., hashlib.sha256)` before writing this
    /// assertion.
    #[test]
    fn hmac_sha256_matches_rfc4231_test_case_2() {
        let tag = boring::hash::hmac_sha256(b"Jefe", b"what do ya want for nothing?")
            .expect("hmac");
        assert_eq!(
            hex_encode(&tag),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    /// Exercises this module's own 32-byte-key wrapper (the shape actually
    /// used by `ConnectionMap`/`QuicConnectionMap`) against a
    /// python/openssl-cross-checked vector: key = 32×0x0b, data = "Hi There".
    #[test]
    fn hmac_sha256_wrapper_matches_known_vector_with_32_byte_key() {
        let key = [0x0bu8; 32];
        let tag = hmac_sha256(&key, b"Hi There");
        // key = 32×0x0b, data = "Hi There". Cross-checked locally against
        // both `python3 -c "import hmac,hashlib; print(hmac.new(bytes([0x0b]*32),
        // b'Hi There', hashlib.sha256).hexdigest())"` and
        // `openssl dgst -sha256 -mac HMAC -macopt hexkey:0b0b...0b` (64 hex
        // chars) before writing this assertion — both tools agree.
        assert_eq!(
            hex_encode(&tag),
            "198a607eb44bfbc69903a0f1cf2bbdc5ba0aa3f3d9ae3c1c7a3b1696a0b68cf7"
        );
    }

    #[test]
    fn verify_accepts_correct_tag_and_rejects_tampered_tag() {
        let key = [0x42u8; 32];
        let data = b"retry-token-payload";
        let tag = hmac_sha256(&key, data);
        assert!(hmac_sha256_verify(&key, data, &tag));

        let mut bad_tag = tag;
        bad_tag[0] ^= 0xff;
        assert!(!hmac_sha256_verify(&key, data, &bad_tag));

        assert!(!hmac_sha256_verify(&key, data, &tag[..31]));
    }

    #[test]
    fn verify_rejects_wrong_key() {
        let key_a = [0x01u8; 32];
        let key_b = [0x02u8; 32];
        let data = b"payload";
        let tag = hmac_sha256(&key_a, data);
        assert!(!hmac_sha256_verify(&key_b, data, &tag));
    }

    #[test]
    fn deterministic_scid_source_never_repeats_and_has_correct_length() {
        let mut source = DeterministicScidSource::new([0x77u8; 32]);
        let encoding = CidEncoding::random();
        let mut seen = std::collections::HashSet::new();
        for _ in 0..500 {
            let scid = source.next_scid(&encoding).expect("scid");
            assert_eq!(scid.len(), SCID_LEN);
            assert!(seen.insert(scid), "scid repeated within 500 draws");
        }
    }

    #[test]
    fn deterministic_scid_source_is_reproducible_from_the_same_key() {
        // Same key + same call sequence -> identical SCIDs. This is a
        // deliberate PRF property (useful for tests), not a security
        // requirement — the key itself is the caller's secret.
        let mut a = DeterministicScidSource::new([0x11u8; 32]);
        let mut b = DeterministicScidSource::new([0x11u8; 32]);
        let encoding = CidEncoding::random();
        for _ in 0..10 {
            assert_eq!(
                a.next_scid(&encoding).unwrap(),
                b.next_scid(&encoding).unwrap()
            );
        }
    }

    #[test]
    fn deterministic_scid_source_applies_quic_lb_encoding() {
        let server_id = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88];
        let encoding = CidEncoding::quic_lb_plaintext(server_id, 2).expect("valid encoding");
        let mut source = DeterministicScidSource::new([0x99u8; 32]);
        let scid = source.next_scid(&encoding).expect("scid");
        assert_eq!(scid.len(), SCID_LEN);
        assert_eq!(
            crate::cid::extract_plaintext_server_id(&scid),
            Some(server_id)
        );
        assert_eq!(scid[0] >> 5, 2);
    }

    fn hex_encode(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}
