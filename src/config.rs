//! QUIC/TLS configuration builder that translates JS option objects into
//! `quiche::Config` instances for both server and client use.

#[cfg(feature = "os-runtime")]
use std::io::Write;
#[cfg(feature = "os-runtime")]
use std::path::PathBuf;

use crate::cid::{CidEncoding, parse_server_id_bytes};
use crate::error::Http3NativeError;
#[cfg(feature = "node-api")]
use napi_derive::napi;

#[cfg(feature = "node-api")]
type ByteBuf = napi::bindgen_prelude::Buffer;
#[cfg(not(feature = "node-api"))]
type ByteBuf = Vec<u8>;

/// Fallback DPLPMTUD probe ceiling used when path MTU auto-detection is
/// unavailable (unresolvable destination, unsupported platform, etc.).
///
/// 1472 = 1500 (Ethernet MTU) - 20 (IPv4) - 8 (UDP).  quiche probes from
/// 1200 up to this ceiling; on standard Ethernet the first probe succeeds
/// immediately.
pub(crate) const FALLBACK_MAX_UDP_PAYLOAD: usize = 1472;
const DEFAULT_INITIAL_MAX_DATA: u64 = 100_000_000;
const DEFAULT_INITIAL_MAX_STREAM_DATA: u64 = 2_000_000;
const DEFAULT_INITIAL_MAX_STREAMS_BIDI: u64 = 10_000;
const DEFAULT_INITIAL_MAX_STREAMS_UNI: u64 = 1_000;

/// Return the PMTUD probe ceiling for a connection to `peer`.
///
/// Queries the kernel routing table for the interface MTU on the path to
/// `peer` and caps at 16383 (quiche's max data packet size, limited by
/// 2-byte QUIC varint encoding).  On loopback this returns 16383; on
/// standard Ethernet it returns 1472; on jumbo frames ~8972. Loopback is
/// platform-dependent but should land near quiche's 16 KB data-packet cap.
///
/// Falls back to `FALLBACK_MAX_UDP_PAYLOAD` (1472) if the query fails, and
/// unconditionally when `os-runtime` is disabled (no socket to query the
/// route table with — e.g. a future wasm build; `1472` is the standard
/// Ethernet MTU minus IPv4/UDP headers, a safe default no real path is
/// likely to be below).
pub fn effective_pmtud_ceiling(peer: &std::net::SocketAddr) -> usize {
    #[cfg(feature = "os-runtime")]
    {
        crate::transport::socket::query_path_mtu(peer).unwrap_or(FALLBACK_MAX_UDP_PAYLOAD)
    }
    #[cfg(not(feature = "os-runtime"))]
    {
        let _ = peer;
        FALLBACK_MAX_UDP_PAYLOAD
    }
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

/// Keep quiche's receive-window autotuning aligned with the limits we
/// advertise to peers.  Without this, sustained streams can reach the
/// advertised connection credit before the default 24 MiB internal window has
/// issued a replacement MAX_DATA frame, and the peer validly closes with
/// FLOW_CONTROL_ERROR.
fn apply_flow_control_window_tuning(
    config: &mut quiche::Config,
    initial_max_data: u64,
    stream_windows: &[u64],
) {
    config.set_max_connection_window(initial_max_data);
    if let Some(max_stream_window) = stream_windows.iter().copied().max() {
        config.set_max_stream_window(max_stream_window);
    }
    config.set_use_initial_max_data_as_flow_control_win(true);
}

/// Write bytes to a temp file and return the path.
#[cfg(feature = "os-runtime")]
fn write_temp_file(data: &[u8], suffix: &str) -> Result<std::path::PathBuf, Http3NativeError> {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("http3_{}{}", std::process::id(), suffix));
    let mut f = std::fs::File::create(&path).map_err(Http3NativeError::Io)?;
    f.write_all(data).map_err(Http3NativeError::Io)?;
    Ok(path)
}

/// Loads TLS material from temp files (`quiche::Config`'s
/// `load_*_from_pem_file` only takes paths). Native-only: `os-runtime`
/// gated. The wasm/sans-IO alternative is the `*_in_memory` config
/// builders below, which use `quiche::Config::with_boring_ssl_ctx_builder`
/// + boring's in-memory `X509`/`PKey` loading instead (A2 task 4).
#[cfg(feature = "os-runtime")]
struct TempFileGuard {
    path: PathBuf,
}

#[cfg(feature = "os-runtime")]
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

