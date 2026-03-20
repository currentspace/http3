use std::net::SocketAddr;

use crate::config::TransportRuntimeMode;

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ClientSocketStrategy {
    SharedPerFamily,
    Dedicated,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum SharedClientWorkerKey {
    V4 {
        runtime_mode: TransportRuntimeMode,
    },
    V6 {
        runtime_mode: TransportRuntimeMode,
    },
}

#[cfg(target_os = "macos")]
pub(crate) fn default_h3_client_socket_strategy(
    _runtime_mode: TransportRuntimeMode,
) -> ClientSocketStrategy {
    ClientSocketStrategy::SharedPerFamily
}

#[cfg(target_os = "linux")]
pub(crate) fn default_h3_client_socket_strategy(
    _runtime_mode: TransportRuntimeMode,
) -> ClientSocketStrategy {
    ClientSocketStrategy::SharedPerFamily
}

#[cfg(target_os = "macos")]
pub(crate) fn default_quic_client_socket_strategy(
    _runtime_mode: TransportRuntimeMode,
) -> ClientSocketStrategy {
    ClientSocketStrategy::SharedPerFamily
}

#[cfg(target_os = "linux")]
pub(crate) fn default_quic_client_socket_strategy(
    _runtime_mode: TransportRuntimeMode,
) -> ClientSocketStrategy {
    // Each client gets its own socket + io_uring instance.  Sharing a single
    // UDP socket across multiple QUIC connections adds head-of-line blocking
    // in the io_uring send path and doesn't match real-world usage where each
    // client session owns its own socket.
    ClientSocketStrategy::Dedicated
}

pub(crate) fn shared_client_worker_key(
    server_addr: SocketAddr,
    runtime_mode: TransportRuntimeMode,
) -> SharedClientWorkerKey {
    match server_addr {
        SocketAddr::V4(_) => SharedClientWorkerKey::V4 { runtime_mode },
        SocketAddr::V6(_) => SharedClientWorkerKey::V6 { runtime_mode },
    }
}

pub(crate) fn shared_client_bind_addr(server_addr: SocketAddr) -> SocketAddr {
    match server_addr {
        SocketAddr::V4(_) => SocketAddr::from(([0, 0, 0, 0], 0)),
        SocketAddr::V6(_) => SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 0], 0)),
    }
}
