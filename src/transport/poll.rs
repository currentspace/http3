//! PollDriver: portable Linux readiness-based UDP driver.
//! Uses `poll(2)` on the UDP socket plus an `eventfd` waker.
//! RX uses `recvmmsg(2)` and TX uses `sendmmsg(2)` for batched syscalls.

#[cfg(target_os = "linux")]
mod inner {
    use std::collections::VecDeque;
    use std::io;
    use std::net::SocketAddr;
    use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::sync::Arc;
    use std::time::Instant;

    use crate::buffer_pool::AdaptiveBufferPool;
    use crate::reactor_metrics;
    use crate::transport::socket::{
        CMSG_CONTROL_LEN, build_gso_cmsg, enable_gro, parse_gro_cmsg, parse_pktinfo_cmsg,
        probe_gso, set_pktinfo,
    };
    use crate::transport::{
        Driver, DriverWaker, PollOutcome, RuntimeDriverKind, RxDatagram, TxDatagram, group_for_gso,
    };

    const MAX_RX_PER_POLL: usize = 256;
    const RX_BUF_SIZE: usize = 65535;

    /// Pre-allocated state for a single recvmmsg slot.
    struct RxMmsgSlot {
        buf: Vec<u8>,
        addr: libc::sockaddr_storage,
        iov: libc::iovec,
        cmsg_buf: [u8; CMSG_CONTROL_LEN],
    }

    /// Pre-allocated recvmmsg batch state.
    struct RxBatch {
        slots: Vec<RxMmsgSlot>,
        hdrs: Vec<libc::mmsghdr>,
    }

    impl RxBatch {
        fn new() -> Self {
            let mut slots: Vec<RxMmsgSlot> = (0..MAX_RX_PER_POLL)
                .map(|_| RxMmsgSlot {
                    buf: vec![0u8; RX_BUF_SIZE],
                    // SAFETY: zeroed sockaddr_storage is valid.
                    addr: unsafe { std::mem::zeroed() },
                    iov: libc::iovec {
                        iov_base: std::ptr::null_mut(),
                        iov_len: 0,
                    },
                    cmsg_buf: [0u8; CMSG_CONTROL_LEN],
                })
                .collect();

            // SAFETY: zeroed mmsghdr is valid (null pointers, zero lengths).
            let mut hdrs: Vec<libc::mmsghdr> = (0..MAX_RX_PER_POLL)
                .map(|_| unsafe { std::mem::zeroed() })
                .collect();

            // Wire up pointers. Each mmsghdr.msg_hdr points to the slot's buf/addr.
            for i in 0..MAX_RX_PER_POLL {
                slots[i].iov.iov_base = slots[i].buf.as_mut_ptr().cast();
                slots[i].iov.iov_len = RX_BUF_SIZE;
                hdrs[i].msg_hdr.msg_iov = &mut slots[i].iov as *mut libc::iovec;
                hdrs[i].msg_hdr.msg_iovlen = 1;
                hdrs[i].msg_hdr.msg_name =
                    (&mut slots[i].addr as *mut libc::sockaddr_storage).cast();
                hdrs[i].msg_hdr.msg_namelen =
                    std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
                hdrs[i].msg_hdr.msg_control = slots[i].cmsg_buf.as_mut_ptr().cast();
                hdrs[i].msg_hdr.msg_controllen = CMSG_CONTROL_LEN;
            }

            Self { slots, hdrs }
        }

        /// Reset all headers for the next recvmmsg call.
        fn reset(&mut self) {
            for i in 0..MAX_RX_PER_POLL {
                self.slots[i].iov.iov_len = RX_BUF_SIZE;
                self.hdrs[i].msg_hdr.msg_namelen =
                    std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
                self.hdrs[i].msg_hdr.msg_controllen = CMSG_CONTROL_LEN;
                self.hdrs[i].msg_hdr.msg_flags = 0;
                self.hdrs[i].msg_len = 0;
            }
        }
    }