#[cfg(feature = "os-runtime")]
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
    /// Client certificate policy. Default: `'require'` when `ca` is set,
    /// otherwise `'none'` (matches `JsQuicServerOptions::client_auth`'s
    /// existing semantics exactly — see `ClientAuthMode::parse`).
    ///
    /// Native's file-based [`Http3Config::new_server_quiche_config`] does
    /// **not** read this field (a pre-existing asymmetry: today's H3
    /// server has no client-certificate verification at all, unlike the
    /// raw QUIC server — mirrors the already-documented client-side
    /// asymmetry, "H3 client mTLS parity — deferred", in
    /// `docs/WASM_CLIENT_PLAN.md`'s decision log). Only the new
    /// [`Http3Config::new_server_quiche_config_in_memory`] (server-side
    /// wasm ABI work) reads it. Purely additive: existing native callers
    /// that never set this field are unaffected either way.
    pub client_auth: Option<String>,
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
    /// Disable quiche's send pacing. Sans-IO/wasm callers have no way to
    /// honor sub-ms `SendInfo` release times (A2 task 4) — plumbing exists
    /// here for that profile; native behavior is unaffected unless a
    /// caller explicitly opts in.
    pub disable_pacing: Option<bool>,
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
    /// Client certificate policy (see `JsServerOptions::client_auth`'s doc
    /// comment for why native's file-based server config builder doesn't
    /// read this — only the new in-memory / direct-call server surface
    /// does, added alongside server-side wasm ABI support).
    pub client_auth: ClientAuthMode,
}

impl Http3Config {
    #[cfg(feature = "os-runtime")]
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
        let initial_max_data = u64::from(
            options
                .initial_max_data
                .unwrap_or(DEFAULT_INITIAL_MAX_DATA as u32),
        );
        let initial_stream_data_bidi_local = u64::from(
            options
                .initial_max_stream_data_bidi_local
                .unwrap_or(DEFAULT_INITIAL_MAX_STREAM_DATA as u32),
        );
        let initial_stream_data_bidi_remote = DEFAULT_INITIAL_MAX_STREAM_DATA;
        let initial_stream_data_uni = DEFAULT_INITIAL_MAX_STREAM_DATA;
        config.set_initial_max_data(initial_max_data);
        config.set_initial_max_stream_data_bidi_local(initial_stream_data_bidi_local);
        config.set_initial_max_stream_data_bidi_remote(initial_stream_data_bidi_remote);
        config.set_initial_max_stream_data_uni(initial_stream_data_uni);
        config.set_initial_max_streams_bidi(u64::from(
            options
                .initial_max_streams_bidi
                .unwrap_or(DEFAULT_INITIAL_MAX_STREAMS_BIDI as u32),
        ));
        config.set_initial_max_streams_uni(DEFAULT_INITIAL_MAX_STREAMS_UNI);
        config.set_disable_active_migration(options.disable_active_migration.unwrap_or(true));

        if let Some(keys) = options.session_ticket_keys.as_ref() {
            config
                .set_ticket_key(keys)
                .map_err(Http3NativeError::Quiche)?;
        }

        apply_flow_control_window_tuning(
            &mut config,
            initial_max_data,
            &[
                initial_stream_data_bidi_local,
                initial_stream_data_bidi_remote,
                initial_stream_data_uni,
            ],
        );
        apply_congestion_tuning(&mut config);

        if options.enable_datagrams.unwrap_or(false) {
            config.enable_dgram(true, 1000, 1000);
        }

        if options.keylog.unwrap_or(false) {
            config.log_keys();
        }

