//! Serializes `JsH3Event` batches into the wire format `h3c_drain_events` /
//! `qc_drain_events` return: one JSON array using the exact TS field names
//! (`eventType`, `streamId`, `headers`, `fin`, `meta`, `metrics`), with
//! `data` payloads replaced by `dataOff`/`dataLen` — absolute offsets into
//! this module's own linear memory (the same address space `h3c_rx_buffer`
//! etc. already hand out pointers into), valid only until the next call on
//! the handle (see the crate-level doc comment).

use http3::wasm_exports::{JsEventMeta, JsH3Event, JsSessionMetrics};
use serde_json::{Map, Value};

/// Serialize `events` into `json_scratch` (cleared and overwritten) and
/// append every event's `data` payload into `data_scratch` (cleared first,
/// then grown once per event) so callers can compute a single stable base
/// pointer after all pushes are done — `Vec::as_ptr()` is only meaningful
/// once nothing will trigger a reallocation.
pub(crate) fn serialize_events(
    events: &[JsH3Event],
    json_scratch: &mut Vec<u8>,
    data_scratch: &mut Vec<u8>,
) {
    data_scratch.clear();
    let mut values: Vec<Value> = Vec::with_capacity(events.len());
    for event in events {
        values.push(event_to_json_relative(event, data_scratch));
    }

    // `data_scratch` is now fully built and will not reallocate again
    // before the caller reads it — safe to take one absolute base pointer
    // and bake it into every event's relative offset.
    let base = data_scratch.as_ptr() as u64;
    for value in &mut values {
        let Value::Object(obj) = value else { continue };
        let Some(off) = obj.get("dataOff").and_then(Value::as_u64) else {
            continue;
        };
        obj.insert("dataOff".to_string(), Value::from(off + base));
    }

    json_scratch.clear();
    // `Vec<u8>` implements `std::io::Write` — no separate string buffer.
    let _ = serde_json::to_writer(&mut *json_scratch, &Value::Array(values));
}

fn event_to_json_relative(event: &JsH3Event, data_scratch: &mut Vec<u8>) -> Value {
    let mut obj = Map::new();
    obj.insert("eventType".to_string(), Value::from(event.event_type));
    obj.insert("streamId".to_string(), Value::from(event.stream_id));

    if let Some(fin) = event.fin {
        obj.insert("fin".to_string(), Value::from(fin));
    }

    if let Some(headers) = &event.headers {
        let arr: Vec<Value> = headers
            .iter()
            .map(|h| {
                let mut hm = Map::new();
                hm.insert("name".to_string(), Value::from(h.name.clone()));
                hm.insert("value".to_string(), Value::from(h.value.clone()));
                Value::Object(hm)
            })
            .collect();
        obj.insert("headers".to_string(), Value::Array(arr));
    }

    if let Some(data) = &event.data {
        // Relative offset for now; `serialize_events` rewrites this to an
        // absolute linear-memory address once `data_scratch` is final.
        let off = data_scratch.len() as u64;
        data_scratch.extend_from_slice(data);
        obj.insert("dataOff".to_string(), Value::from(off));
        obj.insert("dataLen".to_string(), Value::from(data.len() as u64));
    }

    if let Some(meta) = &event.meta {
        obj.insert("meta".to_string(), meta_to_json(meta));
    }

    if let Some(metrics) = &event.metrics {
        obj.insert("metrics".to_string(), metrics_to_json(metrics));
    }

    Value::Object(obj)
}

fn meta_to_json(meta: &JsEventMeta) -> Value {
    let mut obj = Map::new();
    if let Some(v) = meta.error_code {
        obj.insert("errorCode".to_string(), Value::from(v));
    }
    if let Some(v) = &meta.error_reason {
        obj.insert("errorReason".to_string(), Value::from(v.clone()));
    }
    if let Some(v) = &meta.error_category {
        obj.insert("errorCategory".to_string(), Value::from(v.clone()));
    }
    if let Some(v) = &meta.remote_addr {
        obj.insert("remoteAddr".to_string(), Value::from(v.clone()));
    }
    if let Some(v) = meta.remote_port {
        obj.insert("remotePort".to_string(), Value::from(v));
    }
    if let Some(v) = &meta.server_name {
        obj.insert("serverName".to_string(), Value::from(v.clone()));
    }
    if let Some(v) = &meta.reason_code {
        obj.insert("reasonCode".to_string(), Value::from(v.clone()));
    }
    if let Some(v) = &meta.runtime_driver {
        obj.insert("runtimeDriver".to_string(), Value::from(v.clone()));
    }
    if let Some(v) = &meta.runtime_mode {
        obj.insert("runtimeMode".to_string(), Value::from(v.clone()));
    }
    if let Some(v) = &meta.requested_runtime_mode {
        obj.insert("requestedRuntimeMode".to_string(), Value::from(v.clone()));
    }
    if let Some(v) = meta.fallback_occurred {
        obj.insert("fallbackOccurred".to_string(), Value::from(v));
    }
    if let Some(v) = meta.errno {
        obj.insert("errno".to_string(), Value::from(v));
    }
    if let Some(v) = &meta.syscall {
        obj.insert("syscall".to_string(), Value::from(v.clone()));
    }
    if let Some(v) = meta.peer_certificate_presented {
        obj.insert("peerCertificatePresented".to_string(), Value::from(v));
    }
    // `peerCertificateChain` deliberately omitted: the client path never
    // populates it today (native or wasm — see docs/WASM_CLIENT_PLAN.md
    // decision log "peer_certificate_chain on client HANDSHAKE_COMPLETE —
    // deferred"), and encoding a `Vec<Vec<u8>>` through the scratch-buffer
    // scheme isn't worth the complexity until something actually sets it.
    if let Some(v) = meta.duration_ms {
        obj.insert("durationMs".to_string(), Value::from(v));
    }
    Value::Object(obj)
}

