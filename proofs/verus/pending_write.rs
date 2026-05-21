use vstd::prelude::*;

verus! {

spec fn queued_units(queued_bytes: nat, fin_requested: bool) -> nat {
    if queued_bytes == 0nat && fin_requested {
        1nat
    } else {
        queued_bytes
    }
}

spec fn after_released(queued_bytes: nat, released_bytes: nat) -> nat {
    if released_bytes >= queued_bytes {
        0nat
    } else {
        (queued_bytes - released_bytes) as nat
    }
}

spec fn released_units_for_send(
    payload_len: nat,
    send_fin: bool,
    written: nat,
    fin_accepted: bool,
) -> nat {
    if payload_len == 0nat {
        if send_fin && fin_accepted {
            1nat
        } else {
            0nat
        }
    } else {
        let accepted_payload = if written <= payload_len {
            written
        } else {
            payload_len
        };
        if send_fin && !fin_accepted && accepted_payload == payload_len {
            if accepted_payload == 0nat {
                0nat
            } else {
                (accepted_payload - 1nat) as nat
            }
        } else {
            accepted_payload
        }
    }
}

proof fn released_units_are_bounded_by_queued_units(
    payload_len: nat,
    send_fin: bool,
    written: nat,
    fin_accepted: bool,
)
    ensures
        released_units_for_send(payload_len, send_fin, written, fin_accepted)
            <= queued_units(payload_len, send_fin),
{
}

proof fn release_preserves_nonnegative_queue(queued_bytes: nat, released_bytes: nat)
    ensures
        after_released(queued_bytes, released_bytes) <= queued_bytes,
{
}

} // verus!

fn main() {}
