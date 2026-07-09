//! Pure retry-token payload encode/decode, shared by `ConnectionMap`
//! (`src/connection_map.rs`, H3 server) and `QuicConnectionMap`
//! (`src/quic_worker.rs`, raw QUIC server) — until this module existed the
//! two kept a byte-for-byte duplicate of this parsing logic, so a fix in
//! one (e.g. audit finding #4's signed clock-skew window) could silently
//! drift from the other. Neither caller's own HMAC key/verification is
//! duplicated here: this module only owns the "build/parse the payload
//! HMAC covers" half, since that's the half with attacker-controlled input
//! (`parse_token_payload` is called on bytes from an unauthenticated peer,
//! after HMAC verification has already run in the caller).
//!
//! Token wire format (unchanged from the pre-extraction implementation):
//! `[1-byte family][4 or 16-byte addr][2-byte port BE][8-byte timestamp
//! BE][1-byte odcid_len][odcid_len bytes]`.

#![deny(unsafe_code)]

use std::net::SocketAddr;

/// Build the HMAC-covered payload for a retry token: peer address, mint
/// timestamp, and the original DCID the client's Initial packet used.
pub(crate) fn build_token_payload(peer: &SocketAddr, now: u64, odcid: &[u8]) -> Vec<u8> {
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
    payload.extend_from_slice(&now.to_be_bytes());
    // Real callers only ever pass a quiche connection ID, capped at
    // `crate::cid::SCID_LEN` (20 bytes) — `odcid.len() as u8` truncating a
    // larger input would corrupt the token, but every production call site
    // upholds this bound, so it's the caller's contract, not this
    // function's problem to solve (mirrors the pre-extraction code, which
    // had the identical cast).
    payload.push(odcid.len() as u8);
    payload.extend_from_slice(odcid);
    payload
}

