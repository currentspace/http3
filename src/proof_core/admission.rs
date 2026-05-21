//! Pure outbound admission accounting.

#![deny(unsafe_code)]

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AdmissionWatermarks {
    pub(crate) high: usize,
    pub(crate) low: usize,
}

impl AdmissionWatermarks {
    pub(crate) fn new(high: usize, low: usize) -> Self {
        let high = high.max(1);
        let low = low.min(high.saturating_sub(1));
        Self { high, low }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AdmitOutcome {
    Admitted { next_queued: usize },
    Backpressured,
}

#[cfg_attr(kani, kani::ensures(|result: &usize| *result >= payload_len))]
#[cfg_attr(kani, kani::ensures(|result: &usize| *result >= usize::from(fin)))]
pub(crate) fn outbound_payload_units(payload_len: usize, fin: bool) -> usize {
    payload_len.max(usize::from(fin))
}

#[cfg_attr(kani, kani::ensures(|result: &usize| *result <= outbound_payload_units(payload_len, fin)))]
#[cfg_attr(kani, kani::ensures(|result: &usize| *result <= payload_len.max(1)))]
pub(crate) fn accepted_outbound_payload_units(
    payload_len: usize,
    fin: bool,
    written: usize,
    fin_accepted: bool,
) -> usize {
    if payload_len == 0 {
        return usize::from(fin && fin_accepted);
    }

    let accepted_payload = written.min(payload_len);
    if fin && !fin_accepted && accepted_payload == payload_len {
        accepted_payload.saturating_sub(1)
    } else {
        accepted_payload
    }
}

pub(crate) fn admit_next(current: usize, units: usize, high_water: usize) -> AdmitOutcome {
    if units == 0 {
        return AdmitOutcome::Admitted {
            next_queued: current,
        };
    }

    let limit = high_water.max(units);
    let Some(next_queued) = current.checked_add(units) else {
        return AdmitOutcome::Backpressured;
    };

    if next_queued <= limit {
        AdmitOutcome::Admitted { next_queued }
    } else {
        AdmitOutcome::Backpressured
    }
}

pub(crate) fn release_next(current: usize, units: usize) -> usize {
    current.saturating_sub(units)
}

pub(crate) fn release_emits_write_ready(
    next_queued: usize,
    low_water: usize,
    was_pressured: bool,
) -> bool {
    next_queued <= low_water && was_pressured
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watermarks_normalize_to_valid_range() {
        assert_eq!(
            AdmissionWatermarks::new(0, 100),
            AdmissionWatermarks { high: 1, low: 0 }
        );
        assert_eq!(
            AdmissionWatermarks::new(10, 9),
            AdmissionWatermarks { high: 10, low: 9 }
        );
    }

    #[test]
    fn oversized_first_payload_is_admitted_once() {
        assert_eq!(
            admit_next(0, 16, 10),
            AdmitOutcome::Admitted { next_queued: 16 }
        );
        assert_eq!(admit_next(16, 1, 10), AdmitOutcome::Backpressured);
    }
}
