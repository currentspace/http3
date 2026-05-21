//! Pure control-message cursor math.

#![deny(unsafe_code)]

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CmsgStep {
    pub(crate) data_offset: usize,
    pub(crate) next_offset: usize,
}

pub(crate) fn checked_cmsg_align(len: usize, align: usize) -> Option<usize> {
    if align == 0 || !align.is_power_of_two() {
        return None;
    }
    len.checked_add(align - 1).map(|len| len & !(align - 1))
}

pub(crate) fn cmsg_data_offset(hdr_size: usize, align: usize) -> Option<usize> {
    checked_cmsg_align(hdr_size, align)
}

pub(crate) fn cmsg_header_fits(control_len: usize, offset: usize, hdr_size: usize) -> bool {
    offset
        .checked_add(hdr_size)
        .is_some_and(|end| end <= control_len)
}

pub(crate) fn cmsg_step(
    control_len: usize,
    offset: usize,
    hdr_size: usize,
    cmsg_len: usize,
    align: usize,
) -> Option<CmsgStep> {
    if cmsg_len == 0 || !cmsg_header_fits(control_len, offset, hdr_size) {
        return None;
    }

    let data_relative = cmsg_data_offset(hdr_size, align)?;
    if cmsg_len < data_relative {
        return None;
    }

    let data_offset = offset.checked_add(data_relative)?;
    let aligned_len = checked_cmsg_align(cmsg_len, align)?;
    let next_offset = offset.checked_add(aligned_len)?;
    if next_offset <= offset || next_offset > control_len {
        return None;
    }

    Some(CmsgStep {
        data_offset,
        next_offset,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmsg_step_advances_and_keeps_data_in_buffer() {
        let step = cmsg_step(64, 0, 16, 20, 8).unwrap();
        assert_eq!(step.data_offset, 16);
        assert_eq!(step.next_offset, 24);
    }

    #[test]
    fn cmsg_step_rejects_short_control_message_length() {
        assert!(cmsg_step(64, 0, 16, 8, 8).is_none());
    }
}