/// Standalone session-metrics JSON string, for `h3c_session_metrics` /
/// `qc_session_metrics` (outside the per-event batch context).
pub(crate) fn metrics_only_json(metrics: &JsSessionMetrics) -> String {
    metrics_to_json(metrics).to_string()
}

/// 8-field TS shape — omits native's extra `pmtu` field (§6.4: "Metrics:
/// 8-field TS shape (omit native's extra pmtu field)").
fn metrics_to_json(metrics: &JsSessionMetrics) -> Value {
    let mut obj = Map::new();
    obj.insert("packetsIn".to_string(), Value::from(metrics.packets_in));
    obj.insert("packetsOut".to_string(), Value::from(metrics.packets_out));
    obj.insert("bytesIn".to_string(), Value::from(metrics.bytes_in));
    obj.insert("bytesOut".to_string(), Value::from(metrics.bytes_out));
    obj.insert(
        "handshakeTimeMs".to_string(),
        Value::from(metrics.handshake_time_ms),
    );
    obj.insert("rttMs".to_string(), Value::from(metrics.rtt_ms));
    obj.insert("cwnd".to_string(), Value::from(metrics.cwnd));
    obj.insert(
        "datagramQueueDepth".to_string(),
        Value::from(metrics.datagram_queue_depth),
    );
    Value::Object(obj)
}

#[cfg(test)]
mod tests {
    use super::serialize_events;
    use http3::wasm_exports::JsH3Event;

    #[test]
    fn empty_batch_serializes_to_empty_array() {
        let mut json_scratch = Vec::new();
        let mut data_scratch = Vec::new();
        serialize_events(&[], &mut json_scratch, &mut data_scratch);
        assert_eq!(json_scratch, b"[]");
    }

    #[test]
    fn data_event_gets_absolute_offset_into_data_scratch() {
        let events = vec![JsH3Event::data(1, 4, vec![1, 2, 3, 4], false, ())];
        let mut json_scratch = Vec::new();
        let mut data_scratch = Vec::new();
        serialize_events(&events, &mut json_scratch, &mut data_scratch);

        let parsed: serde_json::Value = serde_json::from_slice(&json_scratch).unwrap();
        let arr = parsed.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        let obj = &arr[0];
        assert_eq!(obj["eventType"], 4);
        assert_eq!(obj["streamId"], 4);
        assert_eq!(obj["dataLen"], 4);
        let off = obj["dataOff"].as_u64().unwrap();
        let base = data_scratch.as_ptr() as u64;
        assert_eq!(off, base);
        assert_eq!(&data_scratch[..4], &[1, 2, 3, 4]);
    }

    #[test]
    fn multiple_data_events_get_distinct_non_overlapping_offsets() {
        let events = vec![
            JsH3Event::data(1, 4, vec![9, 9], false, ()),
            JsH3Event::data(1, 8, vec![1, 2, 3], true, ()),
        ];
        let mut json_scratch = Vec::new();
        let mut data_scratch = Vec::new();
        serialize_events(&events, &mut json_scratch, &mut data_scratch);
        let parsed: serde_json::Value = serde_json::from_slice(&json_scratch).unwrap();
        let arr = parsed.as_array().unwrap();
        let base = data_scratch.as_ptr() as u64;
        let off0 = arr[0]["dataOff"].as_u64().unwrap();
        let off1 = arr[1]["dataOff"].as_u64().unwrap();
        assert_eq!(off0, base);
        assert_eq!(off1, base + 2);
        assert_eq!(arr[1]["fin"], true);
    }
}