    pub struct PollDriver {
        socket: std::net::UdpSocket,
        socket_fd: RawFd,
        local_addr: SocketAddr,
        gso_supported: bool,
        eventfd: OwnedFd,
        unsent: VecDeque<TxDatagram>,
        recycled_tx: Vec<Vec<u8>>,
        rx_pool: AdaptiveBufferPool,
        waker_buf: [u8; 8],
        rx_batch: RxBatch,
        /// Pre-allocated mmsghdr + iovec + sockaddr arrays for sendmmsg.
        tx_hdrs: Vec<libc::mmsghdr>,
        tx_iovs: Vec<libc::iovec>,
        tx_addrs: Vec<libc::sockaddr_storage>,
        tx_cmsg_bufs: Vec<[u8; 32]>,
    }

    // SAFETY: PollDriver is created on the main thread and moved to the worker
    // thread before any I/O occurs. The raw pointers in RxBatch (mmsghdr, iovec)
    // point to co-located allocations that move with the driver. The driver is
    // single-threaded after the move — no concurrent access.
    unsafe impl Send for PollDriver {}

    #[derive(Clone)]
    pub struct PollWaker {
        eventfd: Arc<OwnedFd>,
    }

    impl Driver for PollDriver {
        type Waker = PollWaker;

        fn new(socket: std::net::UdpSocket) -> io::Result<(Self, Self::Waker)> {
            let socket_fd = socket.as_raw_fd();
            let local_addr = socket.local_addr()?;
            let gso_supported = probe_gso(&socket);
            set_pktinfo(&socket);
            enable_gro(&socket);
            log::info!(
                "PollDriver::new fd={socket_fd} local={local_addr} gso={gso_supported} tid={:?}",
                std::thread::current().id(),
            );
            // SAFETY: eventfd with EFD_NONBLOCK returns a valid fd or -1.
            let efd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK) };
            if efd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: efd is a valid fd from successful eventfd().
            let eventfd = unsafe { OwnedFd::from_raw_fd(efd) };

            let driver = Self {
                socket,
                socket_fd,
                local_addr,
                gso_supported,
                eventfd,
                unsent: VecDeque::new(),
                recycled_tx: Vec::new(),
                rx_pool: AdaptiveBufferPool::new(MAX_RX_PER_POLL, RX_BUF_SIZE),
                waker_buf: [0u8; 8],
                rx_batch: RxBatch::new(),
                tx_hdrs: Vec::with_capacity(256),
                tx_iovs: Vec::with_capacity(256),
                tx_addrs: Vec::with_capacity(256),
                tx_cmsg_bufs: Vec::with_capacity(256),
            };

            // SAFETY: dup returns a valid fd or -1.
            let waker_fd = unsafe { libc::dup(driver.eventfd.as_raw_fd()) };
            if waker_fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: waker_fd is a valid fd from successful dup().
            let waker_eventfd = unsafe { OwnedFd::from_raw_fd(waker_fd) };
            let waker = PollWaker {
                eventfd: Arc::new(waker_eventfd),
            };