        Ok(config)
    }

    #[cfg(feature = "os-runtime")]
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
        let initial_max_data = u64::from(
            options
                .initial_max_data
                .unwrap_or(DEFAULT_INITIAL_MAX_DATA as u32),
        );
        let initial_stream_data_bidi_local = u64::from(
            options
                .initial_max_stream_data_bidi_local
                .unwrap_or(DEFAULT_INITIAL_MAX_STREAM_DATA as u32),
        );
        let initial_stream_data_bidi_remote = DEFAULT_INITIAL_MAX_STREAM_DATA;
        let initial_stream_data_uni = DEFAULT_INITIAL_MAX_STREAM_DATA;
        config.set_initial_max_data(initial_max_data);
        config.set_initial_max_stream_data_bidi_local(initial_stream_data_bidi_local);
        config.set_initial_max_stream_data_bidi_remote(initial_stream_data_bidi_remote);
        config.set_initial_max_stream_data_uni(initial_stream_data_uni);
        config.set_initial_max_streams_bidi(u64::from(
            options
                .initial_max_streams_bidi
                .unwrap_or(DEFAULT_INITIAL_MAX_STREAMS_BIDI as u32),
        ));
        config.set_initial_max_streams_uni(DEFAULT_INITIAL_MAX_STREAMS_UNI);

        apply_flow_control_window_tuning(
            &mut config,
            initial_max_data,
            &[
                initial_stream_data_bidi_local,
                initial_stream_data_bidi_remote,
                initial_stream_data_uni,
            ],
        );
        apply_congestion_tuning(&mut config);

        if options.disable_pacing.unwrap_or(false) {
            config.enable_pacing(false);
        }

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

    /// In-memory alternative to [`Http3Config::new_client_quiche_config`]
    /// for a sans-IO / `wasm-abi` caller: loads `ca` via BoringSSL's
    /// `SslContextBuilder` instead of a temp file (A2 task 4). H3 client
    /// mTLS (`cert`/`key`) is out of scope here too — same asymmetry as
    /// the native file-based path (`docs/WASM_CLIENT_PLAN.md` decision
    /// log: "H3 client mTLS parity — deferred").
    pub fn new_client_quiche_config_in_memory(
        options: &JsClientOptions,
    ) -> Result<quiche::Config, Http3NativeError> {
        let mut tls = boring::ssl::SslContextBuilder::new(boring::ssl::SslMethod::tls())
            .map_err(|e| {
                Http3NativeError::Config(format!("boring SslContextBuilder::new failed: {e}"))
            })?;

        tls.set_verify(if options.reject_unauthorized.unwrap_or(true) {
            boring::ssl::SslVerifyMode::PEER
        } else {
            boring::ssl::SslVerifyMode::NONE
        });

        if let Some(ca) = options.ca.as_ref() {
            let ca_cert = boring::x509::X509::from_pem(ca)
                .map_err(|e| Http3NativeError::Config(format!("invalid ca PEM: {e}")))?;
            tls.cert_store_mut().add_cert(ca_cert).map_err(|e| {
                Http3NativeError::Config(format!("failed to add ca cert to store: {e}"))
            })?;
        }

        let mut config = quiche::Config::with_boring_ssl_ctx_builder(quiche::PROTOCOL_VERSION, tls)
            .map_err(Http3NativeError::Quiche)?;

        config
            .set_application_protos(quiche::h3::APPLICATION_PROTOCOL)
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
        let initial_max_data = u64::from(
            options
                .initial_max_data
                .unwrap_or(DEFAULT_INITIAL_MAX_DATA as u32),
        );
        let initial_stream_data_bidi_local = u64::from(
            options
                .initial_max_stream_data_bidi_local
                .unwrap_or(DEFAULT_INITIAL_MAX_STREAM_DATA as u32),
        );
        let initial_stream_data_bidi_remote = DEFAULT_INITIAL_MAX_STREAM_DATA;
        let initial_stream_data_uni = DEFAULT_INITIAL_MAX_STREAM_DATA;
        config.set_initial_max_data(initial_max_data);
        config.set_initial_max_stream_data_bidi_local(initial_stream_data_bidi_local);
        config.set_initial_max_stream_data_bidi_remote(initial_stream_data_bidi_remote);
        config.set_initial_max_stream_data_uni(initial_stream_data_uni);
        config.set_initial_max_streams_bidi(u64::from(
            options
                .initial_max_streams_bidi
                .unwrap_or(DEFAULT_INITIAL_MAX_STREAMS_BIDI as u32),
        ));
        config.set_initial_max_streams_uni(DEFAULT_INITIAL_MAX_STREAMS_UNI);

        apply_flow_control_window_tuning(
            &mut config,
            initial_max_data,
            &[
                initial_stream_data_bidi_local,
                initial_stream_data_bidi_remote,
                initial_stream_data_uni,
            ],
        );
        apply_congestion_tuning(&mut config);

        // No qlog / real socket in this profile: force disable pacing by
        // default unless the caller explicitly re-enables it, since a
        // sans-IO caller cannot honor sub-ms `SendInfo` release times
        // anyway (docs/WASM_CLIENT_PLAN.md §4.7).
        if !options.disable_pacing.is_some_and(|disable| !disable) {
            config.enable_pacing(false);
        }

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

    /// In-memory alternative to
    /// [`Http3Config::new_server_quiche_config`] for a sans-IO /
    /// `wasm-abi` server caller: loads `cert`/`key` (mandatory — a server
    /// cannot start without them, unlike a client's optional `ca`) and the
    /// optional `ca` (for client-certificate verification) via
    /// BoringSSL's `SslContextBuilder` instead of temp files, exactly
    /// mirroring [`Http3Config::new_client_quiche_config_in_memory`]'s
    /// established pattern.
    ///
    /// `clientAuth`/`ca` handling: this is a genuinely new capability, not
    /// a port of existing native behavior — see
    /// `JsServerOptions::client_auth`'s doc comment for why (native's
    /// file-based server builder has no client-certificate verification
    /// at all today). Mirrors `new_quic_server_config`'s
    /// `ClientAuthMode`-based `verify_peer` handling, which is the one
    /// existing native precedent for this option.
    pub fn new_server_quiche_config_in_memory(
        options: &JsServerOptions,
    ) -> Result<quiche::Config, Http3NativeError> {
        let mut tls = boring::ssl::SslContextBuilder::new(boring::ssl::SslMethod::tls())
            .map_err(|e| {
                Http3NativeError::Config(format!("boring SslContextBuilder::new failed: {e}"))
            })?;

        let cert = boring::x509::X509::from_pem(&options.cert)
            .map_err(|e| Http3NativeError::Config(format!("invalid server certificate PEM: {e}")))?;
        tls.set_certificate(&cert)
            .map_err(|e| Http3NativeError::Config(format!("failed to set server certificate: {e}")))?;
        let key = boring::pkey::PKey::private_key_from_pem(&options.key)
            .map_err(|e| Http3NativeError::Config(format!("invalid server private key PEM: {e}")))?;
        tls.set_private_key(&key)
            .map_err(|e| Http3NativeError::Config(format!("failed to set server private key: {e}")))?;

        let client_auth =
            ClientAuthMode::parse(options.client_auth.as_deref(), options.ca.is_some())?;
        if let Some(ca) = options.ca.as_ref() {
            let ca_cert = boring::x509::X509::from_pem(ca)
                .map_err(|e| Http3NativeError::Config(format!("invalid ca PEM: {e}")))?;
            tls.cert_store_mut().add_cert(ca_cert).map_err(|e| {
                Http3NativeError::Config(format!("failed to add ca cert to store: {e}"))
            })?;
        }
        tls.set_verify(if client_auth.verify_peer() {
            boring::ssl::SslVerifyMode::PEER
        } else {
            boring::ssl::SslVerifyMode::NONE
        });

        let mut config = quiche::Config::with_boring_ssl_ctx_builder(quiche::PROTOCOL_VERSION, tls)
            .map_err(Http3NativeError::Quiche)?;

        config
            .set_application_protos(quiche::h3::APPLICATION_PROTOCOL)
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
        let initial_max_data = u64::from(
            options
                .initial_max_data
                .unwrap_or(DEFAULT_INITIAL_MAX_DATA as u32),
        );
        let initial_stream_data_bidi_local = u64::from(
            options
                .initial_max_stream_data_bidi_local
                .unwrap_or(DEFAULT_INITIAL_MAX_STREAM_DATA as u32),
        );
        let initial_stream_data_bidi_remote = DEFAULT_INITIAL_MAX_STREAM_DATA;
        let initial_stream_data_uni = DEFAULT_INITIAL_MAX_STREAM_DATA;
        config.set_initial_max_data(initial_max_data);
        config.set_initial_max_stream_data_bidi_local(initial_stream_data_bidi_local);
        config.set_initial_max_stream_data_bidi_remote(initial_stream_data_bidi_remote);
        config.set_initial_max_stream_data_uni(initial_stream_data_uni);
        config.set_initial_max_streams_bidi(u64::from(
            options
                .initial_max_streams_bidi
                .unwrap_or(DEFAULT_INITIAL_MAX_STREAMS_BIDI as u32),
        ));
        config.set_initial_max_streams_uni(DEFAULT_INITIAL_MAX_STREAMS_UNI);
        config.set_disable_active_migration(options.disable_active_migration.unwrap_or(true));

        if let Some(keys) = options.session_ticket_keys.as_ref() {
            config
                .set_ticket_key(keys)
                .map_err(Http3NativeError::Quiche)?;
        }

        apply_flow_control_window_tuning(
            &mut config,
            initial_max_data,
            &[
                initial_stream_data_bidi_local,
                initial_stream_data_bidi_remote,
                initial_stream_data_uni,
            ],
        );
        apply_congestion_tuning(&mut config);

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

        let client_auth =
            ClientAuthMode::parse(options.client_auth.as_deref(), options.ca.is_some())?;

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
            client_auth,
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
    /// Disable quiche's send pacing (see `JsClientOptions::disable_pacing`).
    pub disable_pacing: Option<bool>,
}

fn alpn_to_bytes(protocols: &[String]) -> Vec<Vec<u8>> {
    protocols.iter().map(|p| p.as_bytes().to_vec()).collect()
}

fn alpn_refs(protos: &[Vec<u8>]) -> Vec<&[u8]> {
    protos.iter().map(Vec::as_slice).collect()
}

#[cfg(feature = "os-runtime")]
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
    let initial_max_data = u64::from(
        options
            .initial_max_data
            .unwrap_or(DEFAULT_INITIAL_MAX_DATA as u32),
    );
    let initial_stream_data_bidi_local = u64::from(
        options
            .initial_max_stream_data_bidi_local
            .unwrap_or(DEFAULT_INITIAL_MAX_STREAM_DATA as u32),
    );
    let initial_stream_data_bidi_remote = DEFAULT_INITIAL_MAX_STREAM_DATA;
    let initial_stream_data_uni = DEFAULT_INITIAL_MAX_STREAM_DATA;
    config.set_initial_max_data(initial_max_data);
    config.set_initial_max_stream_data_bidi_local(initial_stream_data_bidi_local);
    config.set_initial_max_stream_data_bidi_remote(initial_stream_data_bidi_remote);
    config.set_initial_max_stream_data_uni(initial_stream_data_uni);
    config.set_initial_max_streams_bidi(u64::from(
        options
            .initial_max_streams_bidi
            .unwrap_or(DEFAULT_INITIAL_MAX_STREAMS_BIDI as u32),
    ));
    config.set_initial_max_streams_uni(DEFAULT_INITIAL_MAX_STREAMS_UNI);
    config.set_disable_active_migration(options.disable_active_migration.unwrap_or(true));

    if let Some(keys) = options.session_ticket_keys.as_ref() {
        config
            .set_ticket_key(keys)
            .map_err(Http3NativeError::Quiche)?;
    }

    apply_flow_control_window_tuning(
        &mut config,
        initial_max_data,
        &[
            initial_stream_data_bidi_local,
            initial_stream_data_bidi_remote,
            initial_stream_data_uni,
        ],
    );
    apply_congestion_tuning(&mut config);

    if options.enable_datagrams.unwrap_or(false) {
        config.enable_dgram(true, 1000, 1000);
    }

    if options.keylog.unwrap_or(false) {
        config.log_keys();
    }

    Ok(config)
}

