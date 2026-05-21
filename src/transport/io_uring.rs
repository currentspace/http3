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
    use crate::proof_core::ring_layout;
    use crate::reactor_metrics;
    use crate::transport::socket::{
        CMSG_CONTROL_LEN, build_gso_cmsg, enable_gro, parse_recv_cmsgs, probe_gso, set_pktinfo,
    };
    use crate::transport::{
        Driver, DriverWaker, GsoBatch, PollOutcome, RuntimeDriverKind, RxDatagram, TxDatagram,
        group_for_gso,
    };
    use crate::unsafe_boundary::ProvidedBufferId;

    /// Number of provided buffers in the RX buffer ring.
    /// 512 gives enough headroom for burst receives between poll() calls —
    /// the kernel silently drops datagrams when the ring is exhausted.
    const RX_RING_SIZE: u16 = 512;
    /// Flush returned buffers to the kernel every N CQEs to prevent ring exhaustion.
    const RX_FLUSH_INTERVAL: u16 = 32;
    /// Each buffer must hold a full UDP datagram + the recvmsg_out header + sockaddr.
    /// Header: io_uring_recvmsg_out (16 bytes) + sockaddr_storage (128 bytes) = 144 bytes overhead.
    const RX_BUF_OVERHEAD: usize = std::mem::size_of::<libc::sockaddr_storage>()
        + 16 // io_uring_recvmsg_out header
        + CMSG_CONTROL_LEN;
    const USER_RX_BUF_SIZE: usize = 65535;
    const RX_BUF_SIZE: usize = 65535 + RX_BUF_OVERHEAD;
    const TX_SLOTS: usize = 256;
    const SQ_RING_ENTRIES: u32 = TX_SLOTS as u32;
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

    fn send_bundle_prefix_len(packets: &[TxDatagram]) -> usize {
        let mut count = 0usize;
        for packet in packets.iter().take(TX_RING_ENTRIES as usize) {
            if packet.payload_len() > TX_BUF_ENTRY_SIZE {
                break;
            }
            count += 1;
        }
        count
    }

    trait CompletionView {
        fn completion_user_data(&self) -> u64;
    }

    impl CompletionView for io_uring::cqueue::Entry {
        fn completion_user_data(&self) -> u64 {
            self.user_data()
        }
    }

    fn split_bundle_completions<C>(cqes: impl IntoIterator<Item = C>) -> (Vec<C>, Vec<C>)
    where
        C: CompletionView,
    {
        let mut bundle = Vec::new();
        let mut non_bundle = Vec::new();
        for cqe in cqes {
            if cqe.completion_user_data() & OP_MASK == OP_BUNDLE {
                bundle.push(cqe);
            } else {
                non_bundle.push(cqe);
            }
        }
        (bundle, non_bundle)
    }

    #[derive(Debug, Default)]
    struct WakerReadState {
        armed: bool,
    }

    impl WakerReadState {
        fn should_submit(&self) -> bool {
            !self.armed
        }

        fn mark_submitted(&mut self) {
            self.armed = true;
        }

        fn mark_completed(&mut self) {
            self.armed = false;
        }
    }

    /// Compile-time-optional trace logging for diagnosing driver-level issues.
    /// Zero cost when `driver-tracing` feature is disabled (default).
    macro_rules! driver_trace {
        ($($arg:tt)*) => {
            #[cfg(feature = "driver-tracing")]
            log::trace!($($arg)*);
        };
    }

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
                let offset = ring_layout::provided_buffer_offset(i as u16, RX_BUF_SIZE)
                    .expect("RX provided-buffer offset overflow");
                let buf_addr = unsafe { buf_base.add(offset) };
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
        fn stage_buffer_return(&mut self, bid: ProvidedBufferId) {
            let bid = bid.get();
            let entries = self.ring_ptr.cast::<io_uring::types::BufRingEntry>();
            let slot = (self.tail % RX_RING_SIZE) as usize;
            let entry = unsafe { &mut *entries.add(slot) };

            let offset = ring_layout::provided_buffer_offset(bid, RX_BUF_SIZE)
                .expect("RX provided-buffer offset overflow");
            debug_assert!(ring_layout::provided_buffer_range_in_layout(
                bid,
                RX_RING_SIZE,
                RX_BUF_SIZE
            ));
            let buf_addr = unsafe { self.buf_base.add(offset) };
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
        fn buffer_data(&self, bid: ProvidedBufferId) -> &[u8] {
            let bid = bid.get();
            let offset = ring_layout::provided_buffer_offset(bid, RX_BUF_SIZE)
                .expect("RX provided-buffer offset overflow");
            debug_assert!(ring_layout::provided_buffer_range_in_layout(
                bid,
                RX_RING_SIZE,
                RX_BUF_SIZE
            ));
            // SAFETY: ProvidedBufferId proves bid < RX_RING_SIZE; buffer region is valid.
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
            let n = send_bundle_prefix_len(packets);
            if n == 0 {
                self.in_flight = false;
                self.in_flight_count = 0;
                self.in_flight_lengths.clear();
                return 0;
            }

            let entries = self.ring_ptr.cast::<io_uring::types::BufRingEntry>();

            self.in_flight_lengths.clear();
            for i in 0..n {
                let slot = (self.tail % TX_RING_ENTRIES) as usize;
                let len = packets[i].payload_len();
                let buf_offset = slot * TX_BUF_ENTRY_SIZE;

                // SAFETY: slot is in [0, TX_RING_ENTRIES), buf_offset is valid.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        packets[i].payload().as_ptr(),
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
                unsent.push(TxDatagram::from_payload(data, self.connected_peer, None));
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
        payload_len: usize,
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
                payload_len: 0,
                gso_segment_size: 0,
                in_flight: false,
            };
            slot.msg.msg_name = (slot.addr.as_mut() as *mut libc::sockaddr_storage).cast();
            slot.msg.msg_iov = slot.iov.as_mut() as *mut libc::iovec;
            slot.msg.msg_iovlen = 1;
            slot
        }

        fn prepare(&mut self, packet: TxDatagram) {
            self.payload_len = packet.payload_len();
            self.data = packet.data;
            self.peer = packet.to;
            self.iov.iov_base = self.data.as_mut_ptr().cast();
            self.iov.iov_len = self.payload_len;
            self.msg.msg_namelen = socketaddr_to_sockaddr(self.peer, self.addr.as_mut());
            self.msg.msg_control = std::ptr::null_mut();
            self.msg.msg_controllen = 0;
            self.msg.msg_flags = 0;
            self.gso_segment_size = 0;
            self.in_flight = true;
        }

        fn prepare_gso(
            &mut self,
            data: Vec<u8>,
            payload_len: usize,
            to: SocketAddr,
            segment_size: u16,
        ) {
            self.data = data;
            self.payload_len = payload_len;
            self.peer = to;
            self.iov.iov_base = self.data.as_mut_ptr().cast();
            self.iov.iov_len = self.payload_len;
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
            let payload_len = std::mem::take(&mut self.payload_len);
            TxDatagram::new(std::mem::take(&mut self.data), payload_len, self.peer, None)
        }

        fn recycle_buffer(&mut self) -> Vec<u8> {
            self.in_flight = false;
            self.gso_segment_size = 0;
            self.payload_len = 0;
            std::mem::take(&mut self.data)
        }
    }

    fn enqueue_retry_data(
        pending_tx: &mut VecDeque<TxDatagram>,
        data: Vec<u8>,
        payload_len: usize,
        peer: SocketAddr,
        gso_segment_size: u16,
    ) {
        if gso_segment_size == 0 {
            pending_tx.push_back(TxDatagram::new(data, payload_len, peer, None));
            return;
        }

        let seg = gso_segment_size as usize;
        for chunk in data[..payload_len].chunks(seg) {
            pending_tx.push_back(TxDatagram::from_payload(chunk.to_vec(), peer, None));
        }
    }

    fn enqueue_gso_batch_for_retry(pending_tx: &mut VecDeque<TxDatagram>, batch: GsoBatch) {
        enqueue_retry_data(
            pending_tx,
            batch.data,
            batch.payload_len,
            batch.to,
            batch.segment_size,
        );
    }

    fn requeue_tx_slot_for_retry(pending_tx: &mut VecDeque<TxDatagram>, slot: &mut TxSlot) {
        let seg = slot.gso_segment_size;
        let peer = slot.peer;
        if seg == 0 {
            pending_tx.push_back(slot.take_packet());
        } else {
            let payload_len = slot.payload_len;
            let data = slot.recycle_buffer();
            enqueue_retry_data(pending_tx, data, payload_len, peer, seg);
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
        waker_read_state: WakerReadState,
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
                .build(SQ_RING_ENTRIES)
                .map(|r| (r, true))
                .or_else(|_| {
                    // Fallback: without DEFER_TASKRUN (kernel < 6.1).
                    io_uring::IoUring::builder()
                        .setup_cqsize(4096)
                        .build(SQ_RING_ENTRIES)
                        .map(|r| (r, false))
                })
                .or_else(|_| {
                    // Minimal fallback.
                    io_uring::IoUring::new(SQ_RING_ENTRIES).map(|r| (r, false))
                })?;
            let socket_fd = socket.as_raw_fd();
            let local_addr = socket.local_addr()?;
            let gso_supported = probe_gso(&socket);
            set_pktinfo(&socket);
            // Audit finding #18: enable IP_RECVTOS / IPV6_RECVTCLASS so the
            // multishot recvmsg cmsg buffer carries the per-datagram ECN
            // code point. Telemetry only — quiche 0.28 doesn't expose ECN.
            crate::transport::socket::set_recv_ecn(&socket);
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
                waker_read_state: WakerReadState::default(),
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
            self.submit_waker_read()?;

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
            let mut staged_rx_returns = 0u16;
            for cqe_idx in 0..cqe_count {
                let cqe = &self.cqe_buf[cqe_idx];
                let user_data = cqe.user_data();
                let op = user_data & OP_MASK;
                let result = cqe.result();
                let flags = cqe.flags();

                match op {
                    OP_RECV => {
                        // Multishot recvmsg: check if more completions coming.
                        // Audit finding #6: if F_MORE is absent, rearm
                        // *immediately* to minimize the window during which
                        // no recv is posted (the kernel drops datagrams at
                        // the socket buffer if no provided-buffer SQE is
                        // available). The trailing rearm at the end of the
                        // CQE loop is kept as a fallback in case the SQ is
                        // full at this moment.
                        let has_more = io_uring::cqueue::more(flags);
                        if !has_more {
                            driver_trace!("io_uring: multishot disarmed (no IORING_CQE_F_MORE)");
                            if self.arm_multishot_recv().is_err() {
                                // SQ full or other push error — fall through
                                // to the trailing rearm at the end of poll().
                                self.rx_armed = false;
                            }
                        }

                        if result > 0 {
                            if let Some(raw_bid) = io_uring::cqueue::buffer_select(flags) {
                                let Some(bid) = ProvidedBufferId::new(raw_bid, RX_RING_SIZE) else {
                                    log::error!(
                                        "io_uring recv CQE returned out-of-range bid={raw_bid} flags={flags:#x}"
                                    );
                                    continue;
                                };
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
                                        let cmsgs = parse_recv_cmsgs(control);
                                        let local = cmsgs
                                            .local_ip
                                            .map(|ip| SocketAddr::new(ip, self.local_addr.port()))
                                            .unwrap_or(self.local_addr);
                                        let segment_size = cmsgs.segment_size;
                                        // Audit #18: ECN observability.
                                        if let Some(tos) = cmsgs.tos {
                                            reactor_metrics::record_ecn_recv(
                                                crate::transport::socket::EcnCodePoint::from_tos(
                                                    tos,
                                                ),
                                            );
                                        }
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

                                // Return buffer to the ring and flush periodically
                                // to prevent ring exhaustion during large bursts.
                                self.rx_ring.stage_buffer_return(bid);
                                staged_rx_returns += 1;
                                if staged_rx_returns >= RX_FLUSH_INTERVAL {
                                    self.rx_ring.flush_buffer_returns();
                                    driver_trace!(
                                        "io_uring: flushed {staged_rx_returns} RX buffers mid-CQE"
                                    );
                                    staged_rx_returns = 0;
                                }
                            } else {
                                // result > 0 but no buffer — the kernel ran out of
                                // provided buffers and dropped the datagram.
                                // Audit finding #32: bump a metric so a steady
                                // stream of drops is observable in operator
                                // dashboards instead of buried in logs.
                                reactor_metrics::record_iouring_buf_exhausted();
                                log::warn!(
                                    "io_uring: RX buffer ring exhausted — datagram dropped \
                                     (result={result}, ring_size={RX_RING_SIZE})"
                                );
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
                            self.tx_bytes_in_flight.saturating_sub(slot.payload_len);
                        if result >= 0 {
                            if slot.gso_segment_size > 0 {
                                log::trace!(
                                    "io_uring OP_SEND GSO complete: idx={idx} result={result} seg_size={} data_len={} in_flight={}",
                                    slot.gso_segment_size,
                                    slot.payload_len,
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
                                slot.payload_len,
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
                                requeue_tx_slot_for_retry(&mut self.pending_tx, slot);
                                reactor_metrics::record_io_uring_pending_tx(self.pending_tx.len());
                            } else {
                                self.recycled_tx.push(slot.recycle_buffer());
                            }
                        }
                    }
                    OP_WAKER => {
                        self.waker_read_state.mark_completed();
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
                        self.submit_waker_read()?;
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

            // Final fence to publish any remaining returned buffers to the kernel.
            if staged_rx_returns > 0 {
                self.rx_ring.flush_buffer_returns();
                driver_trace!(
                    "io_uring: flushed final {staged_rx_returns} RX buffers after CQE loop"
                );
            }

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
                driver_trace!("io_uring: re-arming multishot recv");
                self.arm_multishot_recv()?;
            }

            // Queue pending TX — flushed by next submit_with_args.
            self.submit_pending_tx()?;

            Ok(outcome)
        }

        fn poll_without_rx(&mut self, deadline: Option<Instant>) -> io::Result<PollOutcome> {
            // io_uring is completion-based: recv CQEs may already be ready
            // because the multishot recv is armed. Drain completions so TX,
            // wakeups, and buffer returns still make progress, but hold any
            // received datagrams in `deferred_rx` for the next normal poll
            // instead of dropping them or admitting them to protocol code.
            let mut outcome = self.poll(deadline)?;
            if !outcome.rx.is_empty() {
                self.deferred_rx.extend(outcome.rx.drain(..));
            }
            Ok(PollOutcome {
                rx: Vec::new(),
                woken: outcome.woken,
                timer_expired: outcome.timer_expired,
            })
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
                    self.drain_bundle_cqes()?;
                    return self.submit_send_bundle(packets);
                }
            }
            let mut sqes_pushed = 0usize;
            if self.gso_supported && packets.len() > 1 {
                // Group into GSO batches, then route through pending_tx so
                // the SQE creation path is identical to non-GSO sends.
                let batching = group_for_gso(packets);
                self.recycled_tx.extend(batching.recycled);
                for batch in batching.batches {
                    if batch.segment_size > 0 && batch.payload_len() > batch.segment_size as usize {
                        // Multi-segment batch: needs GSO SQE with cmsg.
                        // Bytes-cap check: if we'd exceed the peer's estimated
                        // receive buffer, split to pending_tx for the drain loop.
                        if self.tx_bytes_in_flight + batch.payload_len() > self.tx_bytes_cap
                            && self.tx_in_flight > 0
                        {
                            enqueue_gso_batch_for_retry(&mut self.pending_tx, batch);
                            continue;
                        }
                        // Find a free slot directly (can't go through pending_tx).
                        let Some(idx) = self.tx_slots.iter().position(|s| !s.in_flight) else {
                            // No slot: split back to individual packets.
                            enqueue_gso_batch_for_retry(&mut self.pending_tx, batch);
                            continue;
                        };
                        self.tx_slots[idx].prepare_gso(
                            batch.data,
                            batch.payload_len,
                            batch.to,
                            batch.segment_size,
                        );
                        let slot = &mut self.tx_slots[idx];
                        let entry = io_uring::opcode::SendMsg::new(
                            FIXED_SOCKET,
                            slot.msg.as_mut() as *mut libc::msghdr,
                        )
                        .build()
                        .user_data(OP_SEND | idx as u64);
                        let push_result = unsafe { self.ring.submission().push(&entry) };
                        if push_result.is_err() {
                            requeue_tx_slot_for_retry(
                                &mut self.pending_tx,
                                &mut self.tx_slots[idx],
                            );
                            reactor_metrics::record_io_uring_sq_full_event();
                        } else {
                            self.tx_bytes_in_flight += self.tx_slots[idx].payload_len;
                            self.tx_in_flight += 1;
                            sqes_pushed += 1;
                        }
                    } else {
                        // Single-packet batch: route through pending_tx (no cmsg needed).
                        self.pending_tx.push_back(TxDatagram::new(
                            batch.data,
                            batch.payload_len,
                            batch.to,
                            None,
                        ));
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
            // Audit finding #36: env_logger::try_init under a Once so
            // concurrent worker spawns don't race the global logger init.
            // (The Once isn't strictly necessary because log::set_logger
            // is itself one-shot, but it makes the intent obvious and
            // suppresses redundant try_init calls on every worker spawn.)
            use std::sync::Once;
            static LOGGER_INIT: Once = Once::new();
            LOGGER_INIT.call_once(|| {
                let _ = env_logger::try_init();
            });
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
            if !self.waker_read_state.should_submit() {
                return Ok(());
            }

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
            self.waker_read_state.mark_submitted();
            reactor_metrics::record_io_uring_submitted_sqes(1);
            Ok(())
        }

        /// Submit packets via send bundle (connected socket, kernel ≥6.10).
        /// Falls back to GSO/SendMsg for overflow packets.
        fn submit_send_bundle(&mut self, mut packets: Vec<TxDatagram>) -> io::Result<()> {
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

            if enqueued > 0 {
                for pkt in packets.drain(..enqueued) {
                    self.recycled_tx.push(pkt.into_recycle_buffer());
                }
            }

            // Overflow packets that didn't fit in the ring: fall back to GSO/SendMsg.
            let mut overflow_sqes = 0usize;
            if !packets.is_empty() {
                let overflow = packets;
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
        fn drain_bundle_cqes(&mut self) -> io::Result<()> {
            let mut needs_reset = false;
            let (bundle_cqes, non_bundle_cqes) = split_bundle_completions(self.ring.completion());
            let bundle_count = bundle_cqes.len();

            for cqe in bundle_cqes {
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
            if bundle_count > 0 {
                reactor_metrics::record_io_uring_completions(bundle_count);
            }

            if needs_reset {
                if let Some(ref mut tx_ring) = self.tx_buf_ring {
                    let _ = tx_ring.reset(&self.ring.submitter());
                }
            }

            self.cqe_buf = non_bundle_cqes;
            if !self.cqe_buf.is_empty() {
                let _ = self.process_cqes_inline()?;
            }
            Ok(())
        }

        /// Group packets into GSO batches and submit as SQEs directly.
        /// Packets that don't fit into available slots are put back into pending_tx.
        /// Returns the number of SQEs pushed to the submission ring.
        fn submit_sends_gso(&mut self, packets: Vec<TxDatagram>) -> io::Result<usize> {
            let batching = group_for_gso(packets);
            self.recycled_tx.extend(batching.recycled);
            log::trace!(
                "io_uring::submit_sends_gso: {} packets -> {} batches, tx_in_flight={} pending_tx={} tid={:?}",
                batching
                    .batches
                    .iter()
                    .map(|b| {
                        if b.segment_size == 0 {
                            1
                        } else {
                            b.payload_len() / b.segment_size as usize
                        }
                    })
                    .sum::<usize>(),
                batching.batches.len(),
                self.tx_in_flight,
                self.pending_tx.len(),
                std::thread::current().id(),
            );
            let mut sqes_pushed = 0usize;
            let mut batches = batching.batches.into_iter();
            while let Some(batch) = batches.next() {
                let Some(idx) = self.tx_slots.iter().position(|s| !s.in_flight) else {
                    // No free slot — split batch back into individual packets.
                    enqueue_gso_batch_for_retry(&mut self.pending_tx, batch);
                    reactor_metrics::record_io_uring_pending_tx(self.pending_tx.len());
                    continue;
                };

                // Only attach UDP_SEGMENT cmsg when the batch has >1 segment.
                // Single-packet batches sent with UDP_SEGMENT can trigger EMSGSIZE
                // when the segment size exceeds the path MTU.
                if batch.segment_size > 0 && batch.payload_len() > batch.segment_size as usize {
                    self.tx_slots[idx].prepare_gso(
                        batch.data,
                        batch.payload_len,
                        batch.to,
                        batch.segment_size,
                    );
                } else {
                    self.tx_slots[idx].prepare(TxDatagram::new(
                        batch.data,
                        batch.payload_len,
                        batch.to,
                        None,
                    ));
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
                    requeue_tx_slot_for_retry(&mut self.pending_tx, &mut self.tx_slots[idx]);
                    for batch in batches {
                        enqueue_gso_batch_for_retry(&mut self.pending_tx, batch);
                    }
                    reactor_metrics::record_io_uring_sq_full_event();
                    break;
                }

                sqes_pushed += 1;
                self.tx_in_flight += 1;
                self.tx_bytes_in_flight += self.tx_slots[idx].payload_len;
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

                let pkt_bytes = packet.payload_len();
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
                        // Same inline-rearm fix as the main poll() loop.
                        // Audit finding #6.
                        let has_more = io_uring::cqueue::more(flags);
                        if !has_more {
                            if self.arm_multishot_recv().is_err() {
                                self.rx_armed = false;
                            }
                        }
                        if result > 0 {
                            if let Some(raw_bid) = io_uring::cqueue::buffer_select(flags) {
                                let Some(bid) = ProvidedBufferId::new(raw_bid, RX_RING_SIZE) else {
                                    log::error!(
                                        "io_uring recv CQE returned out-of-range bid={raw_bid} flags={flags:#x}"
                                    );
                                    continue;
                                };
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
                                        let cmsgs = parse_recv_cmsgs(control);
                                        let local = cmsgs
                                            .local_ip
                                            .map(|ip| SocketAddr::new(ip, self.local_addr.port()))
                                            .unwrap_or(self.local_addr);
                                        let segment_size = cmsgs.segment_size;
                                        // Audit #18: ECN observability.
                                        if let Some(tos) = cmsgs.tos {
                                            reactor_metrics::record_ecn_recv(
                                                crate::transport::socket::EcnCodePoint::from_tos(
                                                    tos,
                                                ),
                                            );
                                        }
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
                            self.tx_bytes_in_flight.saturating_sub(slot.payload_len);
                        if result >= 0 {
                            self.recycled_tx.push(slot.recycle_buffer());
                            reactor_metrics::record_io_uring_tx_datagrams_completed(1);
                        } else {
                            let errno = -result;
                            log::warn!(
                                "io_uring OP_SEND error (inline): idx={idx} errno={errno} gso_seg={} data_len={} in_flight={}",
                                slot.gso_segment_size,
                                slot.payload_len,
                                self.tx_in_flight,
                            );
                            if errno == libc::EAGAIN
                                || errno == libc::ENOBUFS
                                || errno == libc::EINTR
                                || (errno == libc::EMSGSIZE && slot.gso_segment_size > 0)
                            {
                                reactor_metrics::record_io_uring_retryable_send_completion();
                                requeue_tx_slot_for_retry(&mut self.pending_tx, slot);
                            } else {
                                self.recycled_tx.push(slot.recycle_buffer());
                            }
                        }
                    }
                    OP_WAKER => {
                        self.waker_read_state.mark_completed();
                        self.deferred_woken = true;
                        reactor_metrics::record_io_uring_wake_completion();
                        unsafe {
                            libc::read(
                                self.eventfd.as_raw_fd(),
                                self.waker_buf.as_mut_ptr().cast(),
                                8,
                            );
                        }
                        self.submit_waker_read()?;
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

    #[cfg(test)]
    mod tests {
        use super::*;

        #[derive(Debug, PartialEq, Eq)]
        struct TestCqe {
            user_data: u64,
            label: &'static str,
        }

        impl CompletionView for TestCqe {
            fn completion_user_data(&self) -> u64 {
                self.user_data
            }
        }

        fn test_tx_datagram(len: usize) -> TxDatagram {
            TxDatagram::from_payload(vec![7; len], "127.0.0.1:4433".parse().unwrap(), None)
        }

        #[test]
        fn split_bundle_completions_preserves_interleaved_non_bundle_cqes() {
            let (bundle, non_bundle) = split_bundle_completions([
                TestCqe {
                    user_data: OP_RECV,
                    label: "rx",
                },
                TestCqe {
                    user_data: OP_BUNDLE | 1,
                    label: "bundle-1",
                },
                TestCqe {
                    user_data: OP_SEND | 7,
                    label: "send",
                },
                TestCqe {
                    user_data: OP_WAKER,
                    label: "waker",
                },
                TestCqe {
                    user_data: OP_BUNDLE | 2,
                    label: "bundle-2",
                },
            ]);

            assert_eq!(
                bundle.iter().map(|cqe| cqe.label).collect::<Vec<_>>(),
                ["bundle-1", "bundle-2"]
            );
            assert_eq!(
                non_bundle.iter().map(|cqe| cqe.label).collect::<Vec<_>>(),
                ["rx", "send", "waker"]
            );
        }

        #[test]
        fn requeue_tx_slot_for_retry_preserves_non_gso_datagram() {
            let peer: SocketAddr = "127.0.0.1:4433".parse().unwrap();
            let mut pending = VecDeque::new();
            let mut slot = TxSlot::new();
            slot.prepare(TxDatagram::from_payload(vec![1, 2, 3], peer, None));

            requeue_tx_slot_for_retry(&mut pending, &mut slot);

            assert!(!slot.in_flight);
            assert_eq!(slot.gso_segment_size, 0);
            assert_eq!(pending.len(), 1);
            let packet = pending.pop_front().unwrap();
            assert_eq!(packet.data, vec![1, 2, 3]);
            assert_eq!(packet.to, peer);
            assert_eq!(packet.max_segment_size, None);
        }

        #[test]
        fn requeue_tx_slot_for_retry_splits_gso_datagram() {
            let peer: SocketAddr = "127.0.0.1:4433".parse().unwrap();
            let mut pending = VecDeque::new();
            let mut slot = TxSlot::new();
            slot.prepare_gso(vec![1, 2, 3, 4, 5], 5, peer, 2);

            requeue_tx_slot_for_retry(&mut pending, &mut slot);

            assert!(!slot.in_flight);
            assert_eq!(slot.gso_segment_size, 0);
            assert_eq!(pending.len(), 3);
            assert_eq!(pending.pop_front().unwrap().data, vec![1, 2]);
            assert_eq!(pending.pop_front().unwrap().data, vec![3, 4]);
            assert_eq!(pending.pop_front().unwrap().data, vec![5]);
        }

        #[test]
        fn enqueue_retry_data_accepts_zero_segment_size() {
            let peer: SocketAddr = "127.0.0.1:4433".parse().unwrap();
            let mut pending = VecDeque::new();

            enqueue_retry_data(&mut pending, Vec::new(), 0, peer, 0);

            assert_eq!(pending.len(), 1);
            let packet = pending.pop_front().unwrap();
            assert!(packet.data.is_empty());
            assert_eq!(packet.to, peer);
        }

        #[test]
        fn send_bundle_prefix_len_stops_before_oversized_datagram() {
            let packets = vec![
                test_tx_datagram(TX_BUF_ENTRY_SIZE),
                test_tx_datagram(TX_BUF_ENTRY_SIZE + 1),
                test_tx_datagram(1),
            ];

            assert_eq!(send_bundle_prefix_len(&packets), 1);
            assert_eq!(
                send_bundle_prefix_len(&[test_tx_datagram(TX_BUF_ENTRY_SIZE + 1)]),
                0
            );
        }

        #[test]
        fn send_bundle_prefix_len_caps_at_ring_entries() {
            let packets = (0..(TX_RING_ENTRIES as usize + 1))
                .map(|_| test_tx_datagram(1))
                .collect::<Vec<_>>();

            assert_eq!(send_bundle_prefix_len(&packets), TX_RING_ENTRIES as usize);
        }

        #[test]
        fn waker_read_state_tracks_single_in_flight_read() {
            let mut state = WakerReadState::default();

            assert!(state.should_submit());
            state.mark_submitted();
            assert!(!state.should_submit());
            state.mark_completed();
            assert!(state.should_submit());
        }
    }
}

#[cfg(all(target_os = "linux", feature = "bench-internals"))]
pub use inner::{IoUringDriver, IoUringWaker};
#[cfg(all(target_os = "linux", not(feature = "bench-internals")))]
pub(crate) use inner::{IoUringDriver, IoUringWaker};
