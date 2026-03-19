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

    use crate::reactor_metrics;
    use crate::transport::{Driver, DriverWaker, PollOutcome, RuntimeDriverKind, RxDatagram, TxDatagram, group_for_gso};
    use crate::transport::socket::{CMSG_CONTROL_LEN, set_pktinfo, parse_pktinfo_cmsg, probe_gso, build_gso_cmsg};

    /// Number of provided buffers in the RX buffer ring.
    const RX_RING_SIZE: u16 = 256;
    /// Each buffer must hold a full UDP datagram + the recvmsg_out header + sockaddr.
    /// Header: io_uring_recvmsg_out (16 bytes) + sockaddr_storage (128 bytes) = 144 bytes overhead.
    const RX_BUF_OVERHEAD: usize = std::mem::size_of::<libc::sockaddr_storage>()
        + 16 // io_uring_recvmsg_out header
        + CMSG_CONTROL_LEN;
    const RX_BUF_SIZE: usize = 65535 + RX_BUF_OVERHEAD;
    const TX_SLOTS: usize = 256;

    const OP_RECV: u64 = 1 << 56;
    const OP_SEND: u64 = 2 << 56;
    const OP_WAKER: u64 = 3 << 56;
    const OP_MASK: u64 = 0xFF << 56;
    const IDX_MASK: u64 = (1 << 56) - 1;

    const BUF_GROUP: u16 = 0;

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
                submitter.register_buf_ring(
                    ring_ptr as u64,
                    RX_RING_SIZE,
                    BUF_GROUP,
                )?;
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
            msg.msg_namelen =
                std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
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
        tx_slots: Vec<TxSlot>,
        waker_buf: Box<[u8; 8]>,
        tx_in_flight: usize,
        pending_tx: VecDeque<TxDatagram>,
        recycled_tx: Vec<Vec<u8>>,
        cqe_buf: Vec<io_uring::cqueue::Entry>,
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
            // Try optimal flags first (kernel ≥6.1), fall back gracefully.
            let ring = io_uring::IoUring::builder()
                .setup_coop_taskrun()
                .setup_single_issuer()
                .setup_defer_taskrun()
                .setup_cqsize(4096)
                .build(128)
                .or_else(|_| {
                    // Fallback: older kernel without DEFER_TASKRUN support.
                    io_uring::IoUring::builder()
                        .setup_cqsize(4096)
                        .build(128)
                })
                .or_else(|_| {
                    // Minimal fallback: no optional flags.
                    io_uring::IoUring::new(128)
                })?;
            let socket_fd = socket.as_raw_fd();
            let local_addr = socket.local_addr()?;
            let gso_supported = probe_gso(&socket);
            set_pktinfo(&socket);

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
                .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("register_files: {e}")))?;

            // Set up provided buffer ring for multishot recvmsg.
            let rx_ring = RxBufferRing::new(&ring.submitter())?;

            let tx_slots: Vec<TxSlot> = (0..TX_SLOTS).map(|_| TxSlot::new()).collect();

            let mut driver = Self {
                ring,
                socket_fd,
                socket,
                local_addr,
                gso_supported,
                eventfd,
                rx_ring,
                rx_armed: false,
                tx_slots,
                waker_buf: Box::new([0u8; 8]),
                tx_in_flight: 0,
                pending_tx: VecDeque::new(),
                recycled_tx: Vec::new(),
                cqe_buf: Vec::with_capacity(512),
            };

            // Arm multishot recvmsg and waker read.
            driver.arm_multishot_recv()?;
            driver.submit_waker_read()?;
            reactor_metrics::record_io_uring_submit_call();
            driver.ring.submit()?;

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
                rx: Vec::new(),
                woken: false,
                timer_expired: false,
            };

            if deadline.is_some_and(|d| Instant::now() >= d) {
                outcome.timer_expired = true;
            }

            // Drain CQEs into reusable buffer.
            self.cqe_buf.clear();
            self.cqe_buf.extend(self.ring.completion());
            let cqe_count = self.cqe_buf.len();

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
                                        let payload = parsed.payload_data();
                                        outcome.rx.push(RxDatagram {
                                            data: payload.to_vec(),
                                            peer,
                                            local,
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
                        if result >= 0 {
                            self.recycled_tx.push(slot.recycle_buffer());
                            reactor_metrics::record_io_uring_tx_datagrams_completed(1);
                        } else {
                            let errno = -result;
                            if errno == libc::EAGAIN || errno == libc::ENOBUFS || errno == libc::EINTR {
                                reactor_metrics::record_io_uring_retryable_send_completion();
                                // GSO batch: split back into individual packets for retry.
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
                    _ => {}
                }
            }
            reactor_metrics::record_io_uring_completions(cqe_count);

            // Single fence to publish all returned buffers to the kernel.
            self.rx_ring.flush_buffer_returns();

            if cqe_count == 0 {
                reactor_metrics::record_io_uring_timeout_poll();
                outcome.timer_expired = true;
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
            if self.gso_supported && packets.len() > 1 {
                self.submit_sends_gso(packets)?;
            } else {
                for pkt in packets {
                    self.pending_tx.push_back(pkt);
                    reactor_metrics::record_io_uring_pending_tx(self.pending_tx.len());
                }
            }
            self.submit_pending_tx()?;
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
    }

    impl IoUringDriver {
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
            let entry = io_uring::opcode::Read::new(
                FIXED_EVENTFD,
                self.waker_buf.as_mut_ptr(),
                8,
            )
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

        /// Group packets into GSO batches and submit as SQEs directly.
        /// Packets that don't fit into available slots are put back into pending_tx.
        fn submit_sends_gso(&mut self, packets: Vec<TxDatagram>) -> io::Result<()> {
            let batches = group_for_gso(packets);
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

                self.tx_slots[idx].prepare_gso(batch.data, batch.to, batch.segment_size);
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

                self.tx_in_flight += 1;
                reactor_metrics::record_io_uring_submitted_sqes(1);
                reactor_metrics::record_io_uring_tx_in_flight(self.tx_in_flight);
                reactor_metrics::record_io_uring_tx_datagrams_submitted(1);
            }
            Ok(())
        }

        fn submit_pending_tx(&mut self) -> io::Result<()> {
            let mut submitted = 0usize;
            while let Some(packet) = self.pending_tx.pop_front() {
                let Some(idx) = self.tx_slots.iter().position(|slot| !slot.in_flight) else {
                    self.pending_tx.push_front(packet);
                    reactor_metrics::record_io_uring_pending_tx(self.pending_tx.len());
                    break;
                };

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
                reactor_metrics::record_io_uring_tx_in_flight(self.tx_in_flight);
            }

            if submitted > 0 {
                reactor_metrics::record_io_uring_submitted_sqes(submitted);
                reactor_metrics::record_io_uring_tx_datagrams_submitted(submitted);
            }
            Ok(())
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
        if family == libc::AF_INET as u16
            && data.len() >= std::mem::size_of::<libc::sockaddr_in>()
        {
            // SAFETY: data is large enough and we only read through a properly aligned cast.
            let sin: libc::sockaddr_in = unsafe {
                std::ptr::read_unaligned(data.as_ptr().cast())
            };
            let ip = std::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
            let port = u16::from_be(sin.sin_port);
            Some(SocketAddr::from((ip, port)))
        } else if family == libc::AF_INET6 as u16
            && data.len() >= std::mem::size_of::<libc::sockaddr_in6>()
        {
            // SAFETY: data is large enough and we use read_unaligned.
            let sin6: libc::sockaddr_in6 = unsafe {
                std::ptr::read_unaligned(data.as_ptr().cast())
            };
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
