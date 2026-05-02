#![no_main]

use http3::unsafe_boundary::{InitializedPacketBuf, ProvidedBufferId};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut buf = InitializedPacketBuf::zeroed(data.len());
    buf.as_mut_slice().copy_from_slice(data);

    let truncate_to = data.first().copied().unwrap_or(0) as usize;
    buf.truncate(truncate_to.min(data.len()));
    let vec = buf.into_vec();
    assert!(vec.len() <= data.len());

    if data.len() >= 4 {
        let bid = u16::from_le_bytes([data[0], data[1]]);
        let mut ring_size = u16::from_le_bytes([data[2], data[3]]);
        if ring_size == 0 {
            ring_size = 1;
        }

        let validated = ProvidedBufferId::new(bid, ring_size);
        assert_eq!(validated.is_some(), bid < ring_size);
        if let Some(validated) = validated {
            assert_eq!(validated.get(), bid);
        }
    }
});
