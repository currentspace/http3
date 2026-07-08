//! Always-compiled datagram value types shared between the OS transport
//! drivers (`src/transport/`, gated behind the `os-runtime` feature) and the
//! sans-IO protocol core (`ProtocolHandler` in `src/event_loop.rs`, the
//! client handlers in `worker.rs`/`quic_worker.rs`, always compiled).
//!
//! These are plain value types with no socket/thread dependency, so they
//! live outside `src/transport/` and stay available even when `os-runtime`
//! is disabled (e.g. a `wasm-abi` build) — `ProtocolHandler::process_packet`
//! / `flush_sends` reference `TxDatagram` in their signatures and must keep
//! compiling either way. `src/transport/mod.rs` re-exports these so existing
//! `use crate::transport::{Driver, TxDatagram}`-style imports inside
//! OS-runtime-only code keep working unchanged.

use std::net::SocketAddr;

/// A completed received UDP datagram. Owned by the caller.
pub struct RxDatagram {
    pub data: Vec<u8>,
    pub peer: SocketAddr,
    /// Local address this packet was received on (from IP_PKTINFO cmsg).
    pub local: SocketAddr,
    /// When `Some`, this buffer contains multiple GRO-coalesced segments of
    /// this size (last segment may be shorter). The event loop splits them
    /// before passing to `process_packet`.
    pub segment_size: Option<u16>,
}

/// A transmit request. Ownership transfers to the driver.
pub struct TxDatagram {
    data: Vec<u8>,
    payload_len: usize,
    pub to: SocketAddr,
    /// Quiche's negotiated max_send_udp_payload_size for this connection
    /// at the moment the packet was emitted, used by the GSO grouper to
    /// cap (or extend) coalescing per real PMTU instead of the hardcoded
    /// `GSO_MAX_SEGMENT` Ethernet default. Audit finding #19. `None` =
    /// caller didn't supply a hint, fall back to the Ethernet cap.
    pub max_segment_size: Option<u16>,
}

impl TxDatagram {
    pub fn new(
        data: Vec<u8>,
        payload_len: usize,
        to: SocketAddr,
        max_segment_size: Option<u16>,
    ) -> Self {
        assert!(
            payload_len <= data.len(),
            "payload length must fit in the backing buffer"
        );
        Self {
            data,
            payload_len,
            to,
            max_segment_size,
        }
    }

    pub fn from_payload(data: Vec<u8>, to: SocketAddr, max_segment_size: Option<u16>) -> Self {
        let payload_len = data.len();
        Self::new(data, payload_len, to, max_segment_size)
    }

    pub fn payload(&self) -> &[u8] {
        &self.data[..self.payload_len]
    }

    pub fn payload_len(&self) -> usize {
        self.payload_len
    }

    pub fn into_recycle_buffer(self) -> Vec<u8> {
        self.data
    }
}

/// Outcome of a single `Driver::poll()` cycle.
pub struct PollOutcome {
    /// Completed receive operations since last poll.
    pub rx: Vec<RxDatagram>,
    /// Cross-thread waker fired — drain command channel.
    pub woken: bool,
    /// Deadline reached or timeout expired — process protocol timers.
    pub timer_expired: bool,
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeDriverKind {
    Kqueue,
    IoUring,
    Poll,
    Mock,
}

impl RuntimeDriverKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Kqueue => "kqueue",
            Self::IoUring => "io_uring",
            Self::Poll => "poll",
            Self::Mock => "mock",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tx_datagram_payload_len_preserves_backing_buffer() {
        let addr = SocketAddr::from(([127, 0, 0, 1], 4433));
        let pkt = TxDatagram::new(vec![1, 2, 3, 9, 9], 3, addr, None);

        assert_eq!(pkt.payload(), &[1, 2, 3]);
        assert_eq!(pkt.payload_len(), 3);
        assert_eq!(pkt.into_recycle_buffer().len(), 5);
    }

    #[test]
    fn test_runtime_driver_kind_as_str() {
        assert_eq!(RuntimeDriverKind::Kqueue.as_str(), "kqueue");
        assert_eq!(RuntimeDriverKind::IoUring.as_str(), "io_uring");
        assert_eq!(RuntimeDriverKind::Poll.as_str(), "poll");
        assert_eq!(RuntimeDriverKind::Mock.as_str(), "mock");
    }
}
