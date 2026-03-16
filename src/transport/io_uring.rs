//! IoUringDriver: uses the `io-uring` crate for real `recvmsg`/`sendmsg`
//! completion-based I/O on Linux.
//!
//! This module is only compiled on Linux (`cfg(target_os = "linux")`).
//! Pre-posted RX slots with stable-address buffers, slab-backed TX slots,
//! eventfd waker integrated into the ring.

#[cfg(target_os = "linux")]
mod inner {
    use std::io;
    use std::net::SocketAddr;
    use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use crate::transport::{Driver, DriverWaker, PollOutcome, RxDatagram, TxDatagram};

    const RX_SLOTS: usize = 64;
    const RX_BUF_SIZE: usize = 65535;

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

    /// A single sendmsg operation slot. Owns data until CQE retirement.
    struct TxSlot {
        data: Vec<u8>,
        addr: Box<libc::sockaddr_storage>,
        addr_len: libc::socklen_t,
        iov: Box<libc::iovec>,
        msg: Box<libc::msghdr>,
    }

    impl RxSlot {
        fn new() -> Self {
            let mut slot = Self {
                buf: Box::new([0u8; RX_BUF_SIZE]),
                addr: Box::new(unsafe { std::mem::zeroed() }),
                iov: Box::new(libc::iovec {
                    iov_base: std::ptr::null_mut(),
                    iov_len: 0,
                }),
                msg: Box::new(unsafe { std::mem::zeroed() }),
                in_flight: false,
            };
            // Fix up pointers — safe because Box addresses are stable
            slot.iov.iov_base = slot.buf.as_mut_ptr().cast();
            slot.iov.iov_len = RX_BUF_SIZE;
            slot.msg.msg_name = (slot.addr.as_mut() as *mut libc::sockaddr_storage).cast();
            slot.msg.msg_namelen = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
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
        tx_slots: slab::Slab<TxSlot>,
        waker_buf: Box<[u8; 8]>,
        rx_in_flight: usize,
        tx_in_flight: usize,
    }

    // SAFETY: IoUringDriver is created on the main thread and moved to the worker
    // thread before any I/O occurs. The raw pointers inside RxSlot/TxSlot (msghdr,
    // iovec) point to co-located Box allocations that move with the driver. The
    // driver is single-threaded after the move — no concurrent access.
    unsafe impl Send for IoUringDriver {}

    #[derive(Clone)]
    pub struct IoUringWaker {
        eventfd: Arc<OwnedFd>,
    }

    impl Driver for IoUringDriver {
        type Waker = IoUringWaker;

        fn new(socket: std::net::UdpSocket) -> io::Result<(Self, Self::Waker)> {
            let ring = io_uring::IoUring::new(256)?;
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
                tx_slots: slab::Slab::with_capacity(256),
                waker_buf: Box::new([0u8; 8]),
                rx_in_flight: 0,
                tx_in_flight: 0,
            };

            // Submit initial recvmsg SQEs for all RX slots
            driver.replenish_rx()?;
            // Submit eventfd read for waker
            driver.submit_waker_read()?;
            driver.ring.submit()?;

            // SAFETY: dup the eventfd for the waker (the driver keeps the original)
            let waker_fd = unsafe { libc::dup(driver.eventfd.as_raw_fd()) };
            if waker_fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let waker_eventfd = unsafe { OwnedFd::from_raw_fd(waker_fd) };

            let waker = IoUringWaker {
                eventfd: Arc::new(waker_eventfd),
            };
            Ok((driver, waker))
        }

