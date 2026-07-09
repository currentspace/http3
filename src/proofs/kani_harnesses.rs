#![cfg(kani)]
#![deny(unsafe_code)]

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

use crate::cid::{self, SCID_LEN};
use crate::proof_core::{
    admission, buffer_pool_model, chunk_pool_model, cid_model, cmsg_cursor, pending_write_model,
    recv_buf_model::RecvBufModel, retry_token_model, ring_layout, stream_tracking,
};
use crate::unsafe_boundary::ProvidedBufferId;

#[cfg(kani_contracts)]
#[kani::proof_for_contract(admission::outbound_payload_units)]
fn outbound_payload_units_contract() {
    let payload_len: usize = kani::any();
    let fin: bool = kani::any();

    let _ = admission::outbound_payload_units(payload_len, fin);
}

#[cfg(kani_contracts)]
#[kani::proof_for_contract(admission::accepted_outbound_payload_units)]
fn accepted_outbound_payload_units_contract() {
    let payload_len: usize = kani::any();
    let fin: bool = kani::any();
    let written: usize = kani::any();
    let fin_accepted: bool = kani::any();

    let _ = admission::accepted_outbound_payload_units(payload_len, fin, written, fin_accepted);
}

#[kani::proof]
fn outbound_payload_units_are_bounded() {
    let payload_len: usize = kani::any();
    let fin: bool = kani::any();

    let units = admission::outbound_payload_units(payload_len, fin);

    assert!(units >= payload_len);
    assert!(units >= usize::from(fin));
    assert!(units == payload_len || units == 1);
}

#[kani::proof]
fn accepted_units_never_exceed_admitted_units() {
    let payload_len: usize = kani::any();
    let fin: bool = kani::any();
    let written: usize = kani::any();
    let fin_accepted: bool = kani::any();

    let admitted = admission::outbound_payload_units(payload_len, fin);
    let accepted =
        admission::accepted_outbound_payload_units(payload_len, fin, written, fin_accepted);

    assert!(accepted <= admitted);
    assert!(accepted <= payload_len.max(1));
}

#[kani::proof]
fn admission_release_never_increases_queue() {
    let current: usize = kani::any();
    let units: usize = kani::any();

    let next = admission::release_next(current, units);

    assert!(next <= current);
}

#[kani::proof]
fn recv_buf_model_caps_appends() {
    let target_len_byte: u8 = kani::any();
    let append_len_byte: u8 = kani::any();
    kani::assume(target_len_byte <= 16);
    kani::assume(append_len_byte <= 32);

    let model = RecvBufModel::new(target_len_byte as usize);
    let appended = model.append_len(append_len_byte as usize);
    let next = model.after_append(appended);

    assert!(appended <= append_len_byte as usize);
    assert!(next.initialized_len() <= next.target_len());
    assert_eq!(next.remaining(), next.target_len() - next.initialized_len());
}

#[kani::proof]
fn cmsg_cursor_step_advances_or_rejects() {
    let control_len_byte: u8 = kani::any();
    let offset_byte: u8 = kani::any();
    let cmsg_len_byte: u8 = kani::any();
    let linux_align: bool = kani::any();
    kani::assume(control_len_byte <= 64);
    kani::assume(offset_byte <= 64);
    kani::assume(cmsg_len_byte <= 64);

    let control_len = control_len_byte as usize;
    let offset = offset_byte as usize;
    let hdr_size = 16usize;
    let cmsg_len = cmsg_len_byte as usize;
    let align = if linux_align { 8 } else { 4 };

    if let Some(step) = cmsg_cursor::cmsg_step(control_len, offset, hdr_size, cmsg_len, align) {
        assert!(step.data_offset >= offset);
        assert!(step.next_offset > offset);
        assert!(step.next_offset <= control_len);
    }
}

#[kani::proof]
fn provided_buffer_id_constructor_matches_range() {
    let bid: u16 = kani::any();
    let ring_size: u16 = kani::any();

    let validated = ProvidedBufferId::new(bid, ring_size);

    assert_eq!(validated.is_some(), bid < ring_size);
    if let Some(validated) = validated {
        assert_eq!(validated.get(), bid);
    }
}

