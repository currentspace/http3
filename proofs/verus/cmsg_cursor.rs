use vstd::prelude::*;

verus! {

spec fn align_up(len: nat, align: nat) -> nat
    recommends
        align == 4nat || align == 8nat,
{
    (((len + align - 1nat) as nat) / align) * align
}

proof fn cmsg_step_advances_or_is_rejected(
    control_len: nat,
    offset: nat,
    hdr_size: nat,
    cmsg_len: nat,
    align: nat,
)
    requires
        align == 4nat || align == 8nat,
        hdr_size > 0nat,
        offset + hdr_size <= control_len,
        cmsg_len >= align_up(hdr_size, align),
        align_up(cmsg_len, align) > 0nat,
        offset + align_up(cmsg_len, align) <= control_len,
    ensures
        offset + align_up(hdr_size, align) >= offset,
        offset + align_up(cmsg_len, align) > offset,
        offset + align_up(cmsg_len, align) <= control_len,
{
}

} // verus!

fn main() {}
