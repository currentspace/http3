//! IoUringDriver: uses the `io-uring` crate for completion-based UDP I/O on
//! Linux. Both RX and TX stay on `io_uring` so the fast path does not fall
//! back to readiness or synchronous socket syscalls once the driver is active.
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
    use crate::transport::{Driver, DriverWaker, PollOutcome, RuntimeDriverKind, RxDatagram, TxDatagram};

    const RX_SLOTS: usize = 256;
    const RX_BUF_SIZE: usize = 65535;
    const TX_SLOTS: usize = 256;

    // user_data encoding: high byte = op type, low bytes = slot index
    const OP_RECV: u64 = 1 << 56;
    const OP_SEND: u64 = 2 << 56;
    const OP_WAKER: u64 = 3 << 56;
    const OP_MASK: u64 = 0xFF << 56;
    const IDX_MASK: u64 = (1 << 56) - 1;

    /// A single recvmsg operation slot. All fields are heap-allocated (Box)
    /// to guarantee stable addresses while the SQE is in-flight.
    struct RxSlot {
        buf: Box<[u8; RX_BUF_SIZE]>,
        addr: Box<libc::sockaddr_storage>,
        iov: Box<libc::iovec>,
        msg: Box<libc::msghdr>,
        in_flight: bool,
    }

    impl RxSlot {
        fn new() -> Self {
            let mut slot = Self {
                buf: Box::new([0u8; RX_BUF_SIZE]),
                // SAFETY: zeroed sockaddr_storage is valid (all-zeros family = AF_UNSPEC).
                addr: Box::new(unsafe { std::mem::zeroed() }),
                iov: Box::new(libc::iovec {
                    iov_base: std::ptr::null_mut(),
                    iov_len: 0,
                }),
                // SAFETY: zeroed msghdr is valid (null pointers, zero lengths).
                msg: Box::new(unsafe { std::mem::zeroed() }),
                in_flight: false,
            };
            // Fix up pointers — safe because Box addresses are stable.
            slot.iov.iov_base = slot.buf.as_mut_ptr().cast();
            slot.iov.iov_len = RX_BUF_SIZE;
            slot.msg.msg_name = (slot.addr.as_mut() as *mut libc::sockaddr_storage).cast();
            slot.msg.msg_namelen =
                std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
            slot.msg.msg_iov = slot.iov.as_mut() as *mut libc::iovec;
            slot.msg.msg_iovlen = 1;
            slot
        }

        fn reset(&mut self) {
            self.msg.msg_namelen =
                std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
            self.iov.iov_len = RX_BUF_SIZE;
            self.in_flight = false;
        }
    }

    /// A single sendmsg operation slot. Like [`RxSlot`], all kernel-visible
    /// pointers live behind `Box` so they remain stable while an SQE is in
    /// flight.
    struct TxSlot {
        data: Vec<u8>,
        peer: SocketAddr,
        addr: Box<libc::sockaddr_storage>,
        iov: Box<libc::iovec>,
        msg: Box<libc::msghdr>,
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
            self.in_flight = true;
        }

        fn take_packet(&mut self) -> TxDatagram {
            self.in_flight = false;
            TxDatagram {
                data: std::mem::take(&mut self.data),
                to: self.peer,
            }
        }

        fn recycle_buffer(&mut self) -> Vec<u8> {
            self.in_flight = false;
            std::mem::take(&mut self.data)
        }
    }

    pub struct IoUringDriver {
        ring: io_uring::IoUring,
        socket_fd: RawFd,
        socket: std::net::UdpSocket,
        eventfd: OwnedFd,
        rx_slots: Vec<RxSlot>,
        tx_slots: Vec<TxSlot>,
        waker_buf: Box<[u8; 8]>,
        rx_in_flight: usize,
        tx_in_flight: usize,
        /// Packets waiting for an available TX slot or a retryable completion.
        pending_tx: VecDeque<TxDatagram>,
        /// Buffers from successfully sent packets, ready for pool recycling.
        recycled_tx: Vec<Vec<u8>>,
    }

    // SAFETY: IoUringDriver is created on the main thread and moved to the worker
    // thread before any I/O occurs. The raw pointers inside RxSlot (msghdr, iovec)
    // point to co-located Box allocations that move with the driver. The driver is
    // single-threaded after the move — no concurrent access.
    unsafe impl Send for IoUringDriver {}

    #[derive(Clone)]
    pub struct IoUringWaker {
        eventfd: Arc<OwnedFd>,
    }

    impl Driver for IoUringDriver {
        type Waker = IoUringWaker;

        fn new(socket: std::net::UdpSocket) -> io::Result<(Self, Self::Waker)> {
            let ring = io_uring::IoUring::new(512)?;
            let socket_fd = socket.as_raw_fd();

            // Create eventfd for wakeup
            // SAFETY: eventfd with EFD_NONBLOCK returns a valid fd or -1.
            let efd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK) };
            if efd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: efd is a valid fd from successful eventfd() call.
            let eventfd = unsafe { OwnedFd::from_raw_fd(efd) };

            let rx_slots: Vec<RxSlot> = (0..RX_SLOTS).map(|_| RxSlot::new()).collect();
            let tx_slots: Vec<TxSlot> = (0..TX_SLOTS).map(|_| TxSlot::new()).collect();

            let mut driver = Self {
                ring,
                socket_fd,
                socket,
                eventfd,
                rx_slots,
                tx_slots,
                waker_buf: Box::new([0u8; 8]),
                rx_in_flight: 0,
                tx_in_flight: 0,
                pending_tx: VecDeque::new(),
                recycled_tx: Vec::new(),
            };

            // Submit initial recvmsg SQEs for all RX slots
            driver.replenish_rx()?;
            // Submit eventfd read for waker
            driver.submit_waker_read()?;
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
            // Submit any queued TX before blocking so send completions can wake the loop.
            self.submit_pending_tx()?;

            let wait_dur = deadline.map_or(Duration::from_millis(100), |d| {
                d.saturating_duration_since(Instant::now())
            });

            // Submit pending SQEs (replenished recvmsg + waker read).
            self.ring.submit()?;

            // Wait for at least 1 CQE with timeout.
            let ts = io_uring::types::Timespec::new()
                .sec(wait_dur.as_secs())
                .nsec(wait_dur.subsec_nanos());
            let args = io_uring::types::SubmitArgs::new().timespec(&ts);
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

            // Process all available CQEs.
            let cq = self.ring.completion();
            let cqes: Vec<io_uring::cqueue::Entry> = cq.collect();

            if cqes.is_empty() {
                outcome.timer_expired = true;
            }

            for cqe in cqes {
                let user_data = cqe.user_data();
                let op = user_data & OP_MASK;
                let idx = (user_data & IDX_MASK) as usize;
                let result = cqe.result();

                match op {
                    OP_RECV => {
                        self.rx_in_flight -= 1;
                        let slot = &mut self.rx_slots[idx];
                        slot.in_flight = false;

                        if result > 0 {
                            let peer = sockaddr_to_socketaddr(
                                slot.addr.as_ref(),
                                slot.msg.msg_namelen,
                            );
                            if let Some(peer) = peer {
                                let len = result as usize;
                                let mut data = vec![0u8; len];
                                data.copy_from_slice(&slot.buf[..len]);
                                outcome.rx.push(RxDatagram { data, peer });
                            }
                        }
                    }
                    OP_SEND => {
                        self.tx_in_flight -= 1;
                        let slot = &mut self.tx_slots[idx];
                        if result >= 0 {
                            self.recycled_tx.push(slot.recycle_buffer());
                        } else {
                            let errno = -result;
                            if errno == libc::EAGAIN || errno == libc::ENOBUFS || errno == libc::EINTR {
                                reactor_metrics::record_io_uring_retryable_send_completion();
                                self.pending_tx.push_back(slot.take_packet());
                                reactor_metrics::record_io_uring_pending_tx(self.pending_tx.len());
                            } else {
                                self.recycled_tx.push(slot.recycle_buffer());
                            }
                        }
                    }
                    OP_WAKER => {
                        outcome.woken = true;
                        // Drain eventfd counter.
                        // SAFETY: reading 8 bytes from a valid eventfd.
                        unsafe {
                            libc::read(
                                self.eventfd.as_raw_fd(),
                                self.waker_buf.as_mut_ptr().cast(),
                                8,
                            );
                        }
                        // Resubmit waker read and submit immediately so it's
                        // ready before the next submit_with_args blocks.
                        let _ = self.submit_waker_read();
                        let _ = self.ring.submit();
                    }
                    _ => {}
                }
            }

            // Replenish RX depth — resubmit completed slots.
            self.replenish_rx()?;
            self.submit_pending_tx()?;
            self.ring.submit()?;

            Ok(outcome)
        }

        fn submit_sends(&mut self, packets: Vec<TxDatagram>) -> io::Result<()> {
            for pkt in packets {
                self.pending_tx.push_back(pkt);
                reactor_metrics::record_io_uring_pending_tx(self.pending_tx.len());
            }
            self.submit_pending_tx()?;
            let _ = self.ring.submit();
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
        fn replenish_rx(&mut self) -> io::Result<()> {
            for i in 0..self.rx_slots.len() {
                if self.rx_slots[i].in_flight {
                    continue;
                }
                self.rx_slots[i].reset();
                self.rx_slots[i].in_flight = true;

                let slot = &mut self.rx_slots[i];
                let entry = io_uring::opcode::RecvMsg::new(
                    io_uring::types::Fd(self.socket_fd),
                    slot.msg.as_mut() as *mut libc::msghdr,
                )
                .build()
                .user_data(OP_RECV | i as u64);

                // SAFETY: slot buffers have stable Box addresses. in_flight prevents reuse.
                unsafe {
                    self.ring.submission().push(&entry).map_err(|_| {
                        io::Error::new(io::ErrorKind::Other, "SQ full")
                    })?;
                }
                self.rx_in_flight += 1;
                reactor_metrics::record_io_uring_rx_in_flight(self.rx_in_flight);
            }
            Ok(())
        }

        fn submit_waker_read(&mut self) -> io::Result<()> {
            let entry = io_uring::opcode::Read::new(
                io_uring::types::Fd(self.eventfd.as_raw_fd()),
                self.waker_buf.as_mut_ptr(),
                8,
            )
            .build()
            .user_data(OP_WAKER);

            // SAFETY: waker_buf is a stable Box address. Only one read is in flight at a time.
            unsafe {
                self.ring.submission().push(&entry).map_err(|_| {
                    io::Error::new(io::ErrorKind::Other, "SQ full")
                })?;
            }
            Ok(())
        }

        fn submit_pending_tx(&mut self) -> io::Result<()> {
            while let Some(packet) = self.pending_tx.pop_front() {
                let Some(idx) = self.tx_slots.iter().position(|slot| !slot.in_flight) else {
                    self.pending_tx.push_front(packet);
                    reactor_metrics::record_io_uring_pending_tx(self.pending_tx.len());
                    break;
                };

                self.tx_slots[idx].prepare(packet);
                let slot = &mut self.tx_slots[idx];
                let entry = io_uring::opcode::SendMsg::new(
                    io_uring::types::Fd(self.socket_fd),
                    slot.msg.as_mut() as *mut libc::msghdr,
                )
                .build()
                .user_data(OP_SEND | idx as u64);

                // SAFETY: tx slot buffers have stable addresses while in flight.
                let push_result = unsafe { self.ring.submission().push(&entry) };
                if push_result.is_err() {
                    let packet = self.tx_slots[idx].take_packet();
                    self.pending_tx.push_front(packet);
                    break;
                }

                self.tx_in_flight += 1;
                reactor_metrics::record_io_uring_tx_in_flight(self.tx_in_flight);
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
                Ok(())
            }
        }
    }

    fn sockaddr_to_socketaddr(
        addr: &libc::sockaddr_storage,
        len: libc::socklen_t,
    ) -> Option<SocketAddr> {
        if len as usize >= std::mem::size_of::<libc::sockaddr_in>()
            && i32::from(addr.ss_family) == libc::AF_INET
        {
            // SAFETY: ss_family is AF_INET and len is sufficient.
            let sin: &libc::sockaddr_in = unsafe { &*(addr as *const _ as *const _) };
            let ip = std::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
            let port = u16::from_be(sin.sin_port);
            Some(SocketAddr::from((ip, port)))
        } else if len as usize >= std::mem::size_of::<libc::sockaddr_in6>()
            && i32::from(addr.ss_family) == libc::AF_INET6
        {
            // SAFETY: ss_family is AF_INET6 and len is sufficient.
            let sin6: &libc::sockaddr_in6 = unsafe { &*(addr as *const _ as *const _) };
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

#[cfg(target_os = "linux")]
pub(crate) use inner::{IoUringDriver, IoUringWaker};