/// In-memory alternative to [`new_quic_server_config`] for a sans-IO /
/// `wasm-abi` server caller: loads `cert`/`key` (mandatory) and the
/// optional `ca`/`clientAuth` via BoringSSL's `SslContextBuilder` instead
/// of temp files. Unlike the H3 server's in-memory builder, this mirrors
/// an *existing* native precedent exactly — `new_quic_server_config`
/// already reads `client_auth` and calls `verify_peer` today.
pub fn new_quic_server_config_in_memory(
    options: &JsQuicServerOptions,
) -> Result<quiche::Config, Http3NativeError> {
    let mut tls = boring::ssl::SslContextBuilder::new(boring::ssl::SslMethod::tls())
        .map_err(|e| Http3NativeError::Config(format!("boring SslContextBuilder::new failed: {e}")))?;

    let cert = boring::x509::X509::from_pem(&options.cert)
        .map_err(|e| Http3NativeError::Config(format!("invalid server certificate PEM: {e}")))?;
    tls.set_certificate(&cert)
        .map_err(|e| Http3NativeError::Config(format!("failed to set server certificate: {e}")))?;
    let key = boring::pkey::PKey::private_key_from_pem(&options.key)
        .map_err(|e| Http3NativeError::Config(format!("invalid server private key PEM: {e}")))?;
    tls.set_private_key(&key)
        .map_err(|e| Http3NativeError::Config(format!("failed to set server private key: {e}")))?;

    let client_auth = ClientAuthMode::parse(options.client_auth.as_deref(), options.ca.is_some())?;
    if let Some(ca) = options.ca.as_ref() {
        let ca_cert = boring::x509::X509::from_pem(ca)
            .map_err(|e| Http3NativeError::Config(format!("invalid ca PEM: {e}")))?;
        tls.cert_store_mut()
            .add_cert(ca_cert)
            .map_err(|e| Http3NativeError::Config(format!("failed to add ca cert to store: {e}")))?;
    }
    tls.set_verify(if client_auth.verify_peer() {
        boring::ssl::SslVerifyMode::PEER
    } else {
        boring::ssl::SslVerifyMode::NONE
    });

    let mut config = quiche::Config::with_boring_ssl_ctx_builder(quiche::PROTOCOL_VERSION, tls)
        .map_err(Http3NativeError::Quiche)?;

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
    let initial_max_data = u64::from(
        options
            .initial_max_data
            .unwrap_or(DEFAULT_INITIAL_MAX_DATA as u32),
    );
    let initial_stream_data_bidi_local = u64::from(
        options
            .initial_max_stream_data_bidi_local
            .unwrap_or(DEFAULT_INITIAL_MAX_STREAM_DATA as u32),
    );
    let initial_stream_data_bidi_remote = DEFAULT_INITIAL_MAX_STREAM_DATA;
    let initial_stream_data_uni = DEFAULT_INITIAL_MAX_STREAM_DATA;
    config.set_initial_max_data(initial_max_data);
    config.set_initial_max_stream_data_bidi_local(initial_stream_data_bidi_local);
    config.set_initial_max_stream_data_bidi_remote(initial_stream_data_bidi_remote);
    config.set_initial_max_stream_data_uni(initial_stream_data_uni);
    config.set_initial_max_streams_bidi(u64::from(
        options
            .initial_max_streams_bidi
            .unwrap_or(DEFAULT_INITIAL_MAX_STREAMS_BIDI as u32),
    ));
    config.set_initial_max_streams_uni(DEFAULT_INITIAL_MAX_STREAMS_UNI);
    config.set_disable_active_migration(options.disable_active_migration.unwrap_or(true));

    if let Some(keys) = options.session_ticket_keys.as_ref() {
        config
            .set_ticket_key(keys)
            .map_err(Http3NativeError::Quiche)?;
    }

    apply_flow_control_window_tuning(
        &mut config,
        initial_max_data,
        &[
            initial_stream_data_bidi_local,
            initial_stream_data_bidi_remote,
            initial_stream_data_uni,
        ],
    );
    apply_congestion_tuning(&mut config);

    if options.enable_datagrams.unwrap_or(false) {
        config.enable_dgram(true, 1000, 1000);
    }

    if options.keylog.unwrap_or(false) {
        config.log_keys();
    }

    Ok(config)
}

