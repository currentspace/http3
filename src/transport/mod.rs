//! Platform I/O driver abstraction for UDP socket polling and sends.
//!
//! Provides the [`Driver`] trait that wraps platform-specific I/O multiplexing:
//! - macOS: `KqueueDriver` via `nix::sys::event` (EVFILT_READ/WRITE/USER)
//! - Linux: `IoUringDriver` via the `io-uring` crate (recvmsg/sendmsg CQEs)

use std::io;
use std::net::SocketAddr;
use std::time::Instant;

use crate::config::TransportRuntimeMode;
use crate::error::Http3NativeError;
use crate::reactor_metrics;

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
    pub data: Vec<u8>,
    pub to: SocketAddr,
}

/// A batch of same-size packets to the same peer, coalesced for UDP GSO.
#[cfg(target_os = "linux")]
pub(crate) struct GsoBatch {
    pub data: Vec<u8>,
    pub to: SocketAddr,
    pub segment_size: u16,
}

/// Group consecutive same-(destination, packet-size) packets into GSO batches.
/// Packets within each batch are concatenated; the kernel segments them using
/// the `UDP_SEGMENT` cmsg.  Max 64 segments per batch (kernel limit).
/// Maximum total payload per GSO batch. The kernel rejects sendmsg with
/// UDP_SEGMENT when the iov exceeds 65535 bytes (max UDP payload).
#[cfg(target_os = "linux")]
const GSO_MAX_PAYLOAD: usize = 65535;

/// Maximum segment size for GSO batching. Segments larger than this are not
/// coalesced because:
/// 1. Large GSO segments (e.g. 4140 bytes) can trigger EMSGSIZE if the kernel
///    considers them exceeding the path MTU.
/// 2. On loopback, the receiver's io_uring multishot recvmsg may not deliver
///    the UDP_GRO cmsg for coalesced large segments, preventing the event loop
///    from splitting them — quiche receives an oversized blob it can't parse.
/// 1472 = 1500 (Ethernet MTU) - 20 (IPv4) - 8 (UDP).
#[cfg(target_os = "linux")]
const GSO_MAX_SEGMENT: usize = 1472;

#[cfg(target_os = "linux")]
pub(crate) fn group_for_gso(packets: Vec<TxDatagram>) -> Vec<GsoBatch> {
    let mut batches: Vec<GsoBatch> = Vec::new();
    for pkt in packets {
        let seg_size = pkt.data.len() as u16;
        if let Some(last) = batches.last_mut() {
            if last.to == pkt.to
                && last.segment_size == seg_size
                && (seg_size as usize) <= GSO_MAX_SEGMENT
                && (last.data.len() / seg_size as usize) < 64
                && last.data.len() + pkt.data.len() <= GSO_MAX_PAYLOAD
            {
                last.data.extend_from_slice(&pkt.data);
                continue;
            }
        }
        batches.push(GsoBatch {
            data: pkt.data,
            to: pkt.to,
            segment_size: seg_size,
        });
    }
    batches
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

/// Platform I/O driver.
///
/// On macOS (kqueue): readiness-based. poll() internally does kevent() then
/// recv_from loop, wrapping results as RxDatagram. submit_sends() does send_to
/// immediately, queuing WouldBlock packets for retry on next poll().
///
/// On Linux (io_uring): completion-based. poll() processes CQEs from
/// pre-submitted recvmsg SQEs, returning completed RxDatagram objects.
/// submit_sends() builds sendmsg SQEs with owned stable-address buffers.
pub trait Driver: Sized {
    type Waker: DriverWaker;

    /// Wrap an existing nonblocking `UdpSocket`. Returns `(driver, waker)`.
    fn new(socket: std::net::UdpSocket) -> io::Result<(Self, Self::Waker)>;

    /// Block until: datagrams received, waker fired, or deadline reached.
    /// If deadline is `None`, uses a 100ms default timeout.
    fn poll(&mut self, deadline: Option<Instant>) -> io::Result<PollOutcome>;

    /// Submit outbound datagrams. Ownership of each `TxDatagram` transfers
    /// to the driver. Packets that cannot be sent immediately are queued.
    fn submit_sends(&mut self, packets: Vec<TxDatagram>) -> io::Result<()>;

    /// Number of TX operations still queued (unsent due to `WouldBlock`).
    fn pending_tx_count(&self) -> usize;

    /// Drain recycled TX buffers from completed sends.
    /// Returned buffers can be checked back into a `BufferPool`.
    fn drain_recycled_tx(&mut self) -> Vec<Vec<u8>>;

    /// Socket's bound local address.
    #[allow(dead_code)]
    fn local_addr(&self) -> io::Result<SocketAddr>;

    /// Concrete runtime driver backing this instance.
    fn driver_kind(&self) -> RuntimeDriverKind;

    /// Return consumed RX buffers for reuse by the driver's receive path.
    /// Default no-op; drivers that pool RX buffers override this.
    fn recycle_rx_buffers(&mut self, _buffers: Vec<Vec<u8>>) {}
}

/// Cross-thread wake handle. Clone + Send + Sync.
pub trait DriverWaker: Send + Sync + Clone + 'static {
    fn wake(&self) -> io::Result<()>;
}