/// Parse and validate an HMAC-verified token payload (the caller has
/// already confirmed `payload`'s HMAC tag matches before calling this).
/// Returns the original DCID if `peer` matches the minting address and the
/// mint timestamp is within `lifetime_secs` of `now` in either direction.
///
/// Never panics or indexes out of bounds for any `payload` content or
/// length — every slice access is preceded by an explicit length check
/// (proven for arbitrary byte input in
/// `src/proofs/kani_harnesses.rs::retry_token_parse_never_panics_on_arbitrary_bytes`).
pub(crate) fn parse_token_payload(
    payload: &[u8],
    peer: &SocketAddr,
    now: u64,
    lifetime_secs: u64,
) -> Option<Vec<u8>> {
    let mut pos = 0;
    if pos >= payload.len() {
        return None;
    }
    let family = payload[pos];
    pos += 1;

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

    if payload.len() < pos + 8 {
        return None;
    }
    let timestamp = u64::from_be_bytes(payload[pos..pos + 8].try_into().ok()?);
    pos += 8;

    // Audit finding #4: the skew check must work in both directions. A
    // plain `now.saturating_sub(timestamp)` would underflow to 0 (and thus
    // always pass the `<= lifetime_secs` check) whenever `timestamp > now`
    // — i.e. any backwards clock jump on this host would make every
    // in-flight token look freshly minted forever.
    //
    // An earlier version of this check cast both sides to `i64` and took
    // `.saturating_sub(..).abs()` of the signed difference. That's wrong
    // in a second, subtler way this module's Kani proof
    // (`retry_token_parse_never_panics_on_arbitrary_bytes`) caught:
    // `timestamp` is attacker/corruption-controlled input (parsed straight
    // from the token payload, before this function's caller can trust it
    // any further than "HMAC-verified" — a compromised or buggy peer in a
    // federated deployment could mint a syntactically valid token with an
    // arbitrary 8-byte timestamp). For a `timestamp` whose `as i64` cast
    // wraps to a large negative value, `saturating_sub` can produce
    // `i64::MIN`, and `i64::MIN.abs()` panics (there is no positive `i64`
    // representation of `-i64::MIN`). `u64::abs_diff` computes the same
    // "distance between two unsigned instants" without ever going through
    // a signed intermediate, so it can't hit that panic (or the original
    // underflow-to-0 bug) for any `u64` pair.
    let skew = now.abs_diff(timestamp);
    if skew > lifetime_secs {
        return None;
    }

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4, SocketAddrV6};

    const LIFETIME_SECS: u64 = 60;

    #[test]
    fn round_trip_recovers_odcid_for_v4() {
        let peer = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(203, 0, 113, 7), 4433));
        let odcid = [0xaa, 0xbb, 0xcc, 0xdd];
        let payload = build_token_payload(&peer, 1_000, &odcid);
        assert_eq!(
            parse_token_payload(&payload, &peer, 1_010, LIFETIME_SECS),
            Some(odcid.to_vec())
        );
    }

    #[test]
    fn round_trip_recovers_odcid_for_v6() {
        let peer = SocketAddr::V6(SocketAddrV6::new(
            Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1),
            443,
            0,
            0,
        ));
        let odcid = [0x11; 20];
        let payload = build_token_payload(&peer, 5_000, &odcid);
        assert_eq!(
            parse_token_payload(&payload, &peer, 5_030, LIFETIME_SECS),
            Some(odcid.to_vec())
        );
    }

    #[test]
    fn rejects_mismatched_peer_address() {
        let peer = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(1, 2, 3, 4), 100));
        let other = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(1, 2, 3, 5), 100));
        let payload = build_token_payload(&peer, 100, &[1, 2, 3]);
        assert_eq!(parse_token_payload(&payload, &other, 100, LIFETIME_SECS), None);
    }

    #[test]
    fn rejects_mismatched_peer_port() {
        let peer = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(1, 2, 3, 4), 100));
        let other = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(1, 2, 3, 4), 101));
        let payload = build_token_payload(&peer, 100, &[1, 2, 3]);
        assert_eq!(parse_token_payload(&payload, &other, 100, LIFETIME_SECS), None);
    }

    #[test]
    fn rejects_family_mismatch_between_v4_and_v6() {
        let v4 = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(1, 2, 3, 4), 100));
        let v6 = SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::LOCALHOST, 100, 0, 0));
        let payload = build_token_payload(&v4, 100, &[1, 2, 3]);
        assert_eq!(parse_token_payload(&payload, &v6, 100, LIFETIME_SECS), None);
    }

    #[test]
    fn rejects_expired_token_forwards_in_time() {
        let peer = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(1, 2, 3, 4), 100));
        let payload = build_token_payload(&peer, 1_000, &[1, 2, 3]);
        assert_eq!(
            parse_token_payload(&payload, &peer, 1_000 + LIFETIME_SECS + 1, LIFETIME_SECS),
            None
        );
        assert!(parse_token_payload(&payload, &peer, 1_000 + LIFETIME_SECS, LIFETIME_SECS).is_some());
    }

    /// Audit finding #4's exact regression: a large *backwards* clock jump
    /// (`now` far in the past relative to `timestamp`) must be rejected,
    /// not silently accepted via `saturating_sub` underflowing to 0.
    #[test]
    fn rejects_large_backwards_clock_jump() {
        let peer = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(1, 2, 3, 4), 100));
        let payload = build_token_payload(&peer, 1_000_000, &[1, 2, 3]);
        assert_eq!(parse_token_payload(&payload, &peer, 0, LIFETIME_SECS), None);
    }

    #[test]
    fn rejects_short_payload_without_panicking() {
        let peer = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(1, 2, 3, 4), 100));
        for len in 0..20 {
            let payload = vec![4u8; len];
            assert_eq!(parse_token_payload(&payload, &peer, 0, LIFETIME_SECS), None);
        }
    }

    #[test]
    fn rejects_truncated_odcid_length_prefix_without_panicking() {
        let peer = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(1, 2, 3, 4), 100));
        let mut payload = build_token_payload(&peer, 100, &[1, 2, 3, 4, 5]);
        // Claim a much longer odcid than actually follows.
        let odcid_len_pos = payload.len() - 6;
        payload[odcid_len_pos] = 200;
        assert_eq!(parse_token_payload(&payload, &peer, 100, LIFETIME_SECS), None);
    }
}