#[kani::proof]
fn provided_buffer_id_models_iouring_offset_bounds() {
    const MODEL_RX_RING_SIZE: u16 = 512;
    const MODEL_RX_BUF_SIZE: usize = 65_535 + 256;

    let bid: u16 = kani::any();
    let Some(validated) = ProvidedBufferId::new(bid, MODEL_RX_RING_SIZE) else {
        return;
    };

    let offset = ring_layout::provided_buffer_offset(validated.get(), MODEL_RX_BUF_SIZE)
        .expect("model offset should not overflow");
    let layout_size =
        ring_layout::provided_buffer_layout_size(MODEL_RX_RING_SIZE, MODEL_RX_BUF_SIZE)
            .expect("model layout should not overflow");

    assert!(offset < layout_size);
    assert!(offset + MODEL_RX_BUF_SIZE <= layout_size);
    assert!(ring_layout::provided_buffer_range_in_layout(
        validated.get(),
        MODEL_RX_RING_SIZE,
        MODEL_RX_BUF_SIZE
    ));
}

#[kani::proof]
fn quic_lb_plaintext_preserves_low_bits_and_embeds_server_id() {
    let mut scid: [u8; SCID_LEN] = kani::any();
    let server_id: [u8; cid_model::QUIC_LB_SERVER_ID_LEN] = kani::any();
    let config_rotation: u8 = kani::any();
    kani::assume(config_rotation <= cid_model::MAX_CONFIG_ROTATION);

    let original_low_bits = scid[0] & cid_model::RANDOM_LOW_BITS_MASK;
    let result =
        cid_model::apply_quic_lb_plaintext(&mut scid, SCID_LEN, server_id, config_rotation);

    assert!(result.is_ok());
    assert_eq!(scid[0] & cid_model::RANDOM_LOW_BITS_MASK, original_low_bits);
    assert_eq!(scid[0] >> 5, config_rotation);
    assert_eq!(&scid[1..1 + cid::QUIC_LB_SERVER_ID_LEN], &server_id);
}

#[kani::proof]
fn pending_write_partial_accept_accounting() {
    let payload_len_byte: u8 = kani::any();
    let first_accept_byte: u8 = kani::any();
    let second_accept_byte: u8 = kani::any();
    let first_fin_accepted: bool = kani::any();
    let second_fin_accepted: bool = kani::any();
    let fin: bool = kani::any();
    kani::assume(payload_len_byte <= 16);
    kani::assume(first_accept_byte <= 16);
    kani::assume(second_accept_byte <= 16);

    let payload_len = payload_len_byte as usize;
    let admitted = pending_write_model::PendingWriteSnapshot::new(payload_len, fin).queued_units();

    let first_written = (first_accept_byte as usize).min(payload_len);
    let first_fin_done = fin && first_written == payload_len && first_fin_accepted;
    let first_released = pending_write_model::released_units_for_send(
        payload_len,
        fin,
        first_written,
        first_fin_done,
    );

    let remaining_payload = payload_len - first_written;
    let remaining_fin = fin && !first_fin_done;
    let second_written = (second_accept_byte as usize).min(remaining_payload);
    let second_fin_done =
        remaining_fin && second_written == remaining_payload && second_fin_accepted;
    let second_released = pending_write_model::released_units_for_send(
        remaining_payload,
        remaining_fin,
        second_written,
        second_fin_done,
    );

    assert!(first_released <= admitted);
    assert!(first_released + second_released <= admitted);
}

