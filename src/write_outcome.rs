//! Structured JS-facing outcomes for stream write admission.
//!
//! These objects describe whether the local native command window accepted a
//! write. They intentionally do not mean peer delivery or QUIC ACK; quiche
//! flow-control stalls are reported later through STREAM_BLOCKED/DRAIN.

#[cfg(feature = "node-api")]
use napi_derive::napi;

use crate::outbound_admission::outbound_payload_units;

pub(crate) const STREAM_SEND_STATUS_ACCEPTED: u8 = 0;
pub(crate) const STREAM_SEND_STATUS_BACKPRESSURE: u8 = 1;

#[cfg_attr(feature = "node-api", napi(object))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JsStreamSendOutcome {
    pub status: u8,
    pub written: i64,
}

impl JsStreamSendOutcome {
    pub(crate) fn accepted(payload_len: usize, fin: bool) -> Self {
        Self {
            status: STREAM_SEND_STATUS_ACCEPTED,
            written: units_to_i64(outbound_payload_units(payload_len, fin)),
        }
    }

    pub(crate) const fn backpressured() -> Self {
        Self {
            status: STREAM_SEND_STATUS_BACKPRESSURE,
            written: 0,
        }
    }
}

fn units_to_i64(units: usize) -> i64 {
    match i64::try_from(units) {
        Ok(value) => value,
        Err(_) => i64::MAX,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        JsStreamSendOutcome, STREAM_SEND_STATUS_ACCEPTED, STREAM_SEND_STATUS_BACKPRESSURE,
    };

    #[test]
    fn accepted_payload_reports_payload_units() {
        assert_eq!(
            JsStreamSendOutcome::accepted(32, false),
            JsStreamSendOutcome {
                status: STREAM_SEND_STATUS_ACCEPTED,
                written: 32,
            }
        );
    }

    #[test]
    fn accepted_fin_only_reports_one_unit() {
        assert_eq!(
            JsStreamSendOutcome::accepted(0, true),
            JsStreamSendOutcome {
                status: STREAM_SEND_STATUS_ACCEPTED,
                written: 1,
            }
        );
    }

    #[test]
    fn backpressure_reports_zero_units() {
        assert_eq!(
            JsStreamSendOutcome::backpressured(),
            JsStreamSendOutcome {
                status: STREAM_SEND_STATUS_BACKPRESSURE,
                written: 0,
            }
        );
    }
}
