#![no_main]

use arbitrary::Arbitrary;
use http3::unsafe_boundary::{InitializedPacketBuf, ProvidedBufferId, QuicheRecvBuf};
use libfuzzer_sys::fuzz_target;

#[derive(Arbitrary, Debug)]
struct UnsafeWrapperInput {
    packet_len: u16,
    truncate_to: u16,
    packet_writes: Vec<ByteWrite>,
    bid: u16,
    ring_size: u16,
    recv_target_len: u16,
    recv_chunks: Vec<Vec<u8>>,
}

#[derive(Arbitrary, Debug)]
struct ByteWrite {
    offset: u16,
    value: u8,
}

fuzz_target!(|input: UnsafeWrapperInput| {
    let packet_len = usize::from(input.packet_len % 4097);
    let mut packet = InitializedPacketBuf::zeroed(packet_len);
    for write in input.packet_writes.into_iter().take(256) {
        if packet_len == 0 {
            break;
        }
        packet.as_mut_slice()[usize::from(write.offset) % packet_len] = write.value;
    }
    packet.truncate(usize::from(input.truncate_to).min(packet_len));
    let packet = packet.into_vec();
    assert!(packet.len() <= packet_len);

    let ring_size = input.ring_size.max(1);
    let validated = ProvidedBufferId::new(input.bid, ring_size);
    assert_eq!(validated.is_some(), input.bid < ring_size);
    if let Some(validated) = validated {
        assert_eq!(validated.get(), input.bid);
    }

    let target_len = usize::from(input.recv_target_len % 4097);
    let mut recv = QuicheRecvBuf::with_capacity(target_len);
    let mut expected = Vec::new();
    for chunk in input.recv_chunks.into_iter().take(64) {
        let before = expected.len();
        let written = recv.append_initialized(&chunk);
        expected.extend_from_slice(&chunk[..written]);
        assert_eq!(expected.len().saturating_sub(before), written);
        assert!(expected.len() <= target_len);
    }

    assert_eq!(recv.initialized_len(), expected.len());
    assert!(recv.remaining() <= target_len);
    assert_eq!(recv.into_initialized_vec(), expected);
});