#[kani::proof]
fn cmsg_cursor_bounded_walk_stays_in_buffer() {
    let control_len_byte: u8 = kani::any();
    let cmsg_lens: [u8; 4] = kani::any();
    let linux_align: bool = kani::any();
    kani::assume(control_len_byte <= 64);

    let control_len = control_len_byte as usize;
    let hdr_size = 16usize;
    let align = if linux_align { 8 } else { 4 };
    let mut offset = 0usize;
    let mut stopped = false;

    if !stopped {
        match checked_cmsg_step_next(control_len, offset, hdr_size, cmsg_lens[0] as usize, align) {
            Some(next) => offset = next,
            None => stopped = true,
        }
    }
    if !stopped {
        match checked_cmsg_step_next(control_len, offset, hdr_size, cmsg_lens[1] as usize, align) {
            Some(next) => offset = next,
            None => stopped = true,
        }
    }
    if !stopped {
        match checked_cmsg_step_next(control_len, offset, hdr_size, cmsg_lens[2] as usize, align) {
            Some(next) => offset = next,
            None => stopped = true,
        }
    }
    if !stopped {
        if let Some(next) =
            checked_cmsg_step_next(control_len, offset, hdr_size, cmsg_lens[3] as usize, align)
        {
            offset = next;
        }
    }

    assert!(offset <= control_len);
}

#[kani::proof]
fn stream_tracking_closed_cleanup_drops_target_state() {
    let ops: [u8; 8] = kani::any();
    let target_byte: u8 = kani::any();
    let target = stream_tracking::model_slot(target_byte as usize);
    let mut model = stream_tracking::StreamTrackingModel::new();

    for op in ops {
        apply_stream_tracking_model_op(&mut model, op);
    }

    let slot_0_before = model.flags(0);
    let slot_1_before = model.flags(1);
    let slot_2_before = model.flags(2);
    let slot_3_before = model.flags(3);

    model.cleanup_closed_stream(target);

    assert!(!model.has_any_state(target));
    assert_eq!(model.blocked_queue_entries(target), 0);

    if target != 0 {
        assert_eq!(model.flags(0), slot_0_before);
    }
    if target != 1 {
        assert_eq!(model.flags(1), slot_1_before);
    }
    if target != 2 {
        assert_eq!(model.flags(2), slot_2_before);
    }
    if target != 3 {
        assert_eq!(model.flags(3), slot_3_before);
    }

    let after_cleanup = model;
    model.cleanup_closed_stream(target);
    assert_eq!(model, after_cleanup);
}

#[kani::proof]
fn stream_tracking_open_cleanup_is_noop() {
    let ops: [u8; 8] = kani::any();
    let target_byte: u8 = kani::any();
    let target = stream_tracking::model_slot(target_byte as usize);
    let mut model = stream_tracking::StreamTrackingModel::new();

    for op in ops {
        apply_stream_tracking_model_op(&mut model, op);
    }

    let before = model;
    model.cleanup_if_closed(target, false);

    assert_eq!(model, before);
}

#[kani::proof]
fn chunk_pool_bin_for_cap_returns_largest_class_leq_cap() {
    let cap: usize = kani::any();

    match chunk_pool_model::bin_for_cap(cap) {
        Some(idx) => {
            assert!(chunk_pool_model::CHUNK_CLASSES[idx] <= cap);
            if idx + 1 < chunk_pool_model::NUM_BINS {
                assert!(chunk_pool_model::CHUNK_CLASSES[idx + 1] > cap);
            }
        }
        None => {
            // No class fits: either cap is below the smallest class, or
            // above the largest.
            assert!(
                cap < chunk_pool_model::CHUNK_CLASSES[0]
                    || cap > *chunk_pool_model::CHUNK_CLASSES.last().unwrap()
            );
        }
    }
}

/// Guards audit finding #28 directly: any buffer allocated at
/// `CHUNK_CLASSES[bin_for(len)]` must be accepted back into that exact
/// same bin by `bin_for_cap`, not silently discarded.
#[kani::proof]
fn chunk_pool_bin_for_cap_accepts_every_bin_for_allocation() {
    let len: usize = kani::any();

    if let Some(idx) = chunk_pool_model::bin_for(len) {
        assert_eq!(
            chunk_pool_model::bin_for_cap(chunk_pool_model::CHUNK_CLASSES[idx]),
            Some(idx)
        );
    }
}

