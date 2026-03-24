//! IoUringDriver: uses the `io-uring` crate for completion-based UDP I/O on
//! Linux. Both RX and TX stay on `io_uring` so the fast path does not fall
//! back to readiness or synchronous socket syscalls once the driver is active.
//!
//! RX uses multishot recvmsg with a kernel-managed provided buffer ring
//! (requires kernel ≥6.0). A single SQE arms the kernel to receive
//! datagrams into provided buffers, producing one CQE per datagram without
//! any per-packet SQE re-submission.
//!
//! This module is only compiled on Linux (`cfg(target_os = "linux")`).

#[cfg(target_os = "linux")]
mod inner {
    use std::collections::VecDeque;
    use std::io;
    use std::net::SocketAddr;
    use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use crate::buffer_pool::AdaptiveBufferPool;
    use crate::reactor_metrics;
    use crate::transport::socket::{
        CMSG_CONTROL_LEN, build_gso_cmsg, enable_gro, parse_gro_cmsg, parse_pktinfo_cmsg,
        probe_gso, set_pktinfo,
    };
    use crate::transport::{
        Driver, DriverWaker, PollOutcome, RuntimeDriverKind, RxDatagram, TxDatagram, group_for_gso,
    };

    /// Number of provided buffers in the RX buffer ring.
    const RX_RING_SIZE: u16 = 256;
    /// Each buffer must hold a full UDP datagram + the recvmsg_out header + sockaddr.
    /// Header: io_uring_recvmsg_out (16 bytes) + sockaddr_storage (128 bytes) = 144 bytes overhead.
    const RX_BUF_OVERHEAD: usize = std::mem::size_of::<libc::sockaddr_storage>()
        + 16 // io_uring_recvmsg_out header
        + CMSG_CONTROL_LEN;
    const USER_RX_BUF_SIZE: usize = 65535;
    const RX_BUF_SIZE: usize = 65535 + RX_BUF_OVERHEAD;
    const TX_SLOTS: usize = 256;
    const TIER2_TASKRUN_PREFETCH_LIMIT: u32 = 2;
    const TIER2_BLOCKING_WAIT_LIMIT: u32 = 4;

    const OP_RECV: u64 = 1 << 56;
    const OP_SEND: u64 = 2 << 56;
    const OP_WAKER: u64 = 3 << 56;
    const OP_BUNDLE: u64 = 4 << 56;
    const OP_MASK: u64 = 0xFF << 56;
    const IDX_MASK: u64 = (1 << 56) - 1;

    const BUF_GROUP: u16 = 0;
    const TX_BUF_GROUP: u16 = 1;
    /// Size of each TX buffer ring entry (max QUIC datagram + headroom).
    const TX_BUF_ENTRY_SIZE: usize = 1500;
    /// Number of TX buffer ring entries.
    const TX_RING_ENTRIES: u16 = 256;

    const FIXED_SOCKET: io_uring::types::Fixed = io_uring::types::Fixed(0);
    const FIXED_EVENTFD: io_uring::types::Fixed = io_uring::types::Fixed(1);

    /// Page-aligned buffer ring memory for the kernel provided-buffer interface.
    struct RxBufferRing {
        /// The raw memory region: buf_ring entries at the front, buffers after.
        /// Laid out as: [BufRingEntry × RX_RING_SIZE] [buffer × RX_RING_SIZE]
        /// Allocated via mmap for page-alignment.
        ring_ptr: *mut u8,
        ring_layout_size: usize,
        /// Pointer to the start of the buffer data area.
        buf_base: *mut u8,
        /// The msghdr used by the multishot SQE (only msg_namelen matters).
        msg: Box<libc::msghdr>,
        /// Tracks the next buffer ID for tail advancement.
        tail: u16,
    }

    // SAFETY: RxBufferRing is only accessed from the single driver thread.
    unsafe impl Send for RxBufferRing {}

    impl RxBufferRing {
        fn new(submitter: &io_uring::Submitter<'_>) -> io::Result<Self> {
            let entry_size = std::mem::size_of::<io_uring::types::BufRingEntry>();
            let ring_header_size = entry_size * (RX_RING_SIZE as usize);
            let total_buf_size = RX_BUF_SIZE * (RX_RING_SIZE as usize);
            let total_size = ring_header_size + total_buf_size;

            // Allocate page-aligned memory via mmap.
            // SAFETY: mmap with MAP_ANONYMOUS | MAP_PRIVATE returns zeroed memory.
            let ptr = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    total_size,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_ANONYMOUS | libc::MAP_PRIVATE,
                    -1,
                    0,
                )
            };
            if ptr == libc::MAP_FAILED {
                return Err(io::Error::last_os_error());
            }
            let ring_ptr = ptr.cast::<u8>();
            let buf_base = unsafe { ring_ptr.add(ring_header_size) };

            // Fill buf ring entries with buffer addresses.
            let entries = ring_ptr.cast::<io_uring::types::BufRingEntry>();
            for i in 0..(RX_RING_SIZE as usize) {
                let entry = unsafe { &mut *entries.add(i) };
                let buf_addr = unsafe { buf_base.add(i * RX_BUF_SIZE) };
                entry.set_addr(buf_addr as u64);
                entry.set_len(RX_BUF_SIZE as u32);
                entry.set_bid(i as u16);
            }

            // Register with the kernel.
            // SAFETY: ring_ptr is page-aligned mmap memory, entries are initialized.
            unsafe {
                submitter.register_buf_ring(ring_ptr as u64, RX_RING_SIZE, BUF_GROUP)?;
            }

            // Advance tail to make all buffers available.
            // SAFETY: entries is the base of a valid buf ring.
            unsafe {
                let tail_ptr = io_uring::types::BufRingEntry::tail(entries) as *mut u16;
                std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
                tail_ptr.write(RX_RING_SIZE);
            }

            // Build the msghdr for multishot (msg_namelen and msg_controllen are
            // used by the kernel to size the name/control regions in each provided buffer).
            // SAFETY: zeroed msghdr is valid.
            let mut msg: Box<libc::msghdr> = Box::new(unsafe { std::mem::zeroed() });
            msg.msg_namelen = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
            msg.msg_controllen = CMSG_CONTROL_LEN;

