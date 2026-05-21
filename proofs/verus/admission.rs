use vstd::prelude::*;

verus! {

spec fn outbound_payload_units(payload_len: nat, fin: bool) -> nat {
    if payload_len == 0nat && fin {
        1nat
    } else {
        payload_len
    }
}

spec fn accepted_outbound_payload_units(
    payload_len: nat,
    fin: bool,
    written: nat,
    fin_accepted: bool,
) -> nat {
    if payload_len == 0nat {
        if fin && fin_accepted {
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
        if fin && !fin_accepted && accepted_payload == payload_len {
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

spec fn release_next(current: nat, units: nat) -> nat {
    if units >= current {
        0nat
    } else {
        (current - units) as nat
    }
}

proof fn outbound_units_are_bounded(payload_len: nat, fin: bool)
    ensures
        outbound_payload_units(payload_len, fin) >= payload_len,
        outbound_payload_units(payload_len, fin) >= if fin { 1nat } else { 0nat },
{
}

proof fn accepted_units_never_exceed_admitted_units(
    payload_len: nat,
    fin: bool,
    written: nat,
    fin_accepted: bool,
)
    ensures
        accepted_outbound_payload_units(payload_len, fin, written, fin_accepted)
            <= outbound_payload_units(payload_len, fin),
{
}

proof fn release_never_increases_queue(current: nat, units: nat)
    ensures
        release_next(current, units) <= current,
{
}

} // verus!

fn main() {}
