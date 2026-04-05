//! QUIC/TLS configuration builder that translates JS option objects into
//! `quiche::Config` instances for both server and client use.

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use crate::cid::{CidEncoding, parse_server_id_bytes};
use crate::error::Http3NativeError;
#[cfg(feature = "node-api")]
use napi_derive::napi;

#[cfg(feature = "node-api")]
type ByteBuf = napi::bindgen_prelude::Buffer;
#[cfg(not(feature = "node-api"))]
type ByteBuf = Vec<u8>;

/// Fallback DPLPMTUD probe ceiling used when path MTU auto-detection is
/// unavailable (non-Linux, unresolvable destination, etc.).
///
/// 1472 = 1500 (Ethernet MTU) - 20 (IPv4) - 8 (UDP).  quiche probes from
/// 1200 up to this ceiling; on standard Ethernet the first probe succeeds
/// immediately.
pub(crate) const FALLBACK_MAX_UDP_PAYLOAD: usize = 1472;

/// Return the PMTUD probe ceiling for a connection to `peer`.
///
/// Queries the kernel routing table for the interface MTU on the path to
/// `peer` and caps at 16383 (quiche's max data packet size, limited by
/// 2-byte QUIC varint encoding).  On loopback this returns 16383; on
/// standard Ethernet it returns 1472; on jumbo frames ~8972.
///
/// Falls back to `FALLBACK_MAX_UDP_PAYLOAD` (1472) if the query fails.
pub fn effective_pmtud_ceiling(peer: &std::net::SocketAddr) -> usize {
    crate::transport::socket::query_path_mtu(peer).unwrap_or(FALLBACK_MAX_UDP_PAYLOAD)
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TransportRuntimeMode {
    Fast,
    Portable,
}

impl Default for TransportRuntimeMode {
    fn default() -> Self {
        Self::Fast
    }
}

impl TransportRuntimeMode {
    pub fn parse(value: Option<&str>) -> Result<Self, Http3NativeError> {
        match value.unwrap_or("fast") {
            "fast" => Ok(Self::Fast),
            "portable" => Ok(Self::Portable),
            other => Err(Http3NativeError::Config(format!(
                "invalid runtimeMode: {other} (expected 'fast' or 'portable')",
            ))),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ClientAuthMode {
    None,
    Request,
    Require,
}

impl ClientAuthMode {
    pub fn parse(value: Option<&str>, has_ca: bool) -> Result<Self, Http3NativeError> {
        match value {
            None => Ok(if has_ca { Self::Require } else { Self::None }),
            Some("none") => {
                if has_ca {
                    return Err(Http3NativeError::Config(
                        "clientAuth='none' cannot be combined with ca".into(),
                    ));
                }
                Ok(Self::None)
            }
            Some("request") => {
                if !has_ca {
                    return Err(Http3NativeError::Config(
                        "clientAuth='request' requires ca".into(),
                    ));
                }
                Ok(Self::Request)
            }
            Some("require") => {
                if !has_ca {
                    return Err(Http3NativeError::Config(
                        "clientAuth='require' requires ca".into(),
                    ));
                }
                Ok(Self::Require)
            }
            Some(other) => Err(Http3NativeError::Config(format!(
                "invalid clientAuth: {other} (expected 'none', 'request', or 'require')",
            ))),
        }
    }

    pub fn verify_peer(self) -> bool {
        !matches!(self, Self::None)
    }

    pub fn require_client_cert(self) -> bool {
        matches!(self, Self::Require)
    }
}

/// Apply standard congestion and PMTU tuning to a quiche `Config`.
///
/// Enables DPLPMTUD (RFC 8899) so quiche discovers the actual path MTU per
/// connection.  The probe ceiling is `FALLBACK_MAX_UDP_PAYLOAD` (1472, standard
/// Ethernet).  On Ethernet the first probe succeeds immediately; the discovery
/// completes in one RTT with zero wasted probes.
///
/// `max_probes = 1`: each probe size is abandoned after a single loss instead
/// of the RFC default of 3.  This prevents the stall pattern where a large
/// failed probe charges the congestion window, waits for 3× PTO loss timeout,
/// and blocks all subsequent probes via the `in_flight` flag.  With max_probes=1
/// the binary search converges in O(log2(ceiling - 1200)) RTTs with one
/// loss-detection delay per level instead of three.
fn apply_congestion_tuning(config: &mut quiche::Config) {
    config.set_send_capacity_factor(20.0);
    config.set_initial_congestion_window_packets(1000);
    config.discover_pmtu(true);
    config.set_pmtud_max_probes(1);
}

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

#[cfg_attr(feature = "node-api", napi(object))]
pub struct JsServerOptions {
    pub key: ByteBuf,
    pub cert: ByteBuf,
    pub ca: Option<ByteBuf>,
    pub runtime_mode: Option<String>,
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
    pub session_ticket_keys: Option<ByteBuf>,
    pub max_connections: Option<u32>,
    pub disable_retry: Option<bool>,
    pub reuse_port: Option<bool>,
    pub keylog: Option<bool>,
    pub quic_lb: Option<bool>,
    pub server_id: Option<ByteBuf>,
    pub keepalive_interval_ms: Option<u32>,
}

#[cfg_attr(feature = "node-api", napi(object))]
pub struct JsClientOptions {
    pub ca: Option<ByteBuf>,
    pub reject_unauthorized: Option<bool>,
    pub runtime_mode: Option<String>,
    pub max_idle_timeout_ms: Option<u32>,
    pub max_udp_payload_size: Option<u32>,
    pub initial_max_data: Option<u32>,
    pub initial_max_stream_data_bidi_local: Option<u32>,
    pub initial_max_streams_bidi: Option<u32>,
    pub session_ticket: Option<ByteBuf>,
    pub allow_0rtt: Option<bool>,
    pub enable_datagrams: Option<bool>,
    pub keylog: Option<bool>,
    pub qlog_dir: Option<String>,
    pub qlog_level: Option<String>,
    pub keepalive_interval_ms: Option<u32>,
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
    pub runtime_mode: TransportRuntimeMode,
    pub keepalive_interval: Option<Duration>,
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
                .unwrap_or(FALLBACK_MAX_UDP_PAYLOAD as u32) as usize,
        );
        config.set_max_send_udp_payload_size(
            options
                .max_udp_payload_size
                .unwrap_or(FALLBACK_MAX_UDP_PAYLOAD as u32) as usize,
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

        apply_congestion_tuning(&mut config);

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
                .unwrap_or(FALLBACK_MAX_UDP_PAYLOAD as u32) as usize,
        );
        config.set_max_send_udp_payload_size(
            options
                .max_udp_payload_size
                .unwrap_or(FALLBACK_MAX_UDP_PAYLOAD as u32) as usize,
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

        apply_congestion_tuning(&mut config);

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
            runtime_mode: TransportRuntimeMode::parse(options.runtime_mode.as_deref())?,
            keepalive_interval: options
                .keepalive_interval_ms
                .map(|ms| Duration::from_millis(u64::from(ms))),
        })
    }
}

// ── QUIC-only config (no HTTP/3 ALPN) ──────────────────────────────

#[cfg_attr(feature = "node-api", napi(object))]
pub struct JsQuicServerOptions {
    pub key: ByteBuf,
    pub cert: ByteBuf,
    pub ca: Option<ByteBuf>,
    pub client_auth: Option<String>,
    pub alpn: Option<Vec<String>>,
    pub runtime_mode: Option<String>,
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
    pub session_ticket_keys: Option<ByteBuf>,
    pub keylog: Option<bool>,
    pub keepalive_interval_ms: Option<u32>,
}

#[cfg_attr(feature = "node-api", napi(object))]
pub struct JsQuicClientOptions {
    pub ca: Option<ByteBuf>,
    pub cert: Option<ByteBuf>,
    pub key: Option<ByteBuf>,
    pub reject_unauthorized: Option<bool>,
    pub alpn: Option<Vec<String>>,
    pub runtime_mode: Option<String>,
    pub max_idle_timeout_ms: Option<u32>,
    pub max_udp_payload_size: Option<u32>,
    pub initial_max_data: Option<u32>,
    pub initial_max_stream_data_bidi_local: Option<u32>,
    pub initial_max_streams_bidi: Option<u32>,
    pub session_ticket: Option<ByteBuf>,
    pub allow_0rtt: Option<bool>,
    pub enable_datagrams: Option<bool>,
    pub keylog: Option<bool>,
    pub qlog_dir: Option<String>,
    pub qlog_level: Option<String>,
    pub keepalive_interval_ms: Option<u32>,
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
    let client_auth = ClientAuthMode::parse(options.client_auth.as_deref(), options.ca.is_some())?;

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
    config.verify_peer(client_auth.verify_peer());

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
            .unwrap_or(FALLBACK_MAX_UDP_PAYLOAD as u32) as usize,
    );
    config.set_max_send_udp_payload_size(
        options
            .max_udp_payload_size
            .unwrap_or(FALLBACK_MAX_UDP_PAYLOAD as u32) as usize,
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

    apply_congestion_tuning(&mut config);

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

    match (options.cert.as_ref(), options.key.as_ref()) {
        (Some(cert), Some(key)) => {
            let cert_path = TempFileGuard::new(cert, "_qclient-cert.pem")?;
            config
                .load_cert_chain_from_pem_file(cert_path.as_str("client cert")?)
                .map_err(|err| {
                    Http3NativeError::Config(format!("invalid client certificate PEM: {err}"))
                })?;

            let key_path = TempFileGuard::new(key, "_qclient-key.pem")?;
            config
                .load_priv_key_from_pem_file(key_path.as_str("client key")?)
                .map_err(|err| {
                    Http3NativeError::Config(format!("invalid client private key PEM: {err}"))
                })?;
        }
        (Some(_), None) => {
            return Err(Http3NativeError::Config(
                "client certificate requires private key".into(),
            ));
        }
        (None, Some(_)) => {
            return Err(Http3NativeError::Config(
                "client private key requires certificate".into(),
            ));
        }
        (None, None) => {}
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
            .unwrap_or(FALLBACK_MAX_UDP_PAYLOAD as u32) as usize,
    );
    config.set_max_send_udp_payload_size(
        options
            .max_udp_payload_size
            .unwrap_or(FALLBACK_MAX_UDP_PAYLOAD as u32) as usize,
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

    apply_congestion_tuning(&mut config);

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
            runtime_mode: None,
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
            keepalive_interval_ms: None,
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

    #[test]
    fn test_runtime_mode_parse_fast() {
        assert_eq!(
            TransportRuntimeMode::parse(Some("fast")).unwrap(),
            TransportRuntimeMode::Fast
        );
        assert_eq!(
            TransportRuntimeMode::parse(None).unwrap(),
            TransportRuntimeMode::Fast,
            "None should default to Fast"
        );
    }

    #[test]
    fn test_runtime_mode_parse_portable() {
        assert_eq!(
            TransportRuntimeMode::parse(Some("portable")).unwrap(),
            TransportRuntimeMode::Portable
        );
    }

    #[test]
    fn test_runtime_mode_parse_invalid_rejects() {
        let err = TransportRuntimeMode::parse(Some("turbo")).unwrap_err();
        assert!(
            err.to_string().contains("invalid runtimeMode"),
            "error should mention invalid runtimeMode, got: {}",
            err
        );
    }

    #[test]
    fn test_client_auth_mode_none_with_ca_rejects() {
        let err = ClientAuthMode::parse(Some("none"), true).unwrap_err();
        assert!(
            err.to_string().contains("cannot be combined with ca"),
            "error should mention ca conflict, got: {}",
            err
        );
    }

    #[test]
    fn test_client_auth_mode_require_without_ca_rejects() {
        let err = ClientAuthMode::parse(Some("require"), false).unwrap_err();
        assert!(
            err.to_string().contains("requires ca"),
            "error should mention ca requirement, got: {}",
            err
        );
    }
}