            Ok(Self {
                ring_ptr,
                ring_layout_size: total_size,
                buf_base,
                msg,
                tail: RX_RING_SIZE, // Next tail value to write
            })
        }

        /// Stage a consumed buffer for return (no fence yet).
        fn stage_buffer_return(&mut self, bid: u16) {
            let entries = self.ring_ptr.cast::<io_uring::types::BufRingEntry>();
            let slot = (self.tail % RX_RING_SIZE) as usize;
            let entry = unsafe { &mut *entries.add(slot) };

            let buf_addr = unsafe { self.buf_base.add((bid as usize) * RX_BUF_SIZE) };
            entry.set_addr(buf_addr as u64);
            entry.set_len(RX_BUF_SIZE as u32);
            entry.set_bid(bid);

            self.tail = self.tail.wrapping_add(1);
        }

        /// Publish all staged buffer returns to the kernel with a single fence.
        fn flush_buffer_returns(&self) {
            let entries = self.ring_ptr.cast::<io_uring::types::BufRingEntry>();
            // SAFETY: entries is the base of a valid buf ring.
            unsafe {
                let tail_ptr = io_uring::types::BufRingEntry::tail(entries) as *mut u16;
                std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
                tail_ptr.write(self.tail);
            }
        }

        /// Get a reference to the buffer data for a given buffer ID.
        fn buffer_data(&self, bid: u16) -> &[u8] {
            let offset = (bid as usize) * RX_BUF_SIZE;
            // SAFETY: bid is within [0, RX_RING_SIZE), buffer region is valid.
            unsafe { std::slice::from_raw_parts(self.buf_base.add(offset), RX_BUF_SIZE) }
        }
    }

    impl Drop for RxBufferRing {
        fn drop(&mut self) {
            // SAFETY: ring_ptr was obtained from mmap with ring_layout_size.
            unsafe {
                libc::munmap(self.ring_ptr.cast(), self.ring_layout_size);
            }
        }
    }

    /// TX provided buffer ring for send bundles (kernel ≥6.10).
    /// At most one SendBundle SQE is in flight at a time. Between CQEs the
    /// ring is empty (head == tail) so we can refill from position 0.
    struct TxBufRing {
        ring_ptr: *mut u8,
        ring_layout_size: usize,
        buf_base: *mut u8,
        tail: u16,
        /// True when a SendBundle SQE is in flight.
        in_flight: bool,
        /// Number of buffers in the current in-flight bundle.
        in_flight_count: usize,
        /// Data length for each buffer in the current in-flight bundle.
        in_flight_lengths: Vec<usize>,
        /// The connected peer address (SendBundle only works with connected sockets).
        connected_peer: SocketAddr,
    }

    // SAFETY: TxBufRing is only accessed from the single driver thread.
    unsafe impl Send for TxBufRing {}

    impl TxBufRing {
        fn new(submitter: &io_uring::Submitter<'_>, peer: SocketAddr) -> io::Result<Self> {
            let entry_size = std::mem::size_of::<io_uring::types::BufRingEntry>();
            let ring_header_size = entry_size * (TX_RING_ENTRIES as usize);
            let total_buf_size = TX_BUF_ENTRY_SIZE * (TX_RING_ENTRIES as usize);
            let total_size = ring_header_size + total_buf_size;

            // SAFETY: mmap with MAP_ANONYMOUS | MAP_PRIVATE returns zeroed memory.
            let ptr = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    total_size,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_ANONYMOUS | libc::MAP_PRIVATE,
                    -1,
                    0,
                )
            };
            if ptr == libc::MAP_FAILED {
                return Err(io::Error::last_os_error());
            }
            let ring_ptr = ptr.cast::<u8>();
            let buf_base = unsafe { ring_ptr.add(ring_header_size) };

            // Initialize entries with buffer addresses.
            let entries = ring_ptr.cast::<io_uring::types::BufRingEntry>();
            for i in 0..(TX_RING_ENTRIES as usize) {
                let entry = unsafe { &mut *entries.add(i) };
                let buf_addr = unsafe { buf_base.add(i * TX_BUF_ENTRY_SIZE) };
                entry.set_addr(buf_addr as u64);
                entry.set_len(0);
                entry.set_bid(i as u16);
            }

            // Register with kernel.
            // SAFETY: ring_ptr is page-aligned mmap memory, entries are initialized.
            unsafe {
                submitter.register_buf_ring(ring_ptr as u64, TX_RING_ENTRIES, TX_BUF_GROUP)?;
            }

            // Tail starts at 0 — no buffers available until fill_and_submit.
            unsafe {
                let tail_ptr = io_uring::types::BufRingEntry::tail(entries) as *mut u16;
                tail_ptr.write(0);
            }

            Ok(Self {
                ring_ptr,
                ring_layout_size: total_size,
                buf_base,
                tail: 0,
                in_flight: false,
                in_flight_count: 0,
                in_flight_lengths: Vec::with_capacity(TX_RING_ENTRIES as usize),
                connected_peer: peer,
            })
        }

        /// Fill ring entries with packet data and publish the tail.
        /// Returns how many packets were enqueued.
        fn fill_and_publish(&mut self, packets: &[TxDatagram]) -> usize {
            let n = packets.len().min(TX_RING_ENTRIES as usize);
            let entries = self.ring_ptr.cast::<io_uring::types::BufRingEntry>();

            self.in_flight_lengths.clear();
            for i in 0..n {
                let slot = (self.tail % TX_RING_ENTRIES) as usize;
                let len = packets[i].data.len().min(TX_BUF_ENTRY_SIZE);
                let buf_offset = slot * TX_BUF_ENTRY_SIZE;

                // SAFETY: slot is in [0, TX_RING_ENTRIES), buf_offset is valid.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        packets[i].data.as_ptr(),
                        self.buf_base.add(buf_offset),
                        len,
                    );
                    let entry = &mut *entries.add(slot);
                    entry.set_addr(self.buf_base.add(buf_offset) as u64);
                    entry.set_len(len as u32);
                    entry.set_bid(slot as u16);
                }

                self.in_flight_lengths.push(len);
                self.tail = self.tail.wrapping_add(1);
            }

            // Publish tail to kernel.
            unsafe {
                let tail_ptr = io_uring::types::BufRingEntry::tail(entries) as *mut u16;
                std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
                tail_ptr.write(self.tail);
            }

            self.in_flight = true;
            self.in_flight_count = n;
            n
        }

        /// Process a completed SendBundle CQE. Returns (consumed, unsent_packets).
        /// Unsent packets are extracted from the ring and returned for retry.
        fn complete(&mut self, bytes_sent: usize) -> (usize, Vec<TxDatagram>) {
            let mut remaining = bytes_sent;
            let mut consumed = 0;
            for &len in &self.in_flight_lengths {
                if remaining >= len {
                    remaining -= len;
                    consumed += 1;
                } else {
                    break;
                }
            }

            // Extract unsent packet data for retry.
            let mut unsent = Vec::new();
            let base_slot =
                ((self.tail.wrapping_sub(self.in_flight_count as u16)) % TX_RING_ENTRIES) as usize;
            for i in consumed..self.in_flight_count {
                let slot = (base_slot + i) % TX_RING_ENTRIES as usize;
                let len = self.in_flight_lengths[i];
                let buf_offset = slot * TX_BUF_ENTRY_SIZE;
                let data = unsafe {
                    std::slice::from_raw_parts(self.buf_base.add(buf_offset), len).to_vec()
                };
                unsent.push(TxDatagram {
                    data,
                    to: self.connected_peer,
                });
            }

            self.in_flight = false;
            self.in_flight_count = 0;
            self.in_flight_lengths.clear();
            (consumed, unsent)
        }

        /// Reset the ring by unregistering and re-registering.
        /// Called after partial sends to reset the kernel's internal head pointer.
        fn reset(&mut self, submitter: &io_uring::Submitter<'_>) -> io::Result<()> {
            submitter.unregister_buf_ring(TX_BUF_GROUP)?;
            unsafe {
                submitter.register_buf_ring(self.ring_ptr as u64, TX_RING_ENTRIES, TX_BUF_GROUP)?;
            }
            self.tail = 0;
            let entries = self.ring_ptr.cast::<io_uring::types::BufRingEntry>();
            unsafe {
                let tail_ptr = io_uring::types::BufRingEntry::tail(entries) as *mut u16;
                tail_ptr.write(0);
            }
            Ok(())
        }
    }

    impl Drop for TxBufRing {
        fn drop(&mut self) {
            // SAFETY: ring_ptr was obtained from mmap with ring_layout_size.
            unsafe {
                libc::munmap(self.ring_ptr.cast(), self.ring_layout_size);
            }
        }
    }

    /// A single sendmsg operation slot. All kernel-visible pointers live behind
    /// `Box` so they remain stable while an SQE is in flight.
    struct TxSlot {
        data: Vec<u8>,
        peer: SocketAddr,
        addr: Box<libc::sockaddr_storage>,
        iov: Box<libc::iovec>,
        msg: Box<libc::msghdr>,
        cmsg_buf: Box<[u8; 32]>,
        /// Non-zero when this slot holds a GSO batch. Used to split on retry.
        gso_segment_size: u16,
        in_flight: bool,
    }

    impl TxSlot {
        fn new() -> Self {
            let mut slot = Self {
                data: Vec::new(),
                peer: SocketAddr::from(([0, 0, 0, 0], 0)),
                // SAFETY: zeroed sockaddr_storage is valid (all-zeros family = AF_UNSPEC).
                addr: Box::new(unsafe { std::mem::zeroed() }),
                iov: Box::new(libc::iovec {
                    iov_base: std::ptr::null_mut(),
                    iov_len: 0,
                }),
                // SAFETY: zeroed msghdr is valid (null pointers, zero lengths).
                msg: Box::new(unsafe { std::mem::zeroed() }),
                cmsg_buf: Box::new([0u8; 32]),
                gso_segment_size: 0,
                in_flight: false,
            };
            slot.msg.msg_name = (slot.addr.as_mut() as *mut libc::sockaddr_storage).cast();
            slot.msg.msg_iov = slot.iov.as_mut() as *mut libc::iovec;
            slot.msg.msg_iovlen = 1;
            slot
        }

        fn prepare(&mut self, packet: TxDatagram) {
            self.data = packet.data;
            self.peer = packet.to;
            self.iov.iov_base = self.data.as_mut_ptr().cast();
            self.iov.iov_len = self.data.len();
            self.msg.msg_namelen = socketaddr_to_sockaddr(self.peer, self.addr.as_mut());
            self.msg.msg_control = std::ptr::null_mut();
            self.msg.msg_controllen = 0;
            self.msg.msg_flags = 0;
            self.gso_segment_size = 0;
            self.in_flight = true;
        }

        fn prepare_gso(&mut self, data: Vec<u8>, to: SocketAddr, segment_size: u16) {
            self.data = data;
            self.peer = to;
            self.iov.iov_base = self.data.as_mut_ptr().cast();
            self.iov.iov_len = self.data.len();
            self.msg.msg_namelen = socketaddr_to_sockaddr(self.peer, self.addr.as_mut());
            let cmsg_len = build_gso_cmsg(&mut *self.cmsg_buf, segment_size);
            self.msg.msg_control = self.cmsg_buf.as_mut_ptr().cast();
            self.msg.msg_controllen = cmsg_len;
            self.msg.msg_flags = 0;
            self.gso_segment_size = segment_size;
            self.in_flight = true;
        }

        fn take_packet(&mut self) -> TxDatagram {
            self.in_flight = false;
            self.gso_segment_size = 0;
            TxDatagram {
                data: std::mem::take(&mut self.data),
                to: self.peer,
            }
        }

        fn recycle_buffer(&mut self) -> Vec<u8> {
            self.in_flight = false;
            self.gso_segment_size = 0;
            std::mem::take(&mut self.data)
        }
    }

    pub struct IoUringDriver {
        ring: io_uring::IoUring,
        socket_fd: RawFd,
        socket: std::net::UdpSocket,
        local_addr: SocketAddr,
        gso_supported: bool,
        eventfd: OwnedFd,
        rx_ring: RxBufferRing,
        /// Whether the multishot recvmsg SQE is currently armed.
        rx_armed: bool,
        /// True when the ring was created with R_DISABLED and needs
        /// register_enable_rings() on the worker thread before first use.
        needs_enable: bool,
        /// True when the ring was created with DEFER_TASKRUN semantics.
        defer_taskrun_enabled: bool,
        /// TX provided buffer ring for send bundles (None if unsupported).
        tx_buf_ring: Option<TxBufRing>,
        tx_slots: Vec<TxSlot>,
        waker_buf: Box<[u8; 8]>,
        tx_in_flight: usize,
        /// Total payload bytes across all in-flight TX SQEs.
        tx_bytes_in_flight: usize,
        /// Cap on tx_bytes_in_flight — derived from the socket's effective
        /// SO_RCVBUF, which is the best local estimate of what a peer on the
        /// same system can absorb before the kernel starts dropping.
        tx_bytes_cap: usize,
        pending_tx: VecDeque<TxDatagram>,
        recycled_tx: Vec<Vec<u8>>,
        rx_pool: AdaptiveBufferPool,
        cqe_buf: Vec<io_uring::cqueue::Entry>,
        /// RX datagrams harvested during process_cqes_inline(), prepended
        /// to the next poll() outcome so they aren't lost.
        deferred_rx: Vec<RxDatagram>,
        /// Waker fired during process_cqes_inline().
        deferred_woken: bool,
    }

    // SAFETY: IoUringDriver is created on the main thread and moved to the worker
    // thread before any I/O occurs. The raw pointers inside RxBufferRing point to
    // mmap'd memory that moves with the driver. The driver is single-threaded
    // after the move — no concurrent access.
    unsafe impl Send for IoUringDriver {}

    #[derive(Clone)]
    pub struct IoUringWaker {
        eventfd: Arc<OwnedFd>,
    }

    impl Driver for IoUringDriver {
        type Waker = IoUringWaker;

        fn new(socket: std::net::UdpSocket) -> io::Result<(Self, Self::Waker)> {
            // DEFER_TASKRUN + COOP_TASKRUN + SINGLE_ISSUER reduce overhead by
            // deferring completion work to io_uring_enter. However, SINGLE_ISSUER
            // requires the thread calling io_uring_enter(GETEVENTS) to be the same
            // thread that "enabled" the ring.
            //
            // Our architecture creates the driver on the main thread then moves it
            // to a worker thread. We use R_DISABLED to create the ring without an
            // owner, then call register_enable_rings() on the first poll() — which
            // runs on the worker thread, making IT the submitter task.
            //
            // Registrations (files, buffer rings) work while the ring is disabled.
            // SQE submission and io_uring_enter require the ring to be enabled.
            let (ring, defer_taskrun) = io_uring::IoUring::builder()
                .setup_coop_taskrun()
                .setup_single_issuer()
                .setup_defer_taskrun()
                .setup_r_disabled()
                .setup_cqsize(4096)
                .build(128)
                .map(|r| (r, true))
                .or_else(|_| {
                    // Fallback: without DEFER_TASKRUN (kernel < 6.1).
                    io_uring::IoUring::builder()
                        .setup_cqsize(4096)
                        .build(128)
                        .map(|r| (r, false))
                })
                .or_else(|_| {
                    // Minimal fallback.
                    io_uring::IoUring::new(128).map(|r| (r, false))
                })?;
            let socket_fd = socket.as_raw_fd();
            let local_addr = socket.local_addr()?;
            let gso_supported = probe_gso(&socket);
            set_pktinfo(&socket);
            enable_gro(&socket);
            log::info!(
                "IoUringDriver::new fd={socket_fd} local={local_addr} gso={gso_supported} defer_taskrun={defer_taskrun} tid={:?}",
                std::thread::current().id(),
            );

            // Create eventfd for wakeup
            // SAFETY: eventfd with EFD_NONBLOCK returns a valid fd or -1.
            let efd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK) };
            if efd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: efd is a valid fd from successful eventfd() call.
            let eventfd = unsafe { OwnedFd::from_raw_fd(efd) };

            // Register socket and eventfd for fixed-fd SQE submission.
            ring.submitter()
                .register_files(&[socket_fd, eventfd.as_raw_fd()])
                .map_err(|e| {
                    io::Error::new(io::ErrorKind::Other, format!("register_files: {e}"))
                })?;

            // Set up provided buffer ring for multishot recvmsg.
            let rx_ring = RxBufferRing::new(&ring.submitter())?;

            let tx_slots: Vec<TxSlot> = (0..TX_SLOTS).map(|_| TxSlot::new()).collect();

            // Probe for send bundle support (kernel ≥6.10) with connected socket.
            let tx_buf_ring = if ring.params().is_feature_recvsend_bundle() {
                socket
                    .peer_addr()
                    .ok()
                    .and_then(|peer| TxBufRing::new(&ring.submitter(), peer).ok())
            } else {
                None
            };

            // Read the effective receive buffer size — best local estimate of
            // what a peer on the same system can absorb.  The kernel doubles the
            // requested value, so getsockopt returns 2× the setsockopt value.
            let tx_bytes_cap = {
                let mut val: libc::c_int = 0;
                let mut len = std::mem::size_of_val(&val) as libc::socklen_t;
                let rc = unsafe {
                    libc::getsockopt(
                        socket_fd,
                        libc::SOL_SOCKET,
                        libc::SO_RCVBUF,
                        &mut val as *mut _ as *mut libc::c_void,
                        &mut len,
                    )
                };
                let raw = if rc == 0 && val > 0 {
                    val as usize
                } else {
                    212992
                };
                // Use 75% of the receiver's buffer as our cap — the remaining
                // 25% absorbs packets already in the kernel's send pipeline
                // (submitted but not yet completed) plus any peer sends that
                // share the same buffer.
                let effective = raw * 3 / 4;
                log::info!("IoUringDriver: tx_bytes_cap={effective} (75% of SO_RCVBUF={raw})",);
                effective
            };

            let mut driver = Self {
                ring,
                socket_fd,
                socket,
                local_addr,
                gso_supported,
                eventfd,
                rx_ring,
                rx_armed: false,
                needs_enable: defer_taskrun,
                defer_taskrun_enabled: defer_taskrun,
                tx_buf_ring,
                tx_slots,
                waker_buf: Box::new([0u8; 8]),
                tx_in_flight: 0,
                tx_bytes_in_flight: 0,
                tx_bytes_cap,
                pending_tx: VecDeque::new(),
                recycled_tx: Vec::new(),
                rx_pool: AdaptiveBufferPool::new(RX_RING_SIZE as usize, USER_RX_BUF_SIZE),
                cqe_buf: Vec::with_capacity(512),
                deferred_rx: Vec::new(),
                deferred_woken: false,
            };

            // When R_DISABLED is active, SQE submission is deferred until the
            // worker thread calls enable_on_worker_thread(). Otherwise, arm now.
            if !defer_taskrun {
                driver.arm_multishot_recv()?;
                driver.submit_waker_read()?;
                reactor_metrics::record_io_uring_submit_call();
                driver.ring.submit()?;
            }

            // SAFETY: dup the eventfd for the waker (the driver keeps the original).
            let waker_fd = unsafe { libc::dup(driver.eventfd.as_raw_fd()) };
            if waker_fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: waker_fd is a valid fd from successful dup().
            let waker_eventfd = unsafe { OwnedFd::from_raw_fd(waker_fd) };

            let waker = IoUringWaker {
                eventfd: Arc::new(waker_eventfd),
            };
            Ok((driver, waker))
        }

        fn poll(&mut self, deadline: Option<Instant>) -> io::Result<PollOutcome> {
            // First call on the worker thread: enable the ring and arm initial SQEs.
            // This makes the current (worker) thread the SINGLE_ISSUER submitter task,
            // allowing DEFER_TASKRUN to work correctly.
            if self.needs_enable {
                self.enable_on_worker_thread()?;
            }

            // Queue any pending TX SQEs — submit_with_args will flush them.
            self.submit_pending_tx()?;

            let wait_dur = deadline.map_or(Duration::from_millis(100), |d| {
                d.saturating_duration_since(Instant::now())
            });

            // Single syscall: submit all pending SQEs AND wait for ≥1 CQE.
            let ts = io_uring::types::Timespec::new()
                .sec(wait_dur.as_secs())
                .nsec(wait_dur.subsec_nanos());
            let args = io_uring::types::SubmitArgs::new().timespec(&ts);
            reactor_metrics::record_io_uring_submit_with_args_call();
            match self.ring.submitter().submit_with_args(1, &args) {
                Ok(_) => {}
                Err(ref e) if e.raw_os_error() == Some(libc::ETIME) => {}
                Err(ref e) if e.raw_os_error() == Some(libc::EINTR) => {}
                Err(e) => return Err(e),
            }

            let mut outcome = PollOutcome {
                rx: std::mem::take(&mut self.deferred_rx),
                woken: std::mem::take(&mut self.deferred_woken),
                timer_expired: false,
            };

            if deadline.is_some_and(|d| Instant::now() >= d) {
                outcome.timer_expired = true;
            }

            // Drain CQEs into reusable buffer.
            self.cqe_buf.clear();
            self.cqe_buf.extend(self.ring.completion());
            let cqe_count = self.cqe_buf.len();

            let mut bundle_needs_reset = false;
            for cqe_idx in 0..cqe_count {
                let cqe = &self.cqe_buf[cqe_idx];
                let user_data = cqe.user_data();
                let op = user_data & OP_MASK;
                let result = cqe.result();
                let flags = cqe.flags();

                match op {
                    OP_RECV => {
                        // Multishot recvmsg: check if more completions coming.
                        let has_more = io_uring::cqueue::more(flags);
                        if !has_more {
                            self.rx_armed = false;
                        }

                        if result > 0 {
                            if let Some(bid) = io_uring::cqueue::buffer_select(flags) {
                                let buf = self.rx_ring.buffer_data(bid);
                                let buf_len = result as usize;

                                // Parse the recvmsg_out header to extract peer address
                                // and payload from the provided buffer.
                                if let Ok(parsed) = io_uring::types::RecvMsgOut::parse(
                                    &buf[..buf_len],
                                    self.rx_ring.msg.as_ref(),
                                ) {
                                    let name_data = parsed.name_data();
                                    let peer = parse_sockaddr(name_data);
                                    if let Some(peer) = peer {
                                        let control = parsed.control_data();
                                        let local_ip = parse_pktinfo_cmsg(control);
                                        let local = local_ip
                                            .map(|ip| SocketAddr::new(ip, self.local_addr.port()))
                                            .unwrap_or(self.local_addr);
                                        let segment_size = parse_gro_cmsg(control);
                                        let payload = parsed.payload_data();
                                        let (data, reused) = self.rx_pool.copy_from_slice(payload);
                                        reactor_metrics::record_rx_buffer_checkout(
                                            reused,
                                            payload.len(),
                                        );
                                        outcome.rx.push(RxDatagram {
                                            data,
                                            peer,
                                            local,
                                            segment_size,
                                        });
                                        reactor_metrics::record_io_uring_rx_datagrams(1);
                                    }
                                }

                                // Return buffer to the ring immediately.
                                self.rx_ring.stage_buffer_return(bid);
                            }
                        } else if result < 0 {
                            // Error on multishot — will re-arm below.
                        }
                    }
                    OP_SEND => {
                        let idx = (user_data & IDX_MASK) as usize;
                        self.tx_in_flight -= 1;
                        let slot = &mut self.tx_slots[idx];
                        self.tx_bytes_in_flight =
                            self.tx_bytes_in_flight.saturating_sub(slot.data.len());
                        if result >= 0 {
                            if slot.gso_segment_size > 0 {
                                log::trace!(
                                    "io_uring OP_SEND GSO complete: idx={idx} result={result} seg_size={} data_len={} in_flight={}",
                                    slot.gso_segment_size,
                                    slot.data.len(),
                                    self.tx_in_flight,
                                );
                            }
                            self.recycled_tx.push(slot.recycle_buffer());
                            reactor_metrics::record_io_uring_tx_datagrams_completed(1);
                        } else {
                            let errno = -result;
                            log::warn!(
                                "io_uring OP_SEND error: idx={idx} errno={errno} gso_seg={} data_len={} in_flight={} retryable={}",
                                slot.gso_segment_size,
                                slot.data.len(),
                                self.tx_in_flight,
                                errno == libc::EAGAIN
                                    || errno == libc::ENOBUFS
                                    || errno == libc::EINTR
                                    || (errno == libc::EMSGSIZE && slot.gso_segment_size > 0),
                            );
                            if errno == libc::EAGAIN
                                || errno == libc::ENOBUFS
                                || errno == libc::EINTR
                                || (errno == libc::EMSGSIZE && slot.gso_segment_size > 0)
                            {
                                reactor_metrics::record_io_uring_retryable_send_completion();
                                // GSO batch or retryable: split back into individual packets.
                                let seg = slot.gso_segment_size;
                                if seg > 0 {
                                    let peer = slot.peer;
                                    let data = slot.recycle_buffer();
                                    for chunk in data.chunks(seg as usize) {
                                        self.pending_tx.push_back(TxDatagram {
                                            data: chunk.to_vec(),
                                            to: peer,
                                        });
                                    }
                                } else {
                                    self.pending_tx.push_back(slot.take_packet());
                                }
                                reactor_metrics::record_io_uring_pending_tx(self.pending_tx.len());
                            } else {
                                self.recycled_tx.push(slot.recycle_buffer());
                            }
                        }
                    }
                    OP_WAKER => {
                        outcome.woken = true;
                        reactor_metrics::record_io_uring_wake_completion();
                        // Drain eventfd counter.
                        // SAFETY: reading 8 bytes from a valid eventfd.
                        unsafe {
                            libc::read(
                                self.eventfd.as_raw_fd(),
                                self.waker_buf.as_mut_ptr().cast(),
                                8,
                            );
                        }
                        // Resubmit waker read — flushed by next submit_with_args.
                        let _ = self.submit_waker_read();
                    }
                    OP_BUNDLE => {
                        if let Some(ref mut tx_ring) = self.tx_buf_ring {
                            let (consumed, unsent) = if result > 0 {
                                tx_ring.complete(result as usize)
                            } else {
                                tx_ring.complete(0)
                            };
                            reactor_metrics::record_io_uring_tx_datagrams_completed(consumed);
                            if !unsent.is_empty() {
                                let retryable = result >= 0
                                    || matches!(-result, e if e == libc::EAGAIN || e == libc::ENOBUFS || e == libc::EINTR);
                                if retryable {
                                    for pkt in unsent {
                                        self.pending_tx.push_back(pkt);
                                    }
                                }
                                bundle_needs_reset = true;
                            } else if result <= 0 {
                                bundle_needs_reset = true;
                            }
                        }
                    }
                    _ => {}
                }
            }
            reactor_metrics::record_io_uring_completions(cqe_count);
            if cqe_count > 0 {
                log::trace!(
                    "io_uring::poll CQEs={cqe_count} rx={} tx_in_flight={} pending={}",
                    outcome.rx.len(),
                    self.tx_in_flight,
                    self.pending_tx.len(),
                );
            }

            // Single fence to publish all returned buffers to the kernel.
            self.rx_ring.flush_buffer_returns();

            // Deferred TX bundle ring reset (avoids borrow conflict in CQE loop).
            if bundle_needs_reset {
                if let Some(ref mut tx_ring) = self.tx_buf_ring {
                    let _ = tx_ring.reset(&self.ring.submitter());
                }
            }

            if cqe_count == 0 {
                reactor_metrics::record_io_uring_timeout_poll();
                outcome.timer_expired = true;
                if self.tx_in_flight > 0 || !self.pending_tx.is_empty() {
                    log::warn!(
                        "io_uring::poll TIMEOUT with tx_in_flight={} pending={} wait={:?}",
                        self.tx_in_flight,
                        self.pending_tx.len(),
                        wait_dur,
                    );
                }
            }

            // Re-arm multishot recvmsg if it was disarmed.
            if !self.rx_armed {
                self.arm_multishot_recv()?;
            }

            // Queue pending TX — flushed by next submit_with_args.
            self.submit_pending_tx()?;

            Ok(outcome)
        }

        fn submit_sends(&mut self, packets: Vec<TxDatagram>) -> io::Result<()> {
            let pkt_count = packets.len();
            // Log at warn level when under pressure so we can diagnose stalls.
            if self.tx_in_flight > TX_SLOTS / 2
                || !self.pending_tx.is_empty()
                || self.tx_bytes_in_flight >= self.tx_bytes_cap
            {
                log::warn!(
                    "io_uring::submit_sends PRESSURE: {} pkts, tx_in_flight={}/{}, pending={}, bytes={}/{}, gso={}",
                    pkt_count,
                    self.tx_in_flight,
                    TX_SLOTS,
                    self.pending_tx.len(),
                    self.tx_bytes_in_flight,
                    self.tx_bytes_cap,
                    self.gso_supported,
                );
            }
            log::trace!(
                "io_uring::submit_sends: {} pkts, gso={}, tx_in_flight={}, pending={}",
                packets.len(),
                self.gso_supported,
                self.tx_in_flight,
                self.pending_tx.len(),
            );
            // Try send bundles first (connected socket, kernel ≥6.10).
            if let Some(ref mut tx_ring) = self.tx_buf_ring {
                if !tx_ring.in_flight && !packets.is_empty() {
                    // Process any pending CQEs to reclaim the ring.
                    self.drain_bundle_cqes();
                    return self.submit_send_bundle(packets);
                }
            }
            let mut sqes_pushed = 0usize;
            if self.gso_supported && packets.len() > 1 {
                // Group into GSO batches, then route through pending_tx so
                // the SQE creation path is identical to non-GSO sends.
                let batches = group_for_gso(packets);
                for batch in batches {
                    if batch.data.len() > batch.segment_size as usize {
                        // Multi-segment batch: needs GSO SQE with cmsg.
                        // Bytes-cap check: if we'd exceed the peer's estimated
                        // receive buffer, split to pending_tx for the drain loop.
                        if self.tx_bytes_in_flight + batch.data.len() > self.tx_bytes_cap
                            && self.tx_in_flight > 0
                        {
                            let seg = batch.segment_size as usize;
                            for chunk in batch.data.chunks(seg) {
                                self.pending_tx.push_back(TxDatagram {
                                    data: chunk.to_vec(),
                                    to: batch.to,
                                });
                            }
                            continue;
                        }
                        // Find a free slot directly (can't go through pending_tx).
                        let Some(idx) = self.tx_slots.iter().position(|s| !s.in_flight) else {
                            // No slot: split back to individual packets.
                            let seg = batch.segment_size as usize;
                            for chunk in batch.data.chunks(seg) {
                                self.pending_tx.push_back(TxDatagram {
                                    data: chunk.to_vec(),
                                    to: batch.to,
                                });
                            }
                            continue;
                        };
                        self.tx_slots[idx].prepare_gso(batch.data, batch.to, batch.segment_size);
                        let slot = &mut self.tx_slots[idx];
                        let entry = io_uring::opcode::SendMsg::new(
                            FIXED_SOCKET,
                            slot.msg.as_mut() as *mut libc::msghdr,
                        )
                        .build()
                        .user_data(OP_SEND | idx as u64);
                        let push_result = unsafe { self.ring.submission().push(&entry) };
                        if push_result.is_err() {
                            let seg = self.tx_slots[idx].gso_segment_size as usize;
                            let peer = self.tx_slots[idx].peer;
                            let data = self.tx_slots[idx].recycle_buffer();
                            for chunk in data.chunks(seg) {
                                self.pending_tx.push_back(TxDatagram {
                                    data: chunk.to_vec(),
                                    to: peer,
                                });
                            }
                        } else {
                            self.tx_bytes_in_flight += self.tx_slots[idx].data.len();
                            self.tx_in_flight += 1;
                            sqes_pushed += 1;
                        }
                    } else {
                        // Single-packet batch: route through pending_tx (no cmsg needed).
                        self.pending_tx.push_back(TxDatagram {
                            data: batch.data,
                            to: batch.to,
                        });
                    }
                }
            } else {
                for pkt in packets {
                    self.pending_tx.push_back(pkt);
                    reactor_metrics::record_io_uring_pending_tx(self.pending_tx.len());
                }
            }
            sqes_pushed += self.submit_pending_tx()?;

            // Flush SQEs to the kernel immediately so sends don't stall until
            // the next poll(). This is a non-blocking io_uring_enter(to_submit=N,
            // min_complete=0) — lightweight with DEFER_TASKRUN.
            if sqes_pushed > 0 {
                reactor_metrics::record_io_uring_submit_call();
                let submitted = self.ring.submit();
                if submitted.is_err() {
                    log::warn!(
                        "io_uring::submit_sends: ring.submit() error: {:?}",
                        submitted
                    );
                }
            }

            // Tier 2: When pending_tx is non-empty, all TX slots are occupied.
            // We need to guarantee some forward progress here, but keep the work
            // bounded so the normal poll() path remains responsible for draining
            // the bulk of completions.
            if !self.pending_tx.is_empty() && self.tx_in_flight > 0 {
                let drain_start_pending = self.pending_tx.len();
                let drain_start_inflight = self.tx_in_flight;
                let mut drain_rounds = 0u32;
                let mut blocking_waits = 0u32;
                let mut made_progress = false;

                for _ in 0..TIER2_TASKRUN_PREFETCH_LIMIT {
                    if !self.defer_taskrun_enabled
                        || !self.ring.submission().taskrun()
                        || self.pending_tx.is_empty()
                        || self.tx_in_flight == 0
                    {
                        break;
                    }
                    drain_rounds += 1;
                    reactor_metrics::record_io_uring_tier2_drain_round();
                    reactor_metrics::record_io_uring_tier2_taskrun_prefetch();

                    let freed = self.drain_completions_for_tx()?;
                    if freed == 0 {
                        break;
                    }

                    made_progress = true;
                    self.flush_pending_tx_after_progress()?;
                    break;
                }

                while !made_progress
                    && !self.pending_tx.is_empty()
                    && self.tx_in_flight > 0
                    && blocking_waits < TIER2_BLOCKING_WAIT_LIMIT
                {
                    drain_rounds += 1;
                    blocking_waits += 1;
                    reactor_metrics::record_io_uring_tier2_drain_round();
                    reactor_metrics::record_io_uring_tier2_blocking_wait();
                    reactor_metrics::record_io_uring_submit_call();
                    match self.ring.submit_and_wait(1) {
                        Ok(_) => {}
                        Err(ref e) if e.raw_os_error() == Some(libc::EINTR) => continue,
                        Err(e) => {
                            log::warn!("io_uring::submit_sends drain: submit_and_wait error: {e}");
                            break;
                        }
                    }

                    let freed = self.drain_ready_cqes_inline()?;
                    if freed > 0 {
                        made_progress = true;
                        self.flush_pending_tx_after_progress()?;
                    }
                }

                if !made_progress && !self.pending_tx.is_empty() && self.tx_in_flight > 0 {
                    reactor_metrics::record_io_uring_tier2_cap_hit();
                    log::trace!(
                        "io_uring::submit_sends bounded drain cap: rounds={drain_rounds} pending {drain_start_pending}->{}, in_flight {drain_start_inflight}->{}, input_pkts={pkt_count}",
                        self.pending_tx.len(),
                        self.tx_in_flight,
                    );
                }
            }
            Ok(())
        }

        fn pending_tx_count(&self) -> usize {
            self.pending_tx.len() + self.tx_in_flight
        }

        fn drain_recycled_tx(&mut self) -> Vec<Vec<u8>> {
            std::mem::take(&mut self.recycled_tx)
        }

        fn local_addr(&self) -> io::Result<SocketAddr> {
            self.socket.local_addr()
        }

        fn driver_kind(&self) -> RuntimeDriverKind {
            RuntimeDriverKind::IoUring
        }

        fn recycle_rx_buffers(&mut self, buffers: Vec<Vec<u8>>) {
            for buf in buffers {
                let retained = self.rx_pool.checkin(buf);
                reactor_metrics::record_rx_buffer_checkin(retained);
            }
        }
    }

    impl IoUringDriver {
        /// Enable the ring on the worker thread. Called once on the first poll().
        /// This makes the current thread the SINGLE_ISSUER submitter, allowing
        /// DEFER_TASKRUN to work. Then arms the initial SQEs.
        fn enable_on_worker_thread(&mut self) -> io::Result<()> {
            let _ = env_logger::try_init();
            log::info!(
                "IoUringDriver::enable_on_worker_thread tid={:?}",
                std::thread::current().id(),
            );
            self.ring.submitter().register_enable_rings()?;
            self.needs_enable = false;

            // Now we can submit SQEs.
            self.arm_multishot_recv()?;
            self.submit_waker_read()?;
            reactor_metrics::record_io_uring_submit_call();
            self.ring.submit()?;
            Ok(())
        }

        /// Submit the single multishot recvmsg SQE.
        fn arm_multishot_recv(&mut self) -> io::Result<()> {
            let entry = io_uring::opcode::RecvMsgMulti::new(
                FIXED_SOCKET,
                self.rx_ring.msg.as_ref() as *const libc::msghdr,
                BUF_GROUP,
            )
            .build()
            .user_data(OP_RECV);

            // SAFETY: rx_ring.msg has stable Box address, buffer ring is registered.
            unsafe {
                self.ring.submission().push(&entry).map_err(|_| {
                    reactor_metrics::record_io_uring_sq_full_event();
                    io::Error::new(io::ErrorKind::Other, "SQ full")
                })?;
            }
            self.rx_armed = true;
            reactor_metrics::record_io_uring_submitted_sqes(1);
            Ok(())
        }

        fn submit_waker_read(&mut self) -> io::Result<()> {
            let entry = io_uring::opcode::Read::new(FIXED_EVENTFD, self.waker_buf.as_mut_ptr(), 8)
                .build()
                .user_data(OP_WAKER);

            // SAFETY: waker_buf is a stable Box address. Only one read is in flight.
            unsafe {
                self.ring.submission().push(&entry).map_err(|_| {
                    reactor_metrics::record_io_uring_sq_full_event();
                    io::Error::new(io::ErrorKind::Other, "SQ full")
                })?;
            }
            reactor_metrics::record_io_uring_submitted_sqes(1);
            Ok(())
        }

        /// Submit packets via send bundle (connected socket, kernel ≥6.10).
        /// Falls back to GSO/SendMsg for overflow packets.
        fn submit_send_bundle(&mut self, packets: Vec<TxDatagram>) -> io::Result<()> {
            let tx_ring = self.tx_buf_ring.as_mut().unwrap();
            let enqueued = tx_ring.fill_and_publish(&packets);

            // Submit one SendBundle SQE for all enqueued packets.
            if enqueued > 0 {
                let entry = io_uring::opcode::SendBundle::new(FIXED_SOCKET, TX_BUF_GROUP)
                    .build()
                    .user_data(OP_BUNDLE | enqueued as u64);

                // SAFETY: TX buffer ring is registered and entries are valid.
                let push_result = unsafe { self.ring.submission().push(&entry) };
                if push_result.is_err() {
                    // SQ full — reclaim ring and fall back.
                    let (_, unsent) = tx_ring.complete(0);
                    let _ = tx_ring.reset(&self.ring.submitter());
                    for pkt in unsent {
                        self.pending_tx.push_back(pkt);
                    }
                    reactor_metrics::record_io_uring_sq_full_event();
                } else {
                    reactor_metrics::record_io_uring_submitted_sqes(1);
                    reactor_metrics::record_io_uring_tx_datagrams_submitted(enqueued);
                    // Flush the SQE so the kernel processes it before the next submit_sends.
                    reactor_metrics::record_io_uring_submit_call();
                    let _ = self.ring.submit();
                }
            }

            // Overflow packets that didn't fit in the ring: fall back to GSO/SendMsg.
            let mut overflow_sqes = 0usize;
            if enqueued < packets.len() {
                let overflow: Vec<TxDatagram> = packets.into_iter().skip(enqueued).collect();
                if self.gso_supported && overflow.len() > 1 {
                    overflow_sqes += self.submit_sends_gso(overflow)?;
                } else {
                    for pkt in overflow {
                        self.pending_tx.push_back(pkt);
                    }
                }
            }

            overflow_sqes += self.submit_pending_tx()?;

            // Flush overflow SQEs to the kernel immediately.
            if overflow_sqes > 0 {
                reactor_metrics::record_io_uring_submit_call();
                let _ = self.ring.submit();
            }
            Ok(())
        }

        /// Process pending CQEs to reclaim the TX buffer ring.
        fn drain_bundle_cqes(&mut self) {
            let mut needs_reset = false;
            // Check for completed bundle CQEs without blocking.
            for cqe in self.ring.completion() {
                let user_data = cqe.user_data();
                let op = user_data & OP_MASK;
                if op == OP_BUNDLE {
                    let result = cqe.result();
                    if let Some(ref mut tx_ring) = self.tx_buf_ring {
                        let (consumed, unsent) = if result > 0 {
                            tx_ring.complete(result as usize)
                        } else {
                            tx_ring.complete(0)
                        };
                        reactor_metrics::record_io_uring_tx_datagrams_completed(consumed);
                        if !unsent.is_empty() {
                            for pkt in unsent {
                                self.pending_tx.push_back(pkt);
                            }
                            needs_reset = true;
                        }
                    }
                }
            }
            if needs_reset {
                if let Some(ref mut tx_ring) = self.tx_buf_ring {
                    let _ = tx_ring.reset(&self.ring.submitter());
                }
            }
        }

        /// Group packets into GSO batches and submit as SQEs directly.
        /// Packets that don't fit into available slots are put back into pending_tx.
        /// Returns the number of SQEs pushed to the submission ring.
        fn submit_sends_gso(&mut self, packets: Vec<TxDatagram>) -> io::Result<usize> {
            let batches = group_for_gso(packets);
            log::trace!(
                "io_uring::submit_sends_gso: {} packets -> {} batches, tx_in_flight={} pending_tx={} tid={:?}",
                batches
                    .iter()
                    .map(|b| b.data.len() / b.segment_size as usize)
                    .sum::<usize>(),
                batches.len(),
                self.tx_in_flight,
                self.pending_tx.len(),
                std::thread::current().id(),
            );
            let mut sqes_pushed = 0usize;
            for batch in batches {
                let Some(idx) = self.tx_slots.iter().position(|s| !s.in_flight) else {
                    // No free slot — split batch back into individual packets.
                    let seg = batch.segment_size as usize;
                    for chunk in batch.data.chunks(seg) {
                        self.pending_tx.push_back(TxDatagram {
                            data: chunk.to_vec(),
                            to: batch.to,
                        });
                    }
                    reactor_metrics::record_io_uring_pending_tx(self.pending_tx.len());
                    continue;
                };

                // Only attach UDP_SEGMENT cmsg when the batch has >1 segment.
                // Single-packet batches sent with UDP_SEGMENT can trigger EMSGSIZE
                // when the segment size exceeds the path MTU.
                if batch.data.len() > batch.segment_size as usize {
                    self.tx_slots[idx].prepare_gso(batch.data, batch.to, batch.segment_size);
                } else {
                    self.tx_slots[idx].prepare(TxDatagram {
                        data: batch.data,
                        to: batch.to,
                    });
                }
                let slot = &mut self.tx_slots[idx];
                let entry = io_uring::opcode::SendMsg::new(
                    FIXED_SOCKET,
                    slot.msg.as_mut() as *mut libc::msghdr,
                )
                .build()
                .user_data(OP_SEND | idx as u64);

                // SAFETY: tx slot buffers have stable addresses while in flight.
                let push_result = unsafe { self.ring.submission().push(&entry) };
                if push_result.is_err() {
                    // SQ full — split back to pending.
                    let seg = self.tx_slots[idx].gso_segment_size as usize;
                    let peer = self.tx_slots[idx].peer;
                    let data = self.tx_slots[idx].recycle_buffer();
                    for chunk in data.chunks(seg) {
                        self.pending_tx.push_back(TxDatagram {
                            data: chunk.to_vec(),
                            to: peer,
                        });
                    }
                    reactor_metrics::record_io_uring_sq_full_event();
                    break;
                }

                sqes_pushed += 1;
                self.tx_in_flight += 1;
                self.tx_bytes_in_flight += self.tx_slots[idx].data.len();
                reactor_metrics::record_io_uring_submitted_sqes(1);
                reactor_metrics::record_io_uring_tx_in_flight(self.tx_in_flight);
                reactor_metrics::record_io_uring_tx_datagrams_submitted(1);
            }
            Ok(sqes_pushed)
        }

        /// Drain pending_tx queue into SQEs. Returns the number of SQEs pushed.
        /// Stops when slots are exhausted OR tx_bytes_in_flight exceeds
        /// tx_bytes_cap (peer receive-buffer estimate).
        fn submit_pending_tx(&mut self) -> io::Result<usize> {
            let mut submitted = 0usize;
            while let Some(packet) = self.pending_tx.pop_front() {
                // Backpressure: don't push more if we've already sent more than
                // the peer can likely buffer.  This prevents kernel-level drops
                // on the receiver when SO_RCVBUF is small.
                if self.tx_bytes_in_flight >= self.tx_bytes_cap && self.tx_in_flight > 0 {
                    self.pending_tx.push_front(packet);
                    log::warn!(
                        "io_uring::submit_pending_tx BYTES_CAP: bytes={}/{} in_flight={} pending={}",
                        self.tx_bytes_in_flight,
                        self.tx_bytes_cap,
                        self.tx_in_flight,
                        self.pending_tx.len(),
                    );
                    break;
                }

                let Some(idx) = self.tx_slots.iter().position(|slot| !slot.in_flight) else {
                    self.pending_tx.push_front(packet);
                    reactor_metrics::record_io_uring_pending_tx(self.pending_tx.len());
                    break;
                };

                let pkt_bytes = packet.data.len();
                self.tx_slots[idx].prepare(packet);
                let slot = &mut self.tx_slots[idx];
                let entry = io_uring::opcode::SendMsg::new(
                    FIXED_SOCKET,
                    slot.msg.as_mut() as *mut libc::msghdr,
                )
                .build()
                .user_data(OP_SEND | idx as u64);

                // SAFETY: tx slot buffers have stable addresses while in flight.
                let push_result = unsafe { self.ring.submission().push(&entry) };
                if push_result.is_err() {
                    let packet = self.tx_slots[idx].take_packet();
                    self.pending_tx.push_front(packet);
                    reactor_metrics::record_io_uring_sq_full_event();
                    break;
                }

                submitted += 1;
                self.tx_in_flight += 1;
                self.tx_bytes_in_flight += pkt_bytes;
                reactor_metrics::record_io_uring_tx_in_flight(self.tx_in_flight);
            }

            if submitted > 0 {
                reactor_metrics::record_io_uring_submitted_sqes(submitted);
                reactor_metrics::record_io_uring_tx_datagrams_submitted(submitted);
            }
            Ok(submitted)
        }

        fn flush_pending_tx_after_progress(&mut self) -> io::Result<()> {
            let retry_sqes = self.submit_pending_tx()?;
            if retry_sqes > 0 {
                reactor_metrics::record_io_uring_submit_call();
                if let Err(err) = self.ring.submit() {
                    log::warn!(
                        "io_uring::flush_pending_tx_after_progress: ring.submit() error: {err}",
                    );
                }
            }
            Ok(())
        }

        fn drain_ready_cqes_inline(&mut self) -> io::Result<usize> {
            self.cqe_buf.clear();
            self.cqe_buf.extend(self.ring.completion());
            if self.cqe_buf.is_empty() {
                return Ok(0);
            }
            self.process_cqes_inline()
        }

        /// Process CQEs already in `self.cqe_buf`. Returns the number of TX
        /// slots freed (OP_SEND completions). Handles all op types — OP_RECV
        /// results go to `deferred_rx`, OP_WAKER sets `deferred_woken`.
        /// Also flushes buffer returns and re-arms multishot recv if needed.
        fn process_cqes_inline(&mut self) -> io::Result<usize> {
            let cqe_count = self.cqe_buf.len();
            let mut tx_freed = 0usize;
            let mut bundle_needs_reset = false;

            for cqe_idx in 0..cqe_count {
                let cqe = &self.cqe_buf[cqe_idx];
                let user_data = cqe.user_data();
                let op = user_data & OP_MASK;
                let result = cqe.result();
                let flags = cqe.flags();

                match op {
                    OP_RECV => {
                        let has_more = io_uring::cqueue::more(flags);
                        if !has_more {
                            self.rx_armed = false;
                        }
                        if result > 0 {
                            if let Some(bid) = io_uring::cqueue::buffer_select(flags) {
                                let buf = self.rx_ring.buffer_data(bid);
                                let buf_len = result as usize;
                                if let Ok(parsed) = io_uring::types::RecvMsgOut::parse(
                                    &buf[..buf_len],
                                    self.rx_ring.msg.as_ref(),
                                ) {
                                    let name_data = parsed.name_data();
                                    let peer = parse_sockaddr(name_data);
                                    if let Some(peer) = peer {
                                        let control = parsed.control_data();
                                        let local_ip = parse_pktinfo_cmsg(control);
                                        let local = local_ip
                                            .map(|ip| SocketAddr::new(ip, self.local_addr.port()))
                                            .unwrap_or(self.local_addr);
                                        let segment_size = parse_gro_cmsg(control);
                                        let payload = parsed.payload_data();
                                        let (data, reused) = self.rx_pool.copy_from_slice(payload);
                                        reactor_metrics::record_rx_buffer_checkout(
                                            reused,
                                            payload.len(),
                                        );
                                        self.deferred_rx.push(RxDatagram {
                                            data,
                                            peer,
                                            local,
                                            segment_size,
                                        });
                                        reactor_metrics::record_io_uring_rx_datagrams(1);
                                    }
                                }
                                self.rx_ring.stage_buffer_return(bid);
                            }
                        }
                    }
                    OP_SEND => {
                        let idx = (user_data & IDX_MASK) as usize;
                        self.tx_in_flight -= 1;
                        tx_freed += 1;
                        let slot = &mut self.tx_slots[idx];
                        self.tx_bytes_in_flight =
                            self.tx_bytes_in_flight.saturating_sub(slot.data.len());
                        if result >= 0 {
                            self.recycled_tx.push(slot.recycle_buffer());
                            reactor_metrics::record_io_uring_tx_datagrams_completed(1);
                        } else {
                            let errno = -result;
                            log::warn!(
                                "io_uring OP_SEND error (inline): idx={idx} errno={errno} gso_seg={} data_len={} in_flight={}",
                                slot.gso_segment_size,
                                slot.data.len(),
                                self.tx_in_flight,
                            );
                            if errno == libc::EAGAIN
                                || errno == libc::ENOBUFS
                                || errno == libc::EINTR
                                || (errno == libc::EMSGSIZE && slot.gso_segment_size > 0)
                            {
                                reactor_metrics::record_io_uring_retryable_send_completion();
                                let seg = slot.gso_segment_size;
                                if seg > 0 {
                                    let peer = slot.peer;
                                    let data = slot.recycle_buffer();
                                    for chunk in data.chunks(seg as usize) {
                                        self.pending_tx.push_back(TxDatagram {
                                            data: chunk.to_vec(),
                                            to: peer,
                                        });
                                    }
                                } else {
                                    self.pending_tx.push_back(slot.take_packet());
                                }
                            } else {
                                self.recycled_tx.push(slot.recycle_buffer());
                            }
                        }
                    }
                    OP_WAKER => {
                        self.deferred_woken = true;
                        reactor_metrics::record_io_uring_wake_completion();
                        unsafe {
                            libc::read(
                                self.eventfd.as_raw_fd(),
                                self.waker_buf.as_mut_ptr().cast(),
                                8,
                            );
                        }
                        let _ = self.submit_waker_read();
                    }
                    OP_BUNDLE => {
                        if let Some(ref mut tx_ring) = self.tx_buf_ring {
                            let (consumed, unsent) = if result > 0 {
                                tx_ring.complete(result as usize)
                            } else {
                                tx_ring.complete(0)
                            };
                            reactor_metrics::record_io_uring_tx_datagrams_completed(consumed);
                            if !unsent.is_empty() {
                                let retryable = result >= 0
                                    || matches!(-result, e if e == libc::EAGAIN || e == libc::ENOBUFS || e == libc::EINTR);
                                if retryable {
                                    for pkt in unsent {
                                        self.pending_tx.push_back(pkt);
                                    }
                                }
                                bundle_needs_reset = true;
                            } else if result <= 0 {
                                bundle_needs_reset = true;
                            }
                        }
                    }
                    _ => {}
                }
            }

            reactor_metrics::record_io_uring_completions(cqe_count);
            self.rx_ring.flush_buffer_returns();

            if bundle_needs_reset {
                if let Some(ref mut tx_ring) = self.tx_buf_ring {
                    let _ = tx_ring.reset(&self.ring.submitter());
                }
            }

            if !self.rx_armed {
                self.arm_multishot_recv()?;
            }

            Ok(tx_freed)
        }

        /// IORING_ENTER_GETEVENTS flag for io_uring_enter.
        const GETEVENTS: u32 = 1;

        /// Drain deferred completions to free TX slots when `pending_tx` is
        /// backed up due to slot exhaustion.
        ///
        /// With DEFER_TASKRUN, completed I/O sits in the kernel's work_llist
        /// until we call `io_uring_enter(GETEVENTS)`. The `SQ_TASKRUN` flag
        /// tells us (zero-syscall, mmap'd read) whether there's pending work.
        ///
        /// Returns the number of TX slots freed.
        fn drain_completions_for_tx(&mut self) -> io::Result<usize> {
            // Check if the kernel has deferred completions to process.
            if !self.defer_taskrun_enabled || !self.ring.submission().taskrun() {
                return Ok(0);
            }

            // Drain the work_llist into the CQ ring. This calls
            // io_uring_enter(to_submit=0, min_complete=0, flags=GETEVENTS)
            // which runs io_run_local_work → posts CQEs → returns immediately.
            // SAFETY: no SQEs submitted (to_submit=0), no blocking (min_complete=0).
            unsafe {
                self.ring
                    .submitter()
                    .enter::<libc::sigset_t>(0, 0, Self::GETEVENTS, None)?;
            }
            self.drain_ready_cqes_inline()
        }
    }

    impl DriverWaker for IoUringWaker {
        fn wake(&self) -> io::Result<()> {
            let val: u64 = 1;
            // SAFETY: eventfd is valid, val is a stack-allocated u64.
            let rc = unsafe {
                libc::write(
                    self.eventfd.as_raw_fd(),
                    &val as *const u64 as *const libc::c_void,
                    8,
                )
            };
            if rc < 0 {
                Err(io::Error::last_os_error())
            } else {
                reactor_metrics::record_io_uring_wake_write();
                Ok(())
            }
        }
    }

    /// Parse a `SocketAddr` from raw sockaddr bytes (as returned by recvmsg_out name_data).
    fn parse_sockaddr(data: &[u8]) -> Option<SocketAddr> {
        if data.len() < 2 {
            return None;
        }
        // First two bytes are the address family (sa_family_t).
        let family = u16::from_ne_bytes([data[0], data[1]]);
        if family == libc::AF_INET as u16 && data.len() >= std::mem::size_of::<libc::sockaddr_in>()
        {
            // SAFETY: data is large enough and we only read through a properly aligned cast.
            let sin: libc::sockaddr_in = unsafe { std::ptr::read_unaligned(data.as_ptr().cast()) };
            let ip = std::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
            let port = u16::from_be(sin.sin_port);
            Some(SocketAddr::from((ip, port)))
        } else if family == libc::AF_INET6 as u16
            && data.len() >= std::mem::size_of::<libc::sockaddr_in6>()
        {
            // SAFETY: data is large enough and we use read_unaligned.
            let sin6: libc::sockaddr_in6 =
                unsafe { std::ptr::read_unaligned(data.as_ptr().cast()) };
            let ip = std::net::Ipv6Addr::from(sin6.sin6_addr.s6_addr);
            let port = u16::from_be(sin6.sin6_port);
            Some(SocketAddr::from((ip, port)))
        } else {
            None
        }
    }

    fn socketaddr_to_sockaddr(
        addr: SocketAddr,
        storage: &mut libc::sockaddr_storage,
    ) -> libc::socklen_t {
        match addr {
            SocketAddr::V4(addr_v4) => {
                let sockaddr = libc::sockaddr_in {
                    sin_family: libc::AF_INET as libc::sa_family_t,
                    sin_port: addr_v4.port().to_be(),
                    sin_addr: libc::in_addr {
                        s_addr: u32::from_ne_bytes(addr_v4.ip().octets()),
                    },
                    sin_zero: [0; 8],
                };
                // SAFETY: `storage` points to valid writable storage with enough
                // space for `sockaddr_in`.
                unsafe {
                    std::ptr::write(storage as *mut _ as *mut libc::sockaddr_in, sockaddr);
                }
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t
            }
            SocketAddr::V6(addr_v6) => {
                let sockaddr = libc::sockaddr_in6 {
                    sin6_family: libc::AF_INET6 as libc::sa_family_t,
                    sin6_port: addr_v6.port().to_be(),
                    sin6_flowinfo: addr_v6.flowinfo(),
                    sin6_addr: libc::in6_addr {
                        s6_addr: addr_v6.ip().octets(),
                    },
                    sin6_scope_id: addr_v6.scope_id(),
                };
                // SAFETY: `storage` points to valid writable storage with enough
                // space for `sockaddr_in6`.
                unsafe {
                    std::ptr::write(storage as *mut _ as *mut libc::sockaddr_in6, sockaddr);
                }
                std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t
            }
        }
    }
}

#[cfg(all(target_os = "linux", feature = "bench-internals"))]
pub use inner::{IoUringDriver, IoUringWaker};
#[cfg(all(target_os = "linux", not(feature = "bench-internals")))]
pub(crate) use inner::{IoUringDriver, IoUringWaker};
