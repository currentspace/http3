#![cfg(kani)]
#![deny(unsafe_code)]

use crate::cid::{self, SCID_LEN};
use crate::proof_core::{
    admission, cid_model, cmsg_cursor, pending_write_model, recv_buf_model::RecvBufModel,
    ring_layout, stream_tracking,
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