#[cfg(feature = "os-runtime")]
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
    let initial_max_data = u64::from(
        options
            .initial_max_data
            .unwrap_or(DEFAULT_INITIAL_MAX_DATA as u32),
    );
    let initial_stream_data_bidi_local = u64::from(
        options
            .initial_max_stream_data_bidi_local
            .unwrap_or(DEFAULT_INITIAL_MAX_STREAM_DATA as u32),
    );
    let initial_stream_data_bidi_remote = DEFAULT_INITIAL_MAX_STREAM_DATA;
    let initial_stream_data_uni = DEFAULT_INITIAL_MAX_STREAM_DATA;
    config.set_initial_max_data(initial_max_data);
    config.set_initial_max_stream_data_bidi_local(initial_stream_data_bidi_local);
    config.set_initial_max_stream_data_bidi_remote(initial_stream_data_bidi_remote);
    config.set_initial_max_stream_data_uni(initial_stream_data_uni);
    config.set_initial_max_streams_bidi(u64::from(
        options
            .initial_max_streams_bidi
            .unwrap_or(DEFAULT_INITIAL_MAX_STREAMS_BIDI as u32),
    ));
    config.set_initial_max_streams_uni(DEFAULT_INITIAL_MAX_STREAMS_UNI);

    apply_flow_control_window_tuning(
        &mut config,
        initial_max_data,
        &[
            initial_stream_data_bidi_local,
            initial_stream_data_bidi_remote,
            initial_stream_data_uni,
        ],
    );
    apply_congestion_tuning(&mut config);

    if options.disable_pacing.unwrap_or(false) {
        config.enable_pacing(false);
    }

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