#[kani::proof]
fn buffer_pool_class_for_capacity_returns_largest_class_leq_cap() {
    let capacity: usize = kani::any();

    match buffer_pool_model::class_for_capacity(capacity) {
        Some(idx) => {
            assert!(buffer_pool_model::RIGHT_SIZED_CLASSES[idx] <= capacity);
            if idx + 1 < buffer_pool_model::RIGHT_SIZED_CLASSES.len() {
                assert!(buffer_pool_model::RIGHT_SIZED_CLASSES[idx + 1] > capacity);
            }
        }
        None => {
            // No class fits: either capacity is below the smallest class,
            // or above the largest.
            assert!(
                capacity < buffer_pool_model::RIGHT_SIZED_CLASSES[0]
                    || capacity > *buffer_pool_model::RIGHT_SIZED_CLASSES.last().unwrap()
            );
        }
    }
}

/// Same shape as `chunk_pool_bin_for_cap_accepts_every_bin_for_allocation`:
/// a buffer allocated at the class `class_for_request` picked must be
/// accepted back into that same class by `class_for_capacity`.
#[kani::proof]
fn buffer_pool_class_for_capacity_accepts_every_class_for_request_allocation() {
    let len: usize = kani::any();

    if let Some(idx) = buffer_pool_model::class_for_request(len) {
        assert_eq!(
            buffer_pool_model::class_for_capacity(buffer_pool_model::RIGHT_SIZED_CLASSES[idx]),
            Some(idx)
        );
    }
}

/// Round-trip correctness: minting then immediately parsing a token for
/// the same peer/time must recover the exact original ODCID, for both
/// address families.
///
/// Split into two fixed-length harnesses (empty and `SCID_LEN`, the real
/// quiche connection-ID cap every production call site upholds, and so
/// the two ends of the range any real ODCID falls in) rather than one
/// harness with a symbolic *length*, and comparing the recovered ODCID
/// with an explicit bounded byte-index loop rather than `Vec<u8>`'s
/// `PartialEq`/`assert_eq!`: both `Vec::extend_from_slice` (inside
/// `build_token_payload`) and `Vec`'s own equality operator go through a
/// length-generic comparison path whose loop bound CBMC can't collapse to
/// a compile-time constant even when every length involved is one at
/// runtime, which timed out unwinding a `memcmp` loop past 2000+
/// iterations instead of converging on the handful of bytes actually
/// compared. A fixed-size array argument plus an explicit `0..LEN` loop
/// keeps every bound a literal constant instead.
#[kani::proof]
fn retry_token_round_trip_recovers_empty_odcid() {
    const LIFETIME_SECS: u64 = 60;

    let is_v4: bool = kani::any();
    let port: u16 = kani::any();
    let now: u64 = kani::any();
    let odcid: [u8; 0] = [];

    let peer = if is_v4 {
        let ip: [u8; 4] = kani::any();
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::from(ip), port))
    } else {
        let ip: [u8; 16] = kani::any();
        SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::from(ip), port, 0, 0))
    };

    let payload = retry_token_model::build_token_payload(&peer, now, &odcid);
    let result = retry_token_model::parse_token_payload(&payload, &peer, now, LIFETIME_SECS);

    let Some(actual) = result else {
        panic!("expected the round-tripped token to parse successfully");
    };
    assert!(actual.is_empty());
}

#[kani::proof]
fn retry_token_round_trip_recovers_full_length_odcid() {
    const LIFETIME_SECS: u64 = 60;

    let is_v4: bool = kani::any();
    let port: u16 = kani::any();
    let now: u64 = kani::any();
    let odcid: [u8; SCID_LEN] = kani::any();

    let peer = if is_v4 {
        let ip: [u8; 4] = kani::any();
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::from(ip), port))
    } else {
        let ip: [u8; 16] = kani::any();
        SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::from(ip), port, 0, 0))
    };

    let payload = retry_token_model::build_token_payload(&peer, now, &odcid);
    let result = retry_token_model::parse_token_payload(&payload, &peer, now, LIFETIME_SECS);

    let Some(actual) = result else {
        panic!("expected the round-tripped token to parse successfully");
    };
    assert!(actual.len() == SCID_LEN);
    for i in 0..SCID_LEN {
        assert!(actual[i] == odcid[i]);
    }
}

