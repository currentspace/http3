//! Platform I/O driver abstraction for UDP socket polling and sends.
//!
//! Provides the [`Driver`] trait that wraps platform-specific I/O multiplexing:
//! - macOS: `KqueueDriver` via `nix::sys::event` (EVFILT_READ/WRITE/USER)
//! - Linux: `IoUringDriver` via the `io-uring` crate (recvmsg/sendmsg CQEs)

use std::io;
use std::net::SocketAddr;
use std::time::Instant;

/// A completed received UDP datagram. Owned by the caller.
pub(crate) struct RxDatagram {
    pub data: Vec<u8>,
    pub peer: SocketAddr,
}

/// A transmit request. Ownership transfers to the driver.
pub(crate) struct TxDatagram {
    pub data: Vec<u8>,
    pub to: SocketAddr,
}

/// Outcome of a single `Driver::poll()` cycle.
pub(crate) struct PollOutcome {
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
pub(crate) trait Driver: Sized {
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

    /// Socket's bound local address.
    #[allow(dead_code)]
    fn local_addr(&self) -> io::Result<SocketAddr>;
}

/// Cross-thread wake handle. Clone + Send + Sync.
pub(crate) trait DriverWaker: Send + Sync + Clone + 'static {
    fn wake(&self) -> io::Result<()>;
}

/// Type-erased waker for handle structs that don't know the concrete driver.
pub(crate) trait ErasedWaker: Send + Sync {
    fn wake(&self) -> io::Result<()>;
}

impl<W: DriverWaker> ErasedWaker for W {
    fn wake(&self) -> io::Result<()> {
        DriverWaker::wake(self)
    }
}

pub(crate) mod socket;

// ── Platform driver selection ───────────────────────────────────────

#[cfg(target_os = "macos")]
mod kqueue;

#[cfg(target_os = "linux")]
mod io_uring;

#[cfg(target_os = "macos")]
pub(crate) type PlatformDriver = kqueue::KqueueDriver;

#[cfg(target_os = "linux")]
pub(crate) type PlatformDriver = io_uring::IoUringDriver;

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
compile_error!("Only macOS (kqueue) and Linux (io_uring) are supported");