        fn poll(&mut self, deadline: Option<Instant>) -> io::Result<PollOutcome> {
            // Submit pending and wait for at least 1 CQE with timeout
            let wait_dur = deadline.map_or(Duration::from_millis(100), |d| {
                d.saturating_duration_since(Instant::now())
            });

            // Use submit_and_wait with a manual timeout check
            let _ = self.ring.submit();

            // Wait with timeout using the submitter
            let ts = io_uring::types::Timespec::new()
                .sec(wait_dur.as_secs())
                .nsec(wait_dur.subsec_nanos());
            // submit_and_wait_with_timeout is not always available; use a simpler approach:
            // submit, then wait with a timeout via the completion queue
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

            // Process all available CQEs
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
                        if self.tx_slots.contains(idx) {
                            self.tx_slots.remove(idx);
                        }
                    }
                    OP_WAKER => {
                        outcome.woken = true;
                        // Drain eventfd counter
                        // SAFETY: reading 8 bytes from a valid eventfd.
                        unsafe {
                            libc::read(
                                self.eventfd.as_raw_fd(),
                                self.waker_buf.as_mut_ptr().cast(),
                                8,
                            );
                        }
                        // Resubmit waker read
                        let _ = self.submit_waker_read();
                    }
                    _ => {}
                }
            }

            // Replenish RX depth
            self.replenish_rx()?;
            self.ring.submit()?;

            Ok(outcome)
        }

        fn submit_sends(&mut self, packets: Vec<TxDatagram>) -> io::Result<()> {
            for pkt in packets {
                let (addr, addr_len) = socketaddr_to_sockaddr_storage(&pkt.to);

                let mut slot = TxSlot {
                    data: pkt.data,
                    addr: Box::new(addr),
                    addr_len,
                    iov: Box::new(unsafe { std::mem::zeroed() }),
                    msg: Box::new(unsafe { std::mem::zeroed() }),
                };

                // Fix up pointers
                slot.iov.iov_base = slot.data.as_ptr() as *mut _;
                slot.iov.iov_len = slot.data.len();
                slot.msg.msg_name = (slot.addr.as_ref() as *const _ as *mut _);
                slot.msg.msg_namelen = slot.addr_len;
                slot.msg.msg_iov = slot.iov.as_mut() as *mut _;
                slot.msg.msg_iovlen = 1;

                let slab_key = self.tx_slots.insert(slot);
                let slot_ref = &self.tx_slots[slab_key];

                let entry = io_uring::opcode::SendMsg::new(
                    io_uring::types::Fd(self.socket_fd),
                    slot_ref.msg.as_ref() as *const libc::msghdr,
                )
                .build()
                .user_data(OP_SEND | slab_key as u64);

                // SAFETY: TxSlot fields are heap-allocated. Pointers are stable
                // until the CQE arrives and the slot is removed from the slab.
                unsafe {
                    self.ring.submission().push(&entry).map_err(|_| {
                        io::Error::new(io::ErrorKind::Other, "SQ full")
                    })?;
                }
                self.tx_in_flight += 1;
            }
            self.ring.submit()?;
            Ok(())
        }

        fn pending_tx_count(&self) -> usize {
            self.tx_in_flight
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

    fn socketaddr_to_sockaddr_storage(addr: &SocketAddr) -> (libc::sockaddr_storage, libc::socklen_t) {
        // SAFETY: zeroed sockaddr_storage is valid for all address families.
        let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
        let len = match addr {
            SocketAddr::V4(v4) => {
                // SAFETY: storage is large enough for sockaddr_in.
                let sin: &mut libc::sockaddr_in = unsafe { &mut *(&mut storage as *mut _ as *mut _) };
                sin.sin_family = libc::AF_INET as libc::sa_family_t;
                sin.sin_port = v4.port().to_be();
                sin.sin_addr.s_addr = u32::from(*v4.ip()).to_be();
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t
            }
            SocketAddr::V6(v6) => {
                // SAFETY: storage is large enough for sockaddr_in6.
                let sin6: &mut libc::sockaddr_in6 = unsafe { &mut *(&mut storage as *mut _ as *mut _) };
                sin6.sin6_family = libc::AF_INET6 as libc::sa_family_t;
                sin6.sin6_port = v6.port().to_be();
                sin6.sin6_addr.s6_addr = v6.ip().octets();
                std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t
            }
        };
        (storage, len)
    }
}

#[cfg(target_os = "linux")]
pub(crate) use inner::{IoUringDriver, IoUringWaker};