            Ok((driver, waker))
        }

        fn poll(&mut self, deadline: Option<Instant>) -> io::Result<PollOutcome> {
            let timeout_ms = deadline.map_or(100i32, |d| {
                let dur = d.saturating_duration_since(Instant::now());
                let millis = dur.as_millis().min(i32::MAX as u128);
                millis as i32
            });

            let mut fds = [
                libc::pollfd {
                    fd: self.socket_fd,
                    events: libc::POLLIN
                        | if self.unsent.is_empty() {
                            0
                        } else {
                            libc::POLLOUT
                        },
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.eventfd.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];

            // SAFETY: fds points to valid pollfd entries for the duration of the call.
            let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout_ms) };
            if rc < 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EINTR) {
                    return Ok(PollOutcome {
                        rx: Vec::new(),
                        woken: false,
                        timer_expired: deadline.is_some_and(|d| Instant::now() >= d),
                    });
                }
                return Err(error);
            }

            let mut outcome = PollOutcome {
                rx: Vec::new(),
                woken: false,
                timer_expired: rc == 0 || deadline.is_some_and(|d| Instant::now() >= d),
            };

            if (fds[1].revents & libc::POLLIN) != 0 {
                outcome.woken = true;
                self.drain_waker();
            }

            if (fds[0].revents & libc::POLLOUT) != 0 {
                self.drain_unsent();
            }

            if (fds[0].revents & libc::POLLIN) != 0 {
                self.recv_batch(&mut outcome);
            }

            Ok(outcome)
        }

        fn submit_sends(&mut self, packets: Vec<TxDatagram>) -> io::Result<()> {
            if packets.is_empty() {
                return Ok(());
            }
            self.send_batch(packets);
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

        fn driver_kind(&self) -> RuntimeDriverKind {
            RuntimeDriverKind::Poll
        }

        fn recycle_rx_buffers(&mut self, buffers: Vec<Vec<u8>>) {
            for buf in buffers {
                let retained = self.rx_pool.checkin(buf);
                reactor_metrics::record_rx_buffer_checkin(retained);
            }
        }
    }

    impl PollDriver {
        /// Receive datagrams using recvmmsg(2).
        fn recv_batch(&mut self, outcome: &mut PollOutcome) {
            self.rx_batch.reset();

            // SAFETY: rx_batch pointers are valid for MAX_RX_PER_POLL entries.
            let count = unsafe {
                libc::recvmmsg(
                    self.socket_fd,
                    self.rx_batch.hdrs.as_mut_ptr(),
                    MAX_RX_PER_POLL as libc::c_uint,
                    libc::MSG_DONTWAIT,
                    std::ptr::null_mut(), // no timeout
                )
            };

            if count > 0 {
                log::trace!(
                    "poll::recv_batch fd={} count={count} tid={:?}",
                    self.socket_fd,
                    std::thread::current().id(),
                );
            }
            if count <= 0 {
                return;
            }

            for i in 0..(count as usize) {
                let len = self.rx_batch.hdrs[i].msg_len as usize;
                let namelen = self.rx_batch.hdrs[i].msg_hdr.msg_namelen;
                let peer = sockaddr_to_socketaddr(&self.rx_batch.slots[i].addr, namelen);
                if let Some(peer) = peer {
                    let cmsg_data = &self.rx_batch.slots[i].cmsg_buf
                        [..self.rx_batch.hdrs[i].msg_hdr.msg_controllen];
                    let local_ip = parse_pktinfo_cmsg(cmsg_data);
                    let local = local_ip
                        .map(|ip| SocketAddr::new(ip, self.local_addr.port()))
                        .unwrap_or(self.local_addr);
                    let segment_size = parse_gro_cmsg(cmsg_data);
                    let (data, reused) = self
                        .rx_pool
                        .copy_from_slice(&self.rx_batch.slots[i].buf[..len]);
                    reactor_metrics::record_rx_buffer_checkout(reused, len);
                    outcome.rx.push(RxDatagram {
                        data,
                        peer,
                        local,
                        segment_size,
                    });
                }
            }
        }

        /// Send datagrams using sendmmsg(2), with optional UDP GSO coalescing.
        fn send_batch(&mut self, packets: Vec<TxDatagram>) {
            log::trace!(
                "poll::send_batch pkts={} gso={} tid={:?}",
                packets.len(),
                self.gso_supported,
                std::thread::current().id(),
            );
            if self.gso_supported && packets.len() > 1 {
                self.send_batch_gso(packets);
                return;
            }
            let count = packets.len();

            // Prepare the mmsghdr array.
            self.tx_hdrs.clear();
            self.tx_iovs.clear();
            self.tx_addrs.clear();
            self.tx_hdrs.resize(count, unsafe { std::mem::zeroed() });
            self.tx_iovs.resize(
                count,
                libc::iovec {
                    iov_base: std::ptr::null_mut(),
                    iov_len: 0,
                },
            );
            // SAFETY: zeroed sockaddr_storage is valid.
            self.tx_addrs.resize(count, unsafe { std::mem::zeroed() });

            // Stash packets so data pointers remain valid during sendmmsg.
            let mut pkt_data: Vec<Vec<u8>> = Vec::with_capacity(count);
            let mut pkt_addrs: Vec<SocketAddr> = Vec::with_capacity(count);
            for pkt in packets {
                pkt_addrs.push(pkt.to);
                pkt_data.push(pkt.data);
            }

            for i in 0..count {
                let addrlen = socketaddr_to_sockaddr(pkt_addrs[i], &mut self.tx_addrs[i]);
                self.tx_iovs[i].iov_base = pkt_data[i].as_ptr() as *mut libc::c_void;
                self.tx_iovs[i].iov_len = pkt_data[i].len();
                self.tx_hdrs[i].msg_hdr.msg_name =
                    (&mut self.tx_addrs[i] as *mut libc::sockaddr_storage).cast();
                self.tx_hdrs[i].msg_hdr.msg_namelen = addrlen;
                self.tx_hdrs[i].msg_hdr.msg_iov = &mut self.tx_iovs[i] as *mut libc::iovec;
                self.tx_hdrs[i].msg_hdr.msg_iovlen = 1;
            }

            // SAFETY: tx_hdrs, tx_iovs, tx_addrs, and pkt_data are all valid
            // for the duration of the sendmmsg call.
            let sent = unsafe {
                libc::sendmmsg(
                    self.socket_fd,
                    self.tx_hdrs.as_mut_ptr(),
                    count as libc::c_uint,
                    libc::MSG_DONTWAIT,
                )
            };

            let sent_count = if sent < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::WouldBlock {
                    0usize
                } else {
                    // On other errors, treat as 0 sent and queue all.
                    0
                }
            } else {
                sent as usize
            };

            // Recycle sent buffers, queue unsent as pending.
            for (i, data) in pkt_data.into_iter().enumerate() {
                if i < sent_count {
                    self.recycled_tx.push(data);
                } else {
                    self.unsent.push_back(TxDatagram {
                        data,
                        to: pkt_addrs[i],
                    });
                }
            }
        }

        /// GSO-accelerated send: group packets by (dest, size) and use UDP_SEGMENT cmsg.
        fn send_batch_gso(&mut self, packets: Vec<TxDatagram>) {
            let batches = group_for_gso(packets);
            let count = batches.len();

            // Allocate fresh each time to avoid stale pointer issues from
            // Vec reuse (clear+resize can leave prior-cycle pointers in place
            // if the kernel reads beyond msg_controllen).
            let mut tx_hdrs: Vec<libc::mmsghdr> =
                (0..count).map(|_| unsafe { std::mem::zeroed() }).collect();
            let mut tx_iovs: Vec<libc::iovec> = vec![
                libc::iovec {
                    iov_base: std::ptr::null_mut(),
                    iov_len: 0
                };
                count
            ];
            let mut tx_addrs: Vec<libc::sockaddr_storage> =
                (0..count).map(|_| unsafe { std::mem::zeroed() }).collect();
            let mut tx_cmsg_bufs: Vec<[u8; 32]> = vec![[0u8; 32]; count];

            // Stash batch data so pointers remain valid during sendmmsg.
            let mut batch_data: Vec<Vec<u8>> = Vec::with_capacity(count);
            let mut batch_addrs: Vec<SocketAddr> = Vec::with_capacity(count);
            let mut batch_seg_sizes: Vec<u16> = Vec::with_capacity(count);
            for batch in batches {
                batch_addrs.push(batch.to);
                batch_seg_sizes.push(batch.segment_size);
                batch_data.push(batch.data);
            }

            for i in 0..count {
                let addrlen = socketaddr_to_sockaddr(batch_addrs[i], &mut tx_addrs[i]);
                tx_iovs[i].iov_base = batch_data[i].as_ptr() as *mut libc::c_void;
                tx_iovs[i].iov_len = batch_data[i].len();
                tx_hdrs[i].msg_hdr.msg_name =
                    (&mut tx_addrs[i] as *mut libc::sockaddr_storage).cast();
                tx_hdrs[i].msg_hdr.msg_namelen = addrlen;
                tx_hdrs[i].msg_hdr.msg_iov = &mut tx_iovs[i] as *mut libc::iovec;
                tx_hdrs[i].msg_hdr.msg_iovlen = 1;

                // Attach UDP_SEGMENT cmsg when the batch holds >1 segment.
                if batch_data[i].len() > batch_seg_sizes[i] as usize {
                    let cmsg_len = build_gso_cmsg(&mut tx_cmsg_bufs[i], batch_seg_sizes[i]);
                    tx_hdrs[i].msg_hdr.msg_control = tx_cmsg_bufs[i].as_mut_ptr().cast();
                    tx_hdrs[i].msg_hdr.msg_controllen = cmsg_len;
                }
            }

            // SAFETY: all hdrs/iovs/addrs/cmsg_bufs/batch_data are valid
            // for the duration of the sendmmsg call.
            let sent = unsafe {
                libc::sendmmsg(
                    self.socket_fd,
                    tx_hdrs.as_mut_ptr(),
                    count as libc::c_uint,
                    libc::MSG_DONTWAIT,
                )
            };

            let sent_count = if sent < 0 {
                if io::Error::last_os_error().kind() == io::ErrorKind::WouldBlock {
                    0usize
                } else {
                    0
                }
            } else {
                sent as usize
            };

            // Recycle sent batch buffers, split unsent batches back to individual packets.
            for (i, data) in batch_data.into_iter().enumerate() {
                if i < sent_count {
                    self.recycled_tx.push(data);
                } else {
                    let seg = batch_seg_sizes[i] as usize;
                    if data.len() > seg {
                        for chunk in data.chunks(seg) {
                            self.unsent.push_back(TxDatagram {
                                data: chunk.to_vec(),
                                to: batch_addrs[i],
                            });
                        }
                    } else {
                        self.unsent.push_back(TxDatagram {
                            data,
                            to: batch_addrs[i],
                        });
                    }
                }
            }
        }

        fn drain_unsent(&mut self) {
            // Drain unsent one at a time (backpressure recovery).
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

        fn drain_waker(&mut self) {
            loop {
                // SAFETY: reading 8 bytes from a valid eventfd into a stack buffer.
                let rc = unsafe {
                    libc::read(
                        self.eventfd.as_raw_fd(),
                        self.waker_buf.as_mut_ptr().cast(),
                        self.waker_buf.len(),
                    )
                };
                if rc >= 0 {
                    break;
                }
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::WouldBlock {
                    break;
                }
                if error.raw_os_error() != Some(libc::EINTR) {
                    break;
                }
            }
        }
    }

    impl DriverWaker for PollWaker {
        fn wake(&self) -> io::Result<()> {
            let value: u64 = 1;
            // SAFETY: eventfd is valid; value points to an initialized u64.
            let rc = unsafe {
                libc::write(
                    self.eventfd.as_raw_fd(),
                    &value as *const u64 as *const libc::c_void,
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
                // SAFETY: storage has enough space for sockaddr_in.
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
                // SAFETY: storage has enough space for sockaddr_in6.
                unsafe {
                    std::ptr::write(storage as *mut _ as *mut libc::sockaddr_in6, sockaddr);
                }
                std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t
            }
        }
    }
}

#[cfg(all(target_os = "linux", feature = "bench-internals"))]
pub use inner::{PollDriver, PollWaker};
#[cfg(all(target_os = "linux", not(feature = "bench-internals")))]
pub(crate) use inner::{PollDriver, PollWaker};