/// The safety property that matters most for this parser: it is called on
/// bytes from an unauthenticated peer (after HMAC verification, but the
/// HMAC only proves the bytes weren't tampered with post-mint — it says
/// nothing about a hostile *implementation* of the minting side, e.g. a
/// compromised or buggy peer of a federated deployment). No length or
/// content of `payload` may cause a panic or out-of-bounds index.
#[kani::proof]
fn retry_token_parse_never_panics_on_arbitrary_bytes() {
    const MAX_PAYLOAD: usize = 1 + 16 + 2 + 8 + 1 + SCID_LEN;

    let payload_bytes: [u8; MAX_PAYLOAD] = kani::any();
    let used_len_byte: u8 = kani::any();
    kani::assume((used_len_byte as usize) <= MAX_PAYLOAD);
    let payload = &payload_bytes[..used_len_byte as usize];

    let is_v4: bool = kani::any();
    let port: u16 = kani::any();
    let now: u64 = kani::any();
    let timestamp_hint: u64 = kani::any();

    let peer = if is_v4 {
        let ip: [u8; 4] = kani::any();
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::from(ip), port))
    } else {
        let ip: [u8; 16] = kani::any();
        SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::from(ip), port, 0, 0))
    };

    // `now`/`timestamp_hint` are otherwise unconstrained kani::any() values
    // (timestamp_hint is never read — its only purpose is nudging kani to
    // explore the full u64 range for `now` against arbitrary encoded
    // timestamps inside `payload_bytes` itself, which already cover that).
    let _ = timestamp_hint;
    let _ = retry_token_model::parse_token_payload(payload, &peer, now, 60);
}

/// Audit finding #4's exact regression, proven rather than just
/// example-tested: a backwards clock jump strictly greater than
/// `lifetime_secs` must never be accepted, over the *entire* `u64` domain
/// of `now`/`timestamp` (no escape hatch needed once the skew check uses
/// `u64::abs_diff` instead of an `i64`-cast-and-`.abs()` — see
/// `retry_token_model.rs`'s doc comment on `parse_token_payload`, and
/// `retry_token_parse_never_panics_on_arbitrary_bytes` for the panic that
/// abs_diff avoids).
#[kani::proof]
fn retry_token_rejects_large_backwards_clock_jump() {
    const LIFETIME_SECS: u64 = 60;

    let port: u16 = kani::any();
    let ip: [u8; 4] = kani::any();
    let timestamp: u64 = kani::any();
    let now: u64 = kani::any();
    // A "large backwards jump" here means timestamp (the token's claimed
    // mint time) is far ahead of now (the validator's clock) — i.e. the
    // validator's clock has jumped backwards relative to when the token
    // claims to have been minted.
    kani::assume(timestamp > now);
    kani::assume(timestamp - now > LIFETIME_SECS);

    let peer = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::from(ip), port));
    let odcid = [0u8; 4];
    let payload = retry_token_model::build_token_payload(&peer, timestamp, &odcid);

    assert_eq!(
        retry_token_model::parse_token_payload(&payload, &peer, now, LIFETIME_SECS),
        None
    );
}

fn checked_cmsg_step_next(
    control_len: usize,
    offset: usize,
    hdr_size: usize,
    cmsg_len: usize,
    align: usize,
) -> Option<usize> {
    if let Some(step) = cmsg_cursor::cmsg_step(control_len, offset, hdr_size, cmsg_len, align) {
        assert!(step.data_offset >= offset);
        assert!(step.next_offset > offset);
        assert!(step.next_offset <= control_len);
        Some(step.next_offset)
    } else {
        None
    }
}

fn apply_stream_tracking_model_op(model: &mut stream_tracking::StreamTrackingModel, op: u8) {
    let stream_slot = stream_tracking::model_slot((op & 0x03) as usize);
    match (op >> 2) & 0x07 {
        0 => model.track_blocked(stream_slot),
        1 => model.inject_blocked_queue_entry(stream_slot),
        2 => model.track_pending_body(stream_slot),
        3 => model.track_trailer_fin(stream_slot),
        4 => model.track_pending_write(stream_slot),
        5 => model.track_pending_response(stream_slot),
        6 => model.cleanup_h3_closed_stream(stream_slot),
        _ => model.cleanup_worker_closed_stream(stream_slot),
    }
}
