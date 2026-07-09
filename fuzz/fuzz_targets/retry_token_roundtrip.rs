#![no_main]

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;

#[derive(Arbitrary, Debug)]
struct RetryTokenInput {
    key: [u8; 32],
    peer_is_v6: bool,
    v4_octets: [u8; 4],
    v6_octets: [u8; 16],
    port: u16,
    odcid: Vec<u8>,
    mutations: Vec<Mutation>,
}

#[derive(Arbitrary, Debug)]
enum Mutation {
    FlipBit(u8, u8),
    Truncate(u8),
    AppendByte(u8),
}

fuzz_target!(|input: RetryTokenInput| {
    let peer = if input.peer_is_v6 {
        SocketAddr::V6(SocketAddrV6::new(
            Ipv6Addr::from(input.v6_octets),
            input.port,
            0,
            0,
        ))
    } else {
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::from(input.v4_octets), input.port))
    };
    // Real callers only ever pass a quiche connection ID, capped at 20
    // bytes (`crate::cid::SCID_LEN`) — stay within that contract rather
    // than fuzzing `mint_token`'s documented-but-unenforced caller
    // obligation (see `retry_token_model.rs`'s `build_token_payload` doc
    // comment).
    let odcid: Vec<u8> = input.odcid.into_iter().take(20).collect();

    let token = http3::fuzz_exports::retry_token_mint(input.key, peer, &odcid);

    // A freshly minted token must always validate and recover the exact
    // original ODCID.
    let result = http3::fuzz_exports::retry_token_validate(input.key, &token, peer);
    assert_eq!(result, Some(odcid));

    // Apply bounded, coverage-guided mutations to the token and confirm
    // validation never panics for any byte pattern this produces — the
    // property Kani proves for `parse_token_payload` in isolation
    // (`retry_token_parse_never_panics_on_arbitrary_bytes`), exercised
    // here through the real HMAC-verifying `ConnectionMap` path instead
    // of calling the pure parser directly.
    let mut mutated = token;
    for mutation in input.mutations.into_iter().take(16) {
        match mutation {
            Mutation::FlipBit(byte_idx, bit_idx) => {
                if !mutated.is_empty() {
                    let i = byte_idx as usize % mutated.len();
                    mutated[i] ^= 1 << (bit_idx % 8);
                }
            }
            Mutation::Truncate(new_len) => {
                let n = (new_len as usize).min(mutated.len());
                mutated.truncate(n);
            }
            Mutation::AppendByte(b) => mutated.push(b),
        }
    }

    let _ = http3::fuzz_exports::retry_token_validate(input.key, &mutated, peer);
});