/// In-memory alternative to [`new_quic_client_config`] for a sans-IO /
/// `wasm-abi` caller: loads `ca`/`cert`/`key` PEM buffers via BoringSSL's
/// `SslContextBuilder` instead of writing them to temp files (A2 task 4;
/// O2 in `docs/WASM_CLIENT_PLAN.md`). The file-based path above stays
/// unchanged for native under `os-runtime`.
pub fn new_quic_client_config_in_memory(
    options: &JsQuicClientOptions,
) -> Result<quiche::Config, Http3NativeError> {
    let mut tls = boring::ssl::SslContextBuilder::new(boring::ssl::SslMethod::tls())
        .map_err(|e| Http3NativeError::Config(format!("boring SslContextBuilder::new failed: {e}")))?;

    tls.set_verify(if options.reject_unauthorized.unwrap_or(true) {
        boring::ssl::SslVerifyMode::PEER
    } else {
        boring::ssl::SslVerifyMode::NONE
    });

    match (options.cert.as_ref(), options.key.as_ref()) {
        (Some(cert), Some(key)) => {
            let cert = boring::x509::X509::from_pem(cert).map_err(|e| {
                Http3NativeError::Config(format!("invalid client certificate PEM: {e}"))
            })?;
            tls.set_certificate(&cert).map_err(|e| {
                Http3NativeError::Config(format!("failed to set client certificate: {e}"))
            })?;
            let key = boring::pkey::PKey::private_key_from_pem(key).map_err(|e| {
                Http3NativeError::Config(format!("invalid client private key PEM: {e}"))
            })?;
            tls.set_private_key(&key).map_err(|e| {
                Http3NativeError::Config(format!("failed to set client private key: {e}"))
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
        let ca_cert = boring::x509::X509::from_pem(ca)
            .map_err(|e| Http3NativeError::Config(format!("invalid ca PEM: {e}")))?;
        tls.cert_store_mut()
            .add_cert(ca_cert)
            .map_err(|e| Http3NativeError::Config(format!("failed to add ca cert to store: {e}")))?;
    }

    let mut config = quiche::Config::with_boring_ssl_ctx_builder(quiche::PROTOCOL_VERSION, tls)
        .map_err(Http3NativeError::Quiche)?;

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
    let initial_max_data = u64::from(
        options
            .initial_max_data
            .unwrap_or(DEFAULT_INITIAL_MAX_DATA as u32),
    );
    let initial_stream_data_bidi_local = u64::from(
        options
            .initial_max_stream_data_bidi_local
            .unwrap_or(DEFAULT_INITIAL_MAX_STREAM_DATA as u32),
    );
    let initial_stream_data_bidi_remote = DEFAULT_INITIAL_MAX_STREAM_DATA;
    let initial_stream_data_uni = DEFAULT_INITIAL_MAX_STREAM_DATA;
    config.set_initial_max_data(initial_max_data);
    config.set_initial_max_stream_data_bidi_local(initial_stream_data_bidi_local);
    config.set_initial_max_stream_data_bidi_remote(initial_stream_data_bidi_remote);
    config.set_initial_max_stream_data_uni(initial_stream_data_uni);
    config.set_initial_max_streams_bidi(u64::from(
        options
            .initial_max_streams_bidi
            .unwrap_or(DEFAULT_INITIAL_MAX_STREAMS_BIDI as u32),
    ));
    config.set_initial_max_streams_uni(DEFAULT_INITIAL_MAX_STREAMS_UNI);

    apply_flow_control_window_tuning(
        &mut config,
        initial_max_data,
        &[
            initial_stream_data_bidi_local,
            initial_stream_data_bidi_remote,
            initial_stream_data_uni,
        ],
    );
    apply_congestion_tuning(&mut config);

    // See the H3 in-memory profile's comment: default pacing off in this
    // profile unless the caller explicitly re-enables it.
    if !options.disable_pacing.is_some_and(|disable| !disable) {
        config.enable_pacing(false);
    }

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
            client_auth: None,
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

    // ── A2 task 4: in-memory (boring-backed) config builders ───────────

    fn generate_self_signed_pem() -> (Vec<u8>, Vec<u8>) {
        use rcgen::{CertificateParams, KeyPair};
        let key_pair = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).expect("keypair");
        let mut params = CertificateParams::new(vec!["localhost".into()]).expect("params");
        params.distinguished_name = rcgen::DistinguishedName::new();
        let cert = params.self_signed(&key_pair).expect("self-signed cert");
        (
            cert.pem().into_bytes(),
            key_pair.serialize_pem().into_bytes(),
        )
    }

    fn base_client_options() -> JsClientOptions {
        JsClientOptions {
            ca: None,
            reject_unauthorized: None,
            runtime_mode: None,
            max_idle_timeout_ms: None,
            max_udp_payload_size: None,
            initial_max_data: None,
            initial_max_stream_data_bidi_local: None,
            initial_max_streams_bidi: None,
            session_ticket: None,
            allow_0rtt: None,
            enable_datagrams: None,
            keylog: None,
            qlog_dir: None,
            qlog_level: None,
            disable_pacing: None,
        }
    }

    fn base_quic_client_options() -> JsQuicClientOptions {
        JsQuicClientOptions {
            ca: None,
            cert: None,
            key: None,
            reject_unauthorized: None,
            alpn: None,
            runtime_mode: None,
            max_idle_timeout_ms: None,
            max_udp_payload_size: None,
            initial_max_data: None,
            initial_max_stream_data_bidi_local: None,
            initial_max_streams_bidi: None,
            session_ticket: None,
            allow_0rtt: None,
            enable_datagrams: None,
            keylog: None,
            qlog_dir: None,
            qlog_level: None,
            disable_pacing: None,
        }
    }

    #[test]
    fn h3_in_memory_client_config_accepts_self_signed_ca() {
        let (ca_pem, _key_pem) = generate_self_signed_pem();
        let mut options = base_client_options();
        options.ca = Some(ca_pem.into());

        let config = Http3Config::new_client_quiche_config_in_memory(&options);
        assert!(
            config.is_ok(),
            "expected in-memory H3 client config to build: {:?}",
            config.err()
        );
    }

    #[test]
    fn h3_in_memory_client_config_works_without_ca() {
        let options = base_client_options();
        let config = Http3Config::new_client_quiche_config_in_memory(&options);
        assert!(config.is_ok(), "expected config with no ca to build");
    }

    #[test]
    fn quic_in_memory_client_config_accepts_self_signed_ca_and_mtls_cert() {
        let (ca_pem, _) = generate_self_signed_pem();
        let (client_cert_pem, client_key_pem) = generate_self_signed_pem();
        let mut options = base_quic_client_options();
        options.ca = Some(ca_pem.into());
        options.cert = Some(client_cert_pem.into());
        options.key = Some(client_key_pem.into());

        let config = new_quic_client_config_in_memory(&options);
        assert!(
            config.is_ok(),
            "expected in-memory QUIC client config to build: {:?}",
            config.err()
        );
    }

    #[test]
    fn quic_in_memory_client_config_rejects_cert_without_key() {
        let (client_cert_pem, _) = generate_self_signed_pem();
        let mut options = base_quic_client_options();
        options.cert = Some(client_cert_pem.into());

        // `quiche::Config` isn't `Debug`, so `Result::expect_err` (which
        // requires `T: Debug`) doesn't work here — match manually instead.
        let err = match new_quic_client_config_in_memory(&options) {
            Err(e) => e,
            Ok(_) => panic!("expected cert-without-key to be rejected"),
        };
        assert!(err.to_string().contains("requires private key"));
    }

    #[test]
    fn quic_in_memory_client_config_rejects_key_without_cert() {
        let (_, client_key_pem) = generate_self_signed_pem();
        let mut options = base_quic_client_options();
        options.key = Some(client_key_pem.into());

        let err = match new_quic_client_config_in_memory(&options) {
            Err(e) => e,
            Ok(_) => panic!("expected key-without-cert to be rejected"),
        };
        assert!(err.to_string().contains("requires certificate"));
    }

    #[test]
    fn quic_in_memory_client_config_defaults_build_successfully() {
        // No ca/cert/key at all — proves the pacing-off-by-default profile
        // (A2 task 4) and ALPN/tuning plumbing don't error on their own.
        let options = base_quic_client_options();
        let config = new_quic_client_config_in_memory(&options);
        assert!(config.is_ok(), "expected default config to build");
    }

    #[test]
    fn effective_pmtud_ceiling_returns_fallback_constant_without_os_runtime() {
        // Always exercises the `not(os-runtime)` branch's constant fallback
        // directly when `os-runtime` is off; under `os-runtime` this just
        // confirms the function still returns a sane, non-zero ceiling.
        let peer: std::net::SocketAddr = "127.0.0.1:4433".parse().expect("valid addr");
        let ceiling = effective_pmtud_ceiling(&peer);
        assert!(ceiling > 0);
        #[cfg(not(feature = "os-runtime"))]
        assert_eq!(ceiling, FALLBACK_MAX_UDP_PAYLOAD);
    }

    // ── Server-side wasm ABI: in-memory server config builders ─────────

    fn base_server_options_in_memory(cert_pem: Vec<u8>, key_pem: Vec<u8>) -> JsServerOptions {
        JsServerOptions {
            key: key_pem.into(),
            cert: cert_pem.into(),
            ca: None,
            client_auth: None,
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
        }
    }

    fn base_quic_server_options_in_memory(cert_pem: Vec<u8>, key_pem: Vec<u8>) -> JsQuicServerOptions {
        JsQuicServerOptions {
            key: key_pem.into(),
            cert: cert_pem.into(),
            ca: None,
            client_auth: None,
            alpn: None,
            runtime_mode: None,
            max_idle_timeout_ms: None,
            max_udp_payload_size: None,
            initial_max_data: None,
            initial_max_stream_data_bidi_local: None,
            initial_max_streams_bidi: None,
            disable_active_migration: None,
            enable_datagrams: None,
            max_connections: None,
            disable_retry: None,
            qlog_dir: None,
            qlog_level: None,
            session_ticket_keys: None,
            keylog: None,
        }
    }

    #[test]
    fn h3_in_memory_server_config_builds_with_mandatory_cert_and_key() {
        let (cert_pem, key_pem) = generate_self_signed_pem();
        let options = base_server_options_in_memory(cert_pem, key_pem);
        let config = Http3Config::new_server_quiche_config_in_memory(&options);
        assert!(
            config.is_ok(),
            "expected in-memory H3 server config to build: {:?}",
            config.err()
        );
    }

    #[test]
    fn h3_in_memory_server_config_rejects_garbage_cert() {
        let (_, key_pem) = generate_self_signed_pem();
        let options = base_server_options_in_memory(b"not a cert".to_vec(), key_pem);
        let err = match Http3Config::new_server_quiche_config_in_memory(&options) {
            Err(e) => e,
            Ok(_) => panic!("expected invalid cert PEM to be rejected"),
        };
        assert!(err.to_string().contains("invalid server certificate"));
    }

    #[test]
    fn h3_in_memory_server_config_with_ca_and_default_client_auth_requires_verification() {
        // Default (client_auth: None) + ca present => Require (mirrors
        // `ClientAuthMode::parse`'s documented default), and the config
        // still builds successfully — proves the `set_verify(PEER)` path
        // doesn't error out on its own.
        let (cert_pem, key_pem) = generate_self_signed_pem();
        let (ca_pem, _) = generate_self_signed_pem();
        let mut options = base_server_options_in_memory(cert_pem, key_pem);
        options.ca = Some(ca_pem.into());
        let config = Http3Config::new_server_quiche_config_in_memory(&options);
        assert!(
            config.is_ok(),
            "expected in-memory H3 server config with ca to build: {:?}",
            config.err()
        );
    }

    #[test]
    fn h3_in_memory_server_config_rejects_client_auth_none_with_ca() {
        let (cert_pem, key_pem) = generate_self_signed_pem();
        let (ca_pem, _) = generate_self_signed_pem();
        let mut options = base_server_options_in_memory(cert_pem, key_pem);
        options.ca = Some(ca_pem.into());
        options.client_auth = Some("none".to_string());
        let err = match Http3Config::new_server_quiche_config_in_memory(&options) {
            Err(e) => e,
            Ok(_) => panic!("expected clientAuth='none' + ca to be rejected"),
        };
        assert!(err.to_string().contains("cannot be combined with ca"));
    }

    #[test]
    fn quic_in_memory_server_config_builds_with_mandatory_cert_and_key() {
        let (cert_pem, key_pem) = generate_self_signed_pem();
        let options = base_quic_server_options_in_memory(cert_pem, key_pem);
        let config = new_quic_server_config_in_memory(&options);
        assert!(
            config.is_ok(),
            "expected in-memory QUIC server config to build: {:?}",
            config.err()
        );
    }

    #[test]
    fn quic_in_memory_server_config_accepts_request_client_auth_with_ca() {
        let (cert_pem, key_pem) = generate_self_signed_pem();
        let (ca_pem, _) = generate_self_signed_pem();
        let mut options = base_quic_server_options_in_memory(cert_pem, key_pem);
        options.ca = Some(ca_pem.into());
        options.client_auth = Some("request".to_string());
        let config = new_quic_server_config_in_memory(&options);
        assert!(
            config.is_ok(),
            "expected request-mode client auth to build: {:?}",
            config.err()
        );
    }

    #[test]
    fn quic_in_memory_server_config_rejects_require_without_ca() {
        let (cert_pem, key_pem) = generate_self_signed_pem();
        let mut options = base_quic_server_options_in_memory(cert_pem, key_pem);
        options.client_auth = Some("require".to_string());
        let err = match new_quic_server_config_in_memory(&options) {
            Err(e) => e,
            Ok(_) => panic!("expected clientAuth='require' without ca to be rejected"),
        };
        assert!(err.to_string().contains("requires ca"));
    }
}
