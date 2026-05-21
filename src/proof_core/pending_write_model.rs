//! Pure pending-write accounting helpers.

#![deny(unsafe_code)]

use crate::proof_core::admission::{accepted_outbound_payload_units, outbound_payload_units};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PendingWriteSnapshot {
    queued_bytes: usize,
    fin_requested: bool,
}

impl PendingWriteSnapshot {
    pub(crate) fn new(queued_bytes: usize, fin_requested: bool) -> Self {
        Self {
            queued_bytes,
            fin_requested,
        }
    }

    pub(crate) fn queued_units(self) -> usize {
        outbound_payload_units(self.queued_bytes, self.fin_requested)
    }
}

pub(crate) fn released_units_for_send(
    payload_len: usize,
    send_fin: bool,
    written: usize,
    fin_accepted: bool,
) -> usize {
    accepted_outbound_payload_units(payload_len, send_fin, written, fin_accepted)
}

pub(crate) fn release_is_bounded(
    payload_len: usize,
    send_fin: bool,
    written: usize,
    fin_accepted: bool,
) -> bool {
    released_units_for_send(payload_len, send_fin, written, fin_accepted)
        <= outbound_payload_units(payload_len, send_fin)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fin_only_pending_write_has_one_unit() {
        let snapshot = PendingWriteSnapshot::new(0, true);
        assert_eq!(snapshot.queued_units(), 1);
    }

    #[test]
    fn released_units_are_bounded_by_admitted_units() {
        assert!(release_is_bounded(8, true, 3, false));
        assert!(release_is_bounded(0, true, 0, true));
    }
}
