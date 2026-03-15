use std::io::Write;
use std::path::PathBuf;

use crate::cid::{CidEncoding, parse_server_id_bytes};
use crate::error::Http3NativeError;
use napi_derive::napi;

const MAX_DATAGRAM_SIZE: usize = 1350;

/// Write bytes to a temp file and return the path.
fn write_temp_file(data: &[u8], suffix: &str) -> Result<std::path::PathBuf, Http3NativeError> {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("http3_{}{}", std::process::id(), suffix));
    let mut f = std::fs::File::create(&path).map_err(Http3NativeError::Io)?;
    f.write_all(data).map_err(Http3NativeError::Io)?;
    Ok(path)
}

struct TempFileGuard {
    path: PathBuf,
}

impl TempFileGuard {
    fn new(data: &[u8], suffix: &str) -> Result<Self, Http3NativeError> {
        Ok(Self {
            path: write_temp_file(data, suffix)?,
        })
    }

    fn as_str(&self, kind: &str) -> Result<&str, Http3NativeError> {
        self.path
            .to_str()
            .ok_or_else(|| Http3NativeError::Config(format!("non-UTF-8 {kind} path")))
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[napi(object)]
pub struct JsServerOptions {
    pub key: napi::bindgen_prelude::Buffer,
    pub cert: napi::bindgen_prelude::Buffer,
    pub ca: Option<napi::bindgen_prelude::Buffer>,
    pub max_idle_timeout_ms: Option<u32>,
    pub max_udp_payload_size: Option<u32>,
    pub initial_max_data: Option<u32>,
    pub initial_max_stream_data_bidi_local: Option<u32>,
    pub initial_max_streams_bidi: Option<u32>,
    pub disable_active_migration: Option<bool>,
    pub enable_datagrams: Option<bool>,
    pub qpack_max_table_capacity: Option<u32>,
    pub qpack_blocked_streams: Option<u32>,
    pub recv_batch_size: Option<u32>,
    pub send_batch_size: Option<u32>,
    pub qlog_dir: Option<String>,
    pub qlog_level: Option<String>,
    pub session_ticket_keys: Option<napi::bindgen_prelude::Buffer>,
    pub max_connections: Option<u32>,
    pub disable_retry: Option<bool>,
    pub reuse_port: Option<bool>,
    pub keylog: Option<bool>,
    pub quic_lb: Option<bool>,
    pub server_id: Option<napi::bindgen_prelude::Buffer>,
}

#[napi(object)]
pub struct JsClientOptions {
    pub ca: Option<napi::bindgen_prelude::Buffer>,
    pub reject_unauthorized: Option<bool>,
    pub max_idle_timeout_ms: Option<u32>,
    pub max_udp_payload_size: Option<u32>,
    pub initial_max_data: Option<u32>,
    pub initial_max_stream_data_bidi_local: Option<u32>,
    pub initial_max_streams_bidi: Option<u32>,
    pub session_ticket: Option<napi::bindgen_prelude::Buffer>,
    pub allow_0rtt: Option<bool>,
    pub enable_datagrams: Option<bool>,
    pub keylog: Option<bool>,
    pub qlog_dir: Option<String>,
    pub qlog_level: Option<String>,
}

pub struct Http3Config {
    pub qlog_dir: Option<String>,
    pub qlog_level: Option<String>,
    pub qpack_max_table_capacity: Option<u64>,
    pub qpack_blocked_streams: Option<u64>,
    pub max_connections: usize,
    pub disable_retry: bool,
    pub reuse_port: bool,
    pub cid_encoding: CidEncoding,
}

impl Http3Config {
    pub fn new_server_quiche_config(
        options: &JsServerOptions,
    ) -> Result<quiche::Config, Http3NativeError> {
        let mut config =
            quiche::Config::new(quiche::PROTOCOL_VERSION).map_err(Http3NativeError::Quiche)?;

        // TLS: write PEM bytes to temp files and load them
        let cert_path = TempFileGuard::new(&options.cert, "_cert.pem")?;
        let key_path = TempFileGuard::new(&options.key, "_key.pem")?;
        config
            .load_cert_chain_from_pem_file(cert_path.as_str("cert")?)
            .map_err(Http3NativeError::Quiche)?;
        config
            .load_priv_key_from_pem_file(key_path.as_str("key")?)
            .map_err(Http3NativeError::Quiche)?;

        if let Some(ca) = options.ca.as_ref() {
            let ca_path = TempFileGuard::new(ca, "_ca.pem")?;
            config
                .load_verify_locations_from_file(ca_path.as_str("ca")?)
                .map_err(Http3NativeError::Quiche)?;
        }

        // ALPN
        config
            .set_application_protos(quiche::h3::APPLICATION_PROTOCOL)
            .map_err(Http3NativeError::Quiche)?;

        // QUIC tuning
        config.set_max_idle_timeout(u64::from(options.max_idle_timeout_ms.unwrap_or(30_000)));
        config.set_max_recv_udp_payload_size(
            options
                .max_udp_payload_size
                .unwrap_or(MAX_DATAGRAM_SIZE as u32) as usize,
        );
        config.set_max_send_udp_payload_size(
            options
                .max_udp_payload_size
                .unwrap_or(MAX_DATAGRAM_SIZE as u32) as usize,
        );
        config.set_initial_max_data(u64::from(options.initial_max_data.unwrap_or(100_000_000)));
        config.set_initial_max_stream_data_bidi_local(u64::from(
            options
                .initial_max_stream_data_bidi_local
                .unwrap_or(2_000_000),
        ));
        config.set_initial_max_stream_data_bidi_remote(2_000_000);
        config.set_initial_max_stream_data_uni(2_000_000);
        config.set_initial_max_streams_bidi(u64::from(
            options.initial_max_streams_bidi.unwrap_or(10_000),
        ));
        config.set_initial_max_streams_uni(1_000);
        config.set_disable_active_migration(options.disable_active_migration.unwrap_or(true));

        if let Some(keys) = options.session_ticket_keys.as_ref() {
            config
                .set_ticket_key(keys)
                .map_err(Http3NativeError::Quiche)?;
        }

        // Allow quiche to buffer well beyond the congestion window so that
        // burst stream creation doesn't hit StreamBlocked before the event
        // loop has a chance to flush packets and receive ACKs.
        config.set_send_capacity_factor(20.0);
        config.set_initial_congestion_window_packets(1000);

        if options.enable_datagrams.unwrap_or(false) {
            config.enable_dgram(true, 1000, 1000);
        }

        if options.keylog.unwrap_or(false) {
            config.log_keys();
        }

        Ok(config)
    }

    pub fn new_client_quiche_config(
        options: &JsClientOptions,
    ) -> Result<quiche::Config, Http3NativeError> {
        let mut config =
            quiche::Config::new(quiche::PROTOCOL_VERSION).map_err(Http3NativeError::Quiche)?;

        // ALPN
        config
            .set_application_protos(quiche::h3::APPLICATION_PROTOCOL)
            .map_err(Http3NativeError::Quiche)?;

        // TLS verification
        if options.reject_unauthorized.unwrap_or(true) {
            config.verify_peer(true);
        } else {
            config.verify_peer(false);
        }

        if let Some(ca) = options.ca.as_ref() {
            let ca_path = TempFileGuard::new(ca, "_ca.pem")?;
            config
                .load_verify_locations_from_file(ca_path.as_str("ca")?)
                .map_err(Http3NativeError::Quiche)?;
        }

        // QUIC tuning
        config.set_max_idle_timeout(u64::from(options.max_idle_timeout_ms.unwrap_or(30_000)));
        config.set_max_recv_udp_payload_size(
            options
                .max_udp_payload_size
                .unwrap_or(MAX_DATAGRAM_SIZE as u32) as usize,
        );
        config.set_max_send_udp_payload_size(
            options
                .max_udp_payload_size
                .unwrap_or(MAX_DATAGRAM_SIZE as u32) as usize,
        );
        config.set_initial_max_data(u64::from(options.initial_max_data.unwrap_or(100_000_000)));
        config.set_initial_max_stream_data_bidi_local(u64::from(
            options
                .initial_max_stream_data_bidi_local
                .unwrap_or(2_000_000),
        ));
        config.set_initial_max_stream_data_bidi_remote(2_000_000);
        config.set_initial_max_stream_data_uni(2_000_000);
        config.set_initial_max_streams_bidi(u64::from(
            options.initial_max_streams_bidi.unwrap_or(10_000),
        ));
        config.set_initial_max_streams_uni(1_000);

        config.set_send_capacity_factor(20.0);
        config.set_initial_congestion_window_packets(1000);

        if options.allow_0rtt.unwrap_or(false) {
            config.enable_early_data();
        }

        if options.enable_datagrams.unwrap_or(false) {
            config.enable_dgram(true, 1000, 1000);
        }

        if options.keylog.unwrap_or(false) {
            config.log_keys();
        }

        Ok(config)
    }

    pub fn from_server_options(options: &JsServerOptions) -> Result<Self, Http3NativeError> {
        let quic_lb = options.quic_lb.unwrap_or(false);
        let cid_encoding = if quic_lb {
            let server_id = options.server_id.as_ref().ok_or_else(|| {
                Http3NativeError::Config("server_id is required when quic_lb is enabled".into())
            })?;
            let server_id = parse_server_id_bytes(server_id)?;
            CidEncoding::quic_lb_plaintext(server_id, 0)?
        } else {
            if options.server_id.is_some() {
                return Err(Http3NativeError::Config(
                    "server_id requires quic_lb=true".into(),
                ));
            }
            CidEncoding::random()
        };

        Ok(Self {
            qlog_dir: options.qlog_dir.clone(),
            qlog_level: options.qlog_level.clone(),
            qpack_max_table_capacity: options.qpack_max_table_capacity.map(u64::from),
            qpack_blocked_streams: options.qpack_blocked_streams.map(u64::from),
            max_connections: options.max_connections.unwrap_or(10_000) as usize,
            disable_retry: options.disable_retry.unwrap_or(false),
            reuse_port: options.reuse_port.unwrap_or(false),
            cid_encoding,
        })
    }
}

// ── QUIC-only config (no HTTP/3 ALPN) ──────────────────────────────

#[napi(object)]
pub struct JsQuicServerOptions {
    pub key: napi::bindgen_prelude::Buffer,
    pub cert: napi::bindgen_prelude::Buffer,
    pub ca: Option<napi::bindgen_prelude::Buffer>,
    pub alpn: Option<Vec<String>>,
    pub max_idle_timeout_ms: Option<u32>,
    pub max_udp_payload_size: Option<u32>,
    pub initial_max_data: Option<u32>,
    pub initial_max_stream_data_bidi_local: Option<u32>,
    pub initial_max_streams_bidi: Option<u32>,
    pub disable_active_migration: Option<bool>,
    pub enable_datagrams: Option<bool>,
    pub max_connections: Option<u32>,
    pub disable_retry: Option<bool>,
    pub qlog_dir: Option<String>,
    pub qlog_level: Option<String>,
    pub session_ticket_keys: Option<napi::bindgen_prelude::Buffer>,
    pub keylog: Option<bool>,
}

#[napi(object)]
pub struct JsQuicClientOptions {
    pub ca: Option<napi::bindgen_prelude::Buffer>,
    pub reject_unauthorized: Option<bool>,
    pub alpn: Option<Vec<String>>,
    pub max_idle_timeout_ms: Option<u32>,
    pub max_udp_payload_size: Option<u32>,
    pub initial_max_data: Option<u32>,
    pub initial_max_stream_data_bidi_local: Option<u32>,
    pub initial_max_streams_bidi: Option<u32>,
    pub session_ticket: Option<napi::bindgen_prelude::Buffer>,
    pub allow_0rtt: Option<bool>,
    pub enable_datagrams: Option<bool>,
    pub keylog: Option<bool>,
    pub qlog_dir: Option<String>,
    pub qlog_level: Option<String>,
}

fn alpn_to_bytes(protocols: &[String]) -> Vec<Vec<u8>> {
    protocols.iter().map(|p| p.as_bytes().to_vec()).collect()
}

fn alpn_refs(protos: &[Vec<u8>]) -> Vec<&[u8]> {
    protos.iter().map(Vec::as_slice).collect()
}

pub fn new_quic_server_config(
    options: &JsQuicServerOptions,
) -> Result<quiche::Config, Http3NativeError> {
    let mut config =
        quiche::Config::new(quiche::PROTOCOL_VERSION).map_err(Http3NativeError::Quiche)?;

    let cert_path = TempFileGuard::new(&options.cert, "_qcert.pem")?;
    let key_path = TempFileGuard::new(&options.key, "_qkey.pem")?;
    config
        .load_cert_chain_from_pem_file(cert_path.as_str("cert")?)
        .map_err(Http3NativeError::Quiche)?;
    config
        .load_priv_key_from_pem_file(key_path.as_str("key")?)
        .map_err(Http3NativeError::Quiche)?;

    if let Some(ca) = options.ca.as_ref() {
        let ca_path = TempFileGuard::new(ca, "_qca.pem")?;
        config
            .load_verify_locations_from_file(ca_path.as_str("ca")?)
            .map_err(Http3NativeError::Quiche)?;
    }

    let default_alpn = vec!["quic".to_string()];
    let alpn_protos = options.alpn.as_deref().unwrap_or(&default_alpn);
    let alpn_bytes = alpn_to_bytes(alpn_protos);
    let alpn_slice = alpn_refs(&alpn_bytes);
    config
        .set_application_protos(&alpn_slice)
        .map_err(Http3NativeError::Quiche)?;

    config.set_max_idle_timeout(u64::from(options.max_idle_timeout_ms.unwrap_or(30_000)));
    config.set_max_recv_udp_payload_size(
        options
            .max_udp_payload_size
            .unwrap_or(MAX_DATAGRAM_SIZE as u32) as usize,
    );
    config.set_max_send_udp_payload_size(
        options
            .max_udp_payload_size
            .unwrap_or(MAX_DATAGRAM_SIZE as u32) as usize,
    );
    config.set_initial_max_data(u64::from(options.initial_max_data.unwrap_or(100_000_000)));
    config.set_initial_max_stream_data_bidi_local(u64::from(
        options
            .initial_max_stream_data_bidi_local
            .unwrap_or(2_000_000),
    ));
    config.set_initial_max_stream_data_bidi_remote(2_000_000);
    config.set_initial_max_stream_data_uni(2_000_000);
    config.set_initial_max_streams_bidi(u64::from(
        options.initial_max_streams_bidi.unwrap_or(10_000),
    ));
    config.set_initial_max_streams_uni(1_000);
    config.set_disable_active_migration(options.disable_active_migration.unwrap_or(true));

    if let Some(keys) = options.session_ticket_keys.as_ref() {
        config
            .set_ticket_key(keys)
            .map_err(Http3NativeError::Quiche)?;
    }

    config.set_send_capacity_factor(20.0);
    config.set_initial_congestion_window_packets(1000);

    if options.enable_datagrams.unwrap_or(false) {
        config.enable_dgram(true, 1000, 1000);
    }

    if options.keylog.unwrap_or(false) {
        config.log_keys();
    }

    Ok(config)
}

pub fn new_quic_client_config(
    options: &JsQuicClientOptions,
) -> Result<quiche::Config, Http3NativeError> {
    let mut config =
        quiche::Config::new(quiche::PROTOCOL_VERSION).map_err(Http3NativeError::Quiche)?;

    let default_alpn = vec!["quic".to_string()];
    let alpn_protos = options.alpn.as_deref().unwrap_or(&default_alpn);
    let alpn_bytes = alpn_to_bytes(alpn_protos);
    let alpn_slice = alpn_refs(&alpn_bytes);
    config
        .set_application_protos(&alpn_slice)
        .map_err(Http3NativeError::Quiche)?;

    if options.reject_unauthorized.unwrap_or(true) {
        config.verify_peer(true);
    } else {
        config.verify_peer(false);
    }

    if let Some(ca) = options.ca.as_ref() {
        let ca_path = TempFileGuard::new(ca, "_qca.pem")?;
        config
            .load_verify_locations_from_file(ca_path.as_str("ca")?)
            .map_err(Http3NativeError::Quiche)?;
    }

    config.set_max_idle_timeout(u64::from(options.max_idle_timeout_ms.unwrap_or(30_000)));
    config.set_max_recv_udp_payload_size(
        options
            .max_udp_payload_size
            .unwrap_or(MAX_DATAGRAM_SIZE as u32) as usize,
    );
    config.set_max_send_udp_payload_size(
        options
            .max_udp_payload_size
            .unwrap_or(MAX_DATAGRAM_SIZE as u32) as usize,
    );
    config.set_initial_max_data(u64::from(options.initial_max_data.unwrap_or(100_000_000)));
    config.set_initial_max_stream_data_bidi_local(u64::from(
        options
            .initial_max_stream_data_bidi_local
            .unwrap_or(2_000_000),
    ));
    config.set_initial_max_stream_data_bidi_remote(2_000_000);
    config.set_initial_max_stream_data_uni(2_000_000);
    config.set_initial_max_streams_bidi(u64::from(
        options.initial_max_streams_bidi.unwrap_or(10_000),
    ));
    config.set_initial_max_streams_uni(1_000);

    config.set_send_capacity_factor(20.0);
    config.set_initial_congestion_window_packets(1000);

    if options.allow_0rtt.unwrap_or(false) {
        config.enable_early_data();
    }

    if options.enable_datagrams.unwrap_or(false) {
        config.enable_dgram(true, 1000, 1000);
    }

    if options.keylog.unwrap_or(false) {
        config.log_keys();
    }

    Ok(config)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::cid::{CidEncoding, QUIC_LB_SERVER_ID_LEN};

    fn base_server_options() -> JsServerOptions {
        JsServerOptions {
            key: vec![1u8].into(),
            cert: vec![2u8].into(),
            ca: None,
            max_idle_timeout_ms: None,
            max_udp_payload_size: None,
            initial_max_data: None,
            initial_max_stream_data_bidi_local: None,
            initial_max_streams_bidi: None,
            disable_active_migration: None,
            enable_datagrams: None,
            qpack_max_table_capacity: None,
            qpack_blocked_streams: None,
            recv_batch_size: None,
            send_batch_size: None,
            qlog_dir: None,
            qlog_level: None,
            session_ticket_keys: None,
            max_connections: None,
            disable_retry: None,
            reuse_port: None,
            keylog: None,
            quic_lb: None,
            server_id: None,
        }
    }

    #[test]
    fn from_server_options_rejects_quic_lb_without_server_id() {
        let mut options = base_server_options();
        options.quic_lb = Some(true);

        let err = match Http3Config::from_server_options(&options) {
            Err(err) => err,
            Ok(_) => panic!("expected config error for missing server_id"),
        };
        assert!(
            err.to_string()
                .contains("server_id is required when quic_lb is enabled")
        );
    }

    #[test]
    fn from_server_options_rejects_server_id_without_quic_lb() {
        let mut options = base_server_options();
        options.server_id = Some(vec![0u8; QUIC_LB_SERVER_ID_LEN].into());

        let err = match Http3Config::from_server_options(&options) {
            Err(err) => err,
            Ok(_) => panic!("expected config error when quic_lb is disabled"),
        };
        assert!(err.to_string().contains("server_id requires quic_lb=true"));
    }

    #[test]
    fn from_server_options_accepts_valid_quic_lb_server_id() {
        let mut options = base_server_options();
        options.quic_lb = Some(true);
        options.server_id = Some(vec![0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88].into());

        let cfg = Http3Config::from_server_options(&options).expect("valid quic_lb config");
        match cfg.cid_encoding {
            CidEncoding::QuicLbPlaintext { server_id, .. } => {
                assert_eq!(server_id, [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88]);
            }
            CidEncoding::Random => panic!("expected QUIC-LB plaintext CID encoding"),
        }
    }
}
