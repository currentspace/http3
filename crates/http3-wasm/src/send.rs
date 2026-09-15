use http3::wasm_exports::{EVENT_ERROR, JsH3Event};

use crate::abi::{ERR_AGAIN, ERR_PROTOCOL};

/// The ABI reports ownership transferred, not just bytes flushed to quiche.
/// Retrying a retained remainder would enqueue those bytes (or FIN) twice.
/// STREAM_BLOCKED/DRAIN events gate subsequent writes while Rust drains it.
pub(crate) fn classify_send_outcome(
    events: &[JsH3Event],
    len: usize,
    fin: bool,
    released: usize,
    retained: bool,
) -> i64 {
    let requested = if len == 0 && fin { 1 } else { len };
    if events.iter().any(|event| event.event_type == EVENT_ERROR) {
        ERR_PROTOCOL
    } else if retained || released == requested {
        requested as i64
    } else {
        ERR_AGAIN
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffered_bytes_and_fin_are_admitted_without_retry() {
        assert_eq!(classify_send_outcome(&[], 8192, false, 4000, true), 8192);
        assert_eq!(classify_send_outcome(&[], 8192, true, 0, true), 8192);
        assert_eq!(classify_send_outcome(&[], 0, true, 0, true), 1);
        assert_eq!(classify_send_outcome(&[], 8192, false, 8192, false), 8192);
        assert_eq!(classify_send_outcome(&[], 0, true, 1, false), 1);
    }

    #[test]
    fn rejected_or_discarded_writes_are_not_admitted() {
        assert_eq!(classify_send_outcome(&[], 8192, false, 0, false), ERR_AGAIN);
        assert_eq!(classify_send_outcome(&[], 0, true, 0, false), ERR_AGAIN);
        let events = [JsH3Event::error(0, 0, 0, "send failed".into())];
        assert_eq!(
            classify_send_outcome(&events, 8192, false, 8192, false),
            ERR_PROTOCOL
        );
        assert_eq!(
            classify_send_outcome(&events, 8192, false, 0, true),
            ERR_PROTOCOL
        );
    }
}
