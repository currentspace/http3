//! Native write-admission window for JS -> worker payload commands.
//!
//! This bounds bytes sitting in the Rust command channel or native pending
//! writes. It is intentionally independent of peer ACKs and QUIC delivery:
//! the window opens when quiche accepts stream bytes or when the worker drops
//! a queued write on reset/close, so JS gets protocol-aware backpressure
//! without coupling Writable callbacks to network round trips.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::reactor_metrics;

pub(crate) const OUTBOUND_ADMISSION_HIGH_WATER: usize = 4 * 1024 * 1024;
pub(crate) const OUTBOUND_ADMISSION_LOW_WATER: usize = 2 * 1024 * 1024;

#[derive(Debug)]
pub(crate) struct OutboundAdmission {
    queued: AtomicUsize,
    high_water: usize,
    low_water: usize,
    pressured: AtomicBool,
}

impl Default for OutboundAdmission {
    fn default() -> Self {
        Self::new(OUTBOUND_ADMISSION_HIGH_WATER, OUTBOUND_ADMISSION_LOW_WATER)
    }
}

impl OutboundAdmission {
    pub(crate) fn new(high_water: usize, low_water: usize) -> Self {
        let high_water = high_water.max(1);
        let low_water = low_water.min(high_water.saturating_sub(1));
        Self {
            queued: AtomicUsize::new(0),
            high_water,
            low_water,
            pressured: AtomicBool::new(false),
        }
    }

    pub(crate) fn try_admit(&self, units: usize) -> bool {
        if units == 0 {
            return true;
        }

        loop {
            let current = self.queued.load(Ordering::Acquire);
            let limit = self.high_water.max(units);
            let Some(next) = current.checked_add(units) else {
                self.pressured.store(true, Ordering::Release);
                reactor_metrics::record_outbound_admission_backpressure();
                return false;
            };

            if next > limit {
                self.pressured.store(true, Ordering::Release);
                let latest = self.queued.load(Ordering::Acquire);
                if latest.checked_add(units).is_some_and(|n| n <= limit) {
                    continue;
                }
                reactor_metrics::record_outbound_admission_backpressure();
                return false;
            }

            if self
                .queued
                .compare_exchange(current, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return true;
            }
        }
    }

    /// Release previously admitted units.
    ///
    /// Returns true exactly when callers should emit a write-ready event:
    /// at least one writer has observed pressure and the queue crossed back
    /// to the low watermark.
    pub(crate) fn release(&self, units: usize) -> bool {
        if units == 0 {
            return false;
        }

        let next = loop {
            let current = self.queued.load(Ordering::Acquire);
            let next = current.saturating_sub(units);
            if self
                .queued
                .compare_exchange(current, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                break next;
            }
        };

        let write_ready = next <= self.low_water && self.pressured.swap(false, Ordering::AcqRel);
        reactor_metrics::record_outbound_admission_release(units, write_ready);
        write_ready
    }

    #[cfg(test)]
    pub(crate) fn queued(&self) -> usize {
        self.queued.load(Ordering::Acquire)
    }
}

pub(crate) fn outbound_payload_units(payload_len: usize, fin: bool) -> usize {
    payload_len.max(usize::from(fin))
}

pub(crate) fn accepted_outbound_payload_units(
    payload_len: usize,
    fin: bool,
    written: usize,
    fin_accepted: bool,
) -> usize {
    if payload_len == 0 {
        return usize::from(fin && fin_accepted);
    }

    written.min(payload_len)
}

#[cfg(test)]
mod tests {
    use super::{OutboundAdmission, accepted_outbound_payload_units, outbound_payload_units};
    use crate::reactor_metrics;

    #[test]
    fn admits_until_high_water_and_reopens_at_low_water() {
        let _guard = reactor_metrics::test_metrics_guard();
        reactor_metrics::reset();
        let admission = OutboundAdmission::new(10, 4);
        assert!(admission.try_admit(6));
        assert!(admission.try_admit(4));
        assert!(!admission.try_admit(1));
        assert!(!admission.release(3));
        assert!(admission.release(3));
        assert_eq!(admission.queued(), 4);
        assert!(admission.try_admit(6));
        let snap = reactor_metrics::snapshot();
        assert_eq!(snap.outboundAdmissionBackpressureEventsTotal, 1);
        assert_eq!(snap.outboundAdmissionReleasedUnitsTotal, 6);
        assert_eq!(snap.outboundAdmissionWriteReadyEventsTotal, 1);
    }

    #[test]
    fn admits_single_oversized_payload_when_empty() {
        let _guard = reactor_metrics::test_metrics_guard();
        reactor_metrics::reset();
        let admission = OutboundAdmission::new(10, 4);
        assert!(admission.try_admit(16));
        assert!(!admission.try_admit(1));
        assert!(admission.release(16));
        assert_eq!(admission.queued(), 0);
    }

    #[test]
    fn fin_only_payloads_consume_a_unit() {
        assert_eq!(outbound_payload_units(0, false), 0);
        assert_eq!(outbound_payload_units(0, true), 1);
        assert_eq!(outbound_payload_units(8, true), 8);
    }

    #[test]
    fn accepted_units_release_fin_only_only_after_fin_acceptance() {
        assert_eq!(accepted_outbound_payload_units(0, true, 0, false), 0);
        assert_eq!(accepted_outbound_payload_units(0, true, 0, true), 1);
        assert_eq!(accepted_outbound_payload_units(8, true, 3, false), 3);
        assert_eq!(accepted_outbound_payload_units(8, true, 8, true), 8);
    }
}