/// Type-erased waker for handle structs that don't know the concrete driver.
pub trait ErasedWaker: Send + Sync {
    fn wake(&self) -> io::Result<()>;
}

impl<W: DriverWaker> ErasedWaker for W {
    fn wake(&self) -> io::Result<()> {
        DriverWaker::wake(self)
    }
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

pub(crate) mod socket;
#[cfg(test)]
mod socket_tests;

#[cfg(feature = "bench-internals")]
pub mod mock;
#[cfg(not(feature = "bench-internals"))]
pub(crate) mod mock;

// ── Platform driver selection ───────────────────────────────────────

#[cfg(target_os = "macos")]
mod kqueue;

#[cfg(all(target_os = "linux", feature = "bench-internals"))]
pub mod io_uring;
#[cfg(all(target_os = "linux", not(feature = "bench-internals")))]
mod io_uring;

#[cfg(all(target_os = "linux", feature = "bench-internals"))]
pub mod poll;
#[cfg(all(target_os = "linux", not(feature = "bench-internals")))]
mod poll;

#[cfg(target_os = "macos")]
pub(crate) type PlatformDriver = kqueue::KqueueDriver;

#[cfg(target_os = "macos")]
pub(crate) type PlatformWaker = kqueue::KqueueWaker;

#[cfg(target_os = "linux")]
pub(crate) enum PlatformDriver {
    IoUring(io_uring::IoUringDriver),
    Poll(poll::PollDriver),
}

#[cfg(target_os = "linux")]
#[derive(Clone)]
pub(crate) enum PlatformWaker {
    IoUring(io_uring::IoUringWaker),
    Poll(poll::PollWaker),
}

#[cfg(target_os = "macos")]
pub(crate) fn create_platform_driver(
    socket: std::net::UdpSocket,
    _runtime_mode: TransportRuntimeMode,
) -> Result<(PlatformDriver, PlatformWaker), Http3NativeError> {
    reactor_metrics::record_driver_setup_attempt(RuntimeDriverKind::Kqueue);
    match kqueue::KqueueDriver::new(socket) {
        Ok((driver, waker)) => {
            reactor_metrics::record_driver_setup_success(RuntimeDriverKind::Kqueue);
            Ok((driver, waker))
        }
        Err(error) => {
            reactor_metrics::record_driver_setup_failure(RuntimeDriverKind::Kqueue);
            Err(Http3NativeError::Io(error))
        }
    }
}

#[cfg(target_os = "linux")]
fn transport_error_to_io(err: Http3NativeError) -> io::Error {
    match err {
        Http3NativeError::Io(error) => error,
        Http3NativeError::FastPathUnavailable { source, .. } => source,
        Http3NativeError::RuntimeIo { source, .. } => source,
        other => io::Error::new(io::ErrorKind::Other, other.to_string()),
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn create_platform_driver(
    socket: std::net::UdpSocket,
    runtime_mode: TransportRuntimeMode,
) -> Result<(PlatformDriver, PlatformWaker), Http3NativeError> {
    match runtime_mode {
        TransportRuntimeMode::Fast => {
            reactor_metrics::record_driver_setup_attempt(RuntimeDriverKind::IoUring);
            // Clone the socket before io_uring takes ownership — enables
            // fallback to poll if io_uring setup fails (e.g. ENOMEM).
            let fallback_socket = socket.try_clone().ok();
            match io_uring::IoUringDriver::new(socket) {
                Ok((driver, waker)) => {
                    reactor_metrics::record_driver_setup_success(RuntimeDriverKind::IoUring);
                    Ok((
                        PlatformDriver::IoUring(driver),
                        PlatformWaker::IoUring(waker),
                    ))
                }
                Err(error) => {
                    reactor_metrics::record_driver_setup_failure(RuntimeDriverKind::IoUring);
                    match error.raw_os_error() {
                        Some(libc::EPERM) | Some(libc::EACCES) | Some(libc::ENOSYS)
                        | Some(libc::ENOMEM) => {
                            // Auto-fallback to poll driver
                            if let Some(sock) = fallback_socket {
                                log::warn!(
                                    "io_uring setup failed ({error}), falling back to poll"
                                );
                                reactor_metrics::record_driver_setup_attempt(
                                    RuntimeDriverKind::Poll,
                                );
                                match poll::PollDriver::new(sock) {
                                    Ok((driver, waker)) => {
                                        reactor_metrics::record_driver_setup_success(
                                            RuntimeDriverKind::Poll,
                                        );
                                        return Ok((
                                            PlatformDriver::Poll(driver),
                                            PlatformWaker::Poll(waker),
                                        ));
                                    }
                                    Err(poll_error) => {
                                        reactor_metrics::record_driver_setup_failure(
                                            RuntimeDriverKind::Poll,
                                        );
                                        return Err(Http3NativeError::Io(poll_error));
                                    }
                                }
                            }
                            Err(Http3NativeError::fast_path_unavailable(
                                "io_uring",
                                "io_uring_setup",
                                error,
                            ))
                        }
                        _ => Err(Http3NativeError::Io(error)),
                    }
                }
            }
        }
        TransportRuntimeMode::Portable => {
            reactor_metrics::record_driver_setup_attempt(RuntimeDriverKind::Poll);
            match poll::PollDriver::new(socket) {
                Ok((driver, waker)) => {
                    reactor_metrics::record_driver_setup_success(RuntimeDriverKind::Poll);
                    Ok((PlatformDriver::Poll(driver), PlatformWaker::Poll(waker)))
                }
                Err(error) => {
                    reactor_metrics::record_driver_setup_failure(RuntimeDriverKind::Poll);
                    Err(Http3NativeError::Io(error))
                }
            }
        }
    }
}

pub(crate) fn prepare_client_platform_driver(
    bind_addr: SocketAddr,
    runtime_mode: TransportRuntimeMode,
) -> Result<(PlatformDriver, PlatformWaker, SocketAddr), Http3NativeError> {
    let socket = std::net::UdpSocket::bind(bind_addr).map_err(Http3NativeError::Io)?;
    socket.set_nonblocking(true).map_err(Http3NativeError::Io)?;
    let _ = socket::set_socket_buffers(&socket, 2 * 1024 * 1024);
    let local_addr = socket.local_addr().map_err(Http3NativeError::Io)?;
    let (driver, waker) = create_platform_driver(socket, runtime_mode)?;
    Ok((driver, waker, local_addr))
}

#[cfg(target_os = "linux")]
impl Driver for PlatformDriver {
    type Waker = PlatformWaker;

    fn new(socket: std::net::UdpSocket) -> io::Result<(Self, Self::Waker)> {
        create_platform_driver(socket, TransportRuntimeMode::Fast).map_err(transport_error_to_io)
    }

    fn poll(&mut self, deadline: Option<Instant>) -> io::Result<PollOutcome> {
        match self {
            Self::IoUring(driver) => driver.poll(deadline),
            Self::Poll(driver) => driver.poll(deadline),
        }
    }

    fn submit_sends(&mut self, packets: Vec<TxDatagram>) -> io::Result<()> {
        match self {
            Self::IoUring(driver) => driver.submit_sends(packets),
            Self::Poll(driver) => driver.submit_sends(packets),
        }
    }

    fn pending_tx_count(&self) -> usize {
        match self {
            Self::IoUring(driver) => driver.pending_tx_count(),
            Self::Poll(driver) => driver.pending_tx_count(),
        }
    }

    fn drain_recycled_tx(&mut self) -> Vec<Vec<u8>> {
        match self {
            Self::IoUring(driver) => driver.drain_recycled_tx(),
            Self::Poll(driver) => driver.drain_recycled_tx(),
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        match self {
            Self::IoUring(driver) => driver.local_addr(),
            Self::Poll(driver) => driver.local_addr(),
        }
    }

    fn driver_kind(&self) -> RuntimeDriverKind {
        match self {
            Self::IoUring(driver) => driver.driver_kind(),
            Self::Poll(driver) => driver.driver_kind(),
        }
    }

    fn recycle_rx_buffers(&mut self, buffers: Vec<Vec<u8>>) {
        match self {
            Self::IoUring(driver) => driver.recycle_rx_buffers(buffers),
            Self::Poll(driver) => driver.recycle_rx_buffers(buffers),
        }
    }
}

#[cfg(target_os = "linux")]
impl DriverWaker for PlatformWaker {
    fn wake(&self) -> io::Result<()> {
        match self {
            Self::IoUring(waker) => DriverWaker::wake(waker),
            Self::Poll(waker) => DriverWaker::wake(waker),
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
compile_error!("Only macOS (kqueue) and Linux (io_uring) are supported");

#[cfg(test)]
#[cfg(target_os = "linux")]
mod tests {
    use super::*;

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    fn pkt(size: usize, port: u16) -> TxDatagram {
        TxDatagram {
            data: vec![0xAB; size],
            to: addr(port),
        }
    }

    #[test]
    fn test_gso_empty_input() {
        let batches = group_for_gso(vec![]);
        assert!(batches.is_empty());
    }

    #[test]
    fn test_gso_single_packet() {
        let batches = group_for_gso(vec![pkt(1200, 4433)]);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].data.len(), 1200);
        assert_eq!(batches[0].segment_size, 1200);
        assert_eq!(batches[0].to, addr(4433));
    }

    #[test]
    fn test_gso_same_dest_same_size() {
        let packets = vec![pkt(1200, 4433), pkt(1200, 4433), pkt(1200, 4433)];
        let batches = group_for_gso(packets);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].data.len(), 1200 * 3);
        assert_eq!(batches[0].segment_size, 1200);
        assert_eq!(batches[0].to, addr(4433));
    }

    #[test]
    fn test_gso_different_dest_splits() {
        let packets = vec![pkt(1200, 4433), pkt(1200, 4434), pkt(1200, 4433)];
        let batches = group_for_gso(packets);
        assert_eq!(batches.len(), 3);
        assert_eq!(batches[0].to, addr(4433));
        assert_eq!(batches[1].to, addr(4434));
        assert_eq!(batches[2].to, addr(4433));
    }

    #[test]
    fn test_gso_different_size_splits() {
        let packets = vec![pkt(1200, 4433), pkt(800, 4433), pkt(1200, 4433)];
        let batches = group_for_gso(packets);
        assert_eq!(batches.len(), 3);
        assert_eq!(batches[0].segment_size, 1200);
        assert_eq!(batches[1].segment_size, 800);
        assert_eq!(batches[2].segment_size, 1200);
    }

    #[test]
    fn test_gso_max_64_segments() {
        // 64 same-size packets to the same dest should coalesce into one batch.
        let mut packets: Vec<TxDatagram> = (0..64).map(|_| pkt(100, 4433)).collect();
        // The 65th packet must start a new batch.
        packets.push(pkt(100, 4433));

        let batches = group_for_gso(packets);
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].data.len(), 100 * 64);
        assert_eq!(batches[0].segment_size, 100);
        assert_eq!(batches[1].data.len(), 100);
        assert_eq!(batches[1].segment_size, 100);
    }

    #[test]
    fn test_gso_max_payload_boundary() {
        // Pick a segment size that divides evenly into the payload limit region.
        // With segment_size = 1000, we can fit floor(65535 / 1000) = 65 segments
        // but max segments is 64, so the segment cap fires first.
        // Use segment_size = 1100: floor(65535 / 1100) = 59 fit, 60th would
        // push total to 66000 > 65535, so batch splits at 59.
        let seg = 1100;
        let max_in_batch = GSO_MAX_PAYLOAD / seg; // 59
        assert_eq!(max_in_batch, 59);

        let mut packets: Vec<TxDatagram> =
            (0..max_in_batch).map(|_| pkt(seg, 4433)).collect();
        // One more packet should start a new batch.
        packets.push(pkt(seg, 4433));

        let batches = group_for_gso(packets);
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].data.len(), seg * max_in_batch);
        assert_eq!(batches[1].data.len(), seg);
    }

    #[test]
    fn test_gso_large_segment_no_coalesce() {
        // Segments larger than GSO_MAX_SEGMENT (1472) must not be coalesced.
        let big = GSO_MAX_SEGMENT + 1; // 1473
        let packets = vec![pkt(big, 4433), pkt(big, 4433)];
        let batches = group_for_gso(packets);
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].data.len(), big);
        assert_eq!(batches[1].data.len(), big);
    }
}
