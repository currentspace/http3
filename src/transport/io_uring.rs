//! IoUringDriver: uses the `io-uring` crate for completion-based `recvmsg` on
//! Linux. TX uses synchronous `send_to` (same as kqueue) because UDP sendmsg
//! is non-blocking and io_uring adds no benefit — it just complicates backpressure.
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

    use crate::transport::{Driver, DriverWaker, PollOutcome, RxDatagram, TxDatagram};

    const RX_SLOTS: usize = 256;
    const RX_BUF_SIZE: usize = 65535;

    // user_data encoding: high byte = op type, low bytes = slot index
    const OP_RECV: u64 = 1 << 56;
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

    pub struct IoUringDriver {
        ring: io_uring::IoUring,
        socket_fd: RawFd,
        socket: std::net::UdpSocket,
        eventfd: OwnedFd,
        rx_slots: Vec<RxSlot>,
        waker_buf: Box<[u8; 8]>,
        rx_in_flight: usize,
        /// Packets that couldn't be sent because the OS UDP buffer was full.
        /// Retried at the start of each poll cycle via synchronous send_to.
        unsent: VecDeque<TxDatagram>,
        /// Scratch buffer for fallback recv_from after CQE drain.
        recv_buf: Vec<u8>,
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

            let mut driver = Self {
                ring,
                socket_fd,
                socket,
                eventfd,
                rx_slots,
                waker_buf: Box::new([0u8; 8]),
                rx_in_flight: 0,
                unsent: VecDeque::new(),
                recv_buf: vec![0u8; 65535],
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
            // Drain unsent queue at the top of each poll (socket buffer may have space now).
            self.drain_unsent();

            let wait_dur = deadline.map_or(Duration::from_millis(100), |d| {
                d.saturating_duration_since(Instant::now())
            });

            // Submit pending SQEs (replenished recvmsg + waker read).
            let _ = self.ring.submit();

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

            // Fallback: drain any packets that arrived after all recvmsg CQEs
            // were consumed. Without this, packets sit in the kernel socket buffer
            // until the next poll cycle, adding latency under burst traffic.
            // Cap at 256 to avoid starving the send path under fan-out.
            for _ in 0..256 {
                match self.socket.recv_from(&mut self.recv_buf) {
                    Ok((len, peer)) => {
                        outcome.rx.push(RxDatagram {
                            data: self.recv_buf[..len].to_vec(),
                            peer,
                        });
                    }
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }

            // Replenish RX depth — resubmit completed slots.
            self.replenish_rx()?;
            self.ring.submit()?;

            Ok(outcome)
        }

        fn submit_sends(&mut self, packets: Vec<TxDatagram>) -> io::Result<()> {
            // Synchronous send_to, same as kqueue driver.
            // io_uring sendmsg adds no benefit for non-blocking UDP — it just
            // complicates EAGAIN backpressure handling.
            for pkt in packets {
                match self.socket.send_to(&pkt.data, pkt.to) {
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                        self.unsent.push_back(pkt);
                    }
                    _ => {
                        self.recycled_tx.push(pkt.data);
                    }
                }
            }
            Ok(())
        }

        fn pending_tx_count(&self) -> usize {
            self.unsent.len()
        }

        fn drain_recycled_tx(&mut self) -> Vec<Vec<u8>> {
            std::mem::take(&mut self.recycled_tx)
        }

        fn local_addr(&self) -> io::Result<SocketAddr> {
            self.socket.local_addr()
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

        /// Retry sending queued packets. Stops at the first WouldBlock.
        fn drain_unsent(&mut self) {
            while let Some(front) = self.unsent.front() {
                match self.socket.send_to(&front.data, front.to) {
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => return,
                    Ok(_) | Err(_) => {
                        if let Some(pkt) = self.unsent.pop_front() {
                            self.recycled_tx.push(pkt.data);
                        }
                    }
                }
            }
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
}

#[cfg(target_os = "linux")]
pub(crate) use inner::{IoUringDriver, IoUringWaker};
