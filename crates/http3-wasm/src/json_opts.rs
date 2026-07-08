//! Parses the `h3c_new` / `qc_new` options JSON object into the existing
//! `JsClientOptions` / `JsQuicClientOptions` structs, plus the ABI-only
//! connection-setup fields (`serverAddr`, `serverName`, `localAddr`,
//! `scidHex`) that native callers derive from an actual socket/`ring`
//! instead (see docs/WASM_CLIENT_PLAN.md A3 "h3c_new options (normative)").
//!
//! camelCase field names match the native options object built in
//! `lib/client.ts` / `lib/quic-client.ts` exactly (checked against those
//! files directly, not guessed).

use std::net::SocketAddr;

use http3::wasm_exports::{JsClientOptions, JsQuicClientOptions};
use serde_json::Value;

use crate::encoding::{decode_base64, decode_hex};

fn get_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn get_u32(v: &Value, key: &str) -> Option<u32> {
    v.get(key)
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
}

fn get_bool(v: &Value, key: &str) -> Option<bool> {
    v.get(key).and_then(Value::as_bool)
}

fn get_string_vec(v: &Value, key: &str) -> Option<Vec<String>> {
    v.get(key)?.as_array().map(|arr| {
        arr.iter()
            .filter_map(|item| item.as_str().map(String::from))
            .collect()
    })
}

/// PEM fields (`ca`, `cert`, `key`) cross the ABI as UTF-8 PEM strings —
/// just re-encode as bytes, no decoding needed.
fn get_pem_bytes(v: &Value, key: &str) -> Option<Vec<u8>> {
    get_str(v, key).map(|s| s.as_bytes().to_vec())
}

fn get_base64_bytes(v: &Value, key: &str) -> Result<Option<Vec<u8>>, String> {
    match get_str(v, key) {
        None => Ok(None),
        Some(s) => decode_base64(s)
            .map(Some)
            .ok_or_else(|| format!("invalid base64 in field '{key}'")),
    }
}

/// The ABI-only fields common to both `h3c_new` and `qc_new`: the
/// connected-flow peer/local addresses, SNI server name, and the
/// host-RNG-sourced SCID (native derives all of these from a live
/// socket + `ring`; a sans-IO caller must supply them explicitly).
pub(crate) struct ConnectParams {
    pub(crate) server_addr: SocketAddr,
    pub(crate) server_name: String,
    pub(crate) local_addr: SocketAddr,
    pub(crate) scid: Vec<u8>,
    pub(crate) keylog: bool,
}

pub(crate) fn parse_connect_params(v: &Value) -> Result<ConnectParams, String> {
    let server_addr_str = get_str(v, "serverAddr").ok_or("missing serverAddr")?;
    let server_addr = server_addr_str
        .parse::<SocketAddr>()
        .map_err(|e| format!("invalid serverAddr '{server_addr_str}': {e}"))?;

    let server_name = get_str(v, "serverName")
        .ok_or("missing serverName")?
        .to_string();

    let local_addr_str = get_str(v, "localAddr").ok_or("missing localAddr")?;
    let local_addr = local_addr_str
        .parse::<SocketAddr>()
        .map_err(|e| format!("invalid localAddr '{local_addr_str}': {e}"))?;

    let scid_hex = get_str(v, "scidHex").ok_or("missing scidHex")?;
    if scid_hex.len() != 40 {
        return Err(format!(
            "scidHex must be exactly 40 lowercase hex chars (20 bytes), got {} chars",
            scid_hex.len()
        ));
    }
    let scid = decode_hex(scid_hex).ok_or("scidHex must be lowercase hex digits only")?;

    let keylog = get_bool(v, "keylog").unwrap_or(false);

    Ok(ConnectParams {
        server_addr,
        server_name,
        local_addr,
        scid,
        keylog,
    })
}

