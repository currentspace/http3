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
    V4 { runtime_mode: TransportRuntimeMode },
    V6 { runtime_mode: TransportRuntimeMode },
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
    // Each H3 client gets its own socket + io_uring instance, matching
    // the QUIC client strategy.  Shared workers bottleneck under high
    // concurrent connection counts (50+ TLS handshakes on one thread).
    ClientSocketStrategy::Dedicated
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

    #[test]
    fn test_shared_client_worker_key_v4() {
        let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 443));
        let mode = TransportRuntimeMode::default();
        let key = shared_client_worker_key(addr, mode);
        assert_eq!(key, SharedClientWorkerKey::V4 { runtime_mode: mode });
    }

    #[test]
    fn test_shared_client_worker_key_v6() {
        let addr = SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::LOCALHOST, 443, 0, 0));
        let mode = TransportRuntimeMode::default();
        let key = shared_client_worker_key(addr, mode);
        assert_eq!(key, SharedClientWorkerKey::V6 { runtime_mode: mode });
    }

    #[test]
    fn test_shared_client_bind_addr_v4() {
        let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(93, 184, 216, 34), 443));
        let bind = shared_client_bind_addr(addr);
        assert_eq!(bind, SocketAddr::from(([0, 0, 0, 0], 0)));
    }

    #[test]
    fn test_shared_client_bind_addr_v6() {
        let addr = SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::LOCALHOST, 443, 0, 0));
        let bind = shared_client_bind_addr(addr);
        assert_eq!(bind, SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 0], 0)));
    }

    #[test]
    fn test_default_strategy() {
        let mode = TransportRuntimeMode::default();
        let strategy = default_h3_client_socket_strategy(mode);
        #[cfg(target_os = "macos")]
        assert_eq!(strategy, ClientSocketStrategy::SharedPerFamily);
        #[cfg(target_os = "linux")]
        assert_eq!(strategy, ClientSocketStrategy::Dedicated);
    }
}
