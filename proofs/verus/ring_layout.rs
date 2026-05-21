use vstd::prelude::*;

verus! {

spec fn provided_buffer_id_is_valid(bid: nat, ring_size: nat) -> bool {
    bid < ring_size
}

spec fn provided_buffer_offset(bid: nat, buf_size: nat) -> nat {
    bid * buf_size
}

spec fn provided_buffer_layout_size(ring_size: nat, buf_size: nat) -> nat {
    ring_size * buf_size
}

proof fn valid_provided_buffer_range_stays_inside_layout(
    bid: nat,
    ring_size: nat,
    buf_size: nat,
)
    requires
        provided_buffer_id_is_valid(bid, ring_size),
        buf_size > 0nat,
        ensures
            provided_buffer_offset(bid, buf_size)
                < provided_buffer_layout_size(ring_size, buf_size),
            provided_buffer_offset(bid, buf_size) + buf_size
                <= provided_buffer_layout_size(ring_size, buf_size),
    {
        assert(bid < ring_size);
        assert(buf_size > 0nat);
        assert(bid + 1nat <= ring_size);
        assert(bid * buf_size < ring_size * buf_size) by(nonlinear_arith)
            requires
                bid < ring_size,
                buf_size > 0nat;
        assert(bid * buf_size + buf_size == (bid + 1nat) * buf_size) by(nonlinear_arith);
        assert((bid + 1nat) * buf_size <= ring_size * buf_size) by(nonlinear_arith)
            requires
                bid + 1nat <= ring_size,
                buf_size > 0nat;
    }

} // verus!

fn main() {}