/// Fields ignored on wasm (documented, not errors — see A3 "h3c_new
/// options (normative)"): `runtimeMode`, `fallbackPolicy`, `qlogDir`,
/// `qlogLevel` (N5: qlog excluded from the wasm build), and `keylog` as a
/// string path (only the boolean form is honored; delivery is via the
/// `take_keylog` export instead of file tailing).
pub(crate) fn build_h3_options(v: &Value) -> Result<JsClientOptions, String> {
    Ok(JsClientOptions {
        ca: get_pem_bytes(v, "ca"),
        reject_unauthorized: get_bool(v, "rejectUnauthorized"),
        runtime_mode: None,
        max_idle_timeout_ms: get_u32(v, "maxIdleTimeoutMs"),
        max_udp_payload_size: get_u32(v, "maxUdpPayloadSize"),
        initial_max_data: get_u32(v, "initialMaxData"),
        initial_max_stream_data_bidi_local: get_u32(v, "initialMaxStreamDataBidiLocal"),
        initial_max_streams_bidi: get_u32(v, "initialMaxStreamsBidi"),
        session_ticket: get_base64_bytes(v, "sessionTicket")?,
        allow_0rtt: get_bool(v, "allow0rtt"),
        enable_datagrams: get_bool(v, "enableDatagrams"),
        keylog: get_bool(v, "keylog"),
        qlog_dir: None,
        qlog_level: None,
        disable_pacing: get_bool(v, "disablePacing"),
    })
}

pub(crate) fn build_quic_options(v: &Value) -> Result<JsQuicClientOptions, String> {
    Ok(JsQuicClientOptions {
        ca: get_pem_bytes(v, "ca"),
        cert: get_pem_bytes(v, "cert"),
        key: get_pem_bytes(v, "key"),
        reject_unauthorized: get_bool(v, "rejectUnauthorized"),
        alpn: get_string_vec(v, "alpn"),
        runtime_mode: None,
        max_idle_timeout_ms: get_u32(v, "maxIdleTimeoutMs"),
        max_udp_payload_size: get_u32(v, "maxUdpPayloadSize"),
        initial_max_data: get_u32(v, "initialMaxData"),
        initial_max_stream_data_bidi_local: get_u32(v, "initialMaxStreamDataBidiLocal"),
        initial_max_streams_bidi: get_u32(v, "initialMaxStreamsBidi"),
        session_ticket: get_base64_bytes(v, "sessionTicket")?,
        allow_0rtt: get_bool(v, "allow0rtt"),
        enable_datagrams: get_bool(v, "enableDatagrams"),
        keylog: get_bool(v, "keylog"),
        qlog_dir: None,
        qlog_level: None,
        disable_pacing: get_bool(v, "disablePacing"),
    })
}

#[cfg(test)]
mod tests {
    use super::parse_connect_params;
    use serde_json::json;

    #[test]
    fn parses_valid_connect_params() {
        let scid_hex = "00".repeat(20);
        let v = json!({
            "serverAddr": "127.0.0.1:4433",
            "serverName": "example.test",
            "localAddr": "127.0.0.1:0",
            "scidHex": scid_hex,
        });
        let params = parse_connect_params(&v).unwrap();
        assert_eq!(params.server_addr.port(), 4433);
        assert_eq!(params.server_name, "example.test");
        assert_eq!(params.scid.len(), 20);
        assert!(!params.keylog);
    }

    #[test]
    fn rejects_missing_fields() {
        let v = json!({});
        assert!(parse_connect_params(&v).is_err());
    }

    #[test]
    fn rejects_short_scid_hex() {
        let v = json!({
            "serverAddr": "127.0.0.1:4433",
            "serverName": "example.test",
            "localAddr": "127.0.0.1:0",
            "scidHex": "abcd",
        });
        assert!(parse_connect_params(&v).is_err());
    }

    #[test]
    fn accepts_ipv6_addrs() {
        let scid_hex = "ab".repeat(20);
        let v = json!({
            "serverAddr": "[::1]:4433",
            "serverName": "example.test",
            "localAddr": "[::1]:0",
            "scidHex": scid_hex,
        });
        let params = parse_connect_params(&v).unwrap();
        assert!(params.server_addr.is_ipv6());
    }
}
