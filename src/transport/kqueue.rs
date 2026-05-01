//! KqueueDriver: uses `nix::sys::event` for kqueue/kevent on macOS.
//! Readiness-based: `poll()` does `kevent()` then `recv_from` loop.
//! `submit_sends()` does `send_to` immediately, queueing `WouldBlock` packets.
//! Wakeup via `EVFILT_USER` + `NOTE_TRIGGER` — zero-copy and atomic.

#[cfg(target_os = "macos")]
mod inner {
    use std::collections::VecDeque;
    use std::io;
    use std::net::SocketAddr;
    use std::os::unix::io::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::sync::Arc;
    use std::time::Instant;

    use nix::sys::event::{EventFilter, EventFlag, FilterFlag, KEvent, Kqueue};

    use crate::buffer_pool::AdaptiveBufferPool;
    use crate::reactor_metrics;
    use crate::transport::{
        Driver, DriverWaker, PollOutcome, RuntimeDriverKind, RxDatagram, TxDatagram,
    };

    const WAKER_IDENT: usize = 0xCAFE;
    const RX_BUF_SIZE: usize = 65535;

    /// Max datagrams to recv per poll iteration.  Prevents the recv loop from
    /// starving the send path under fan-out: after this many packets the loop
    /// yields so flush_sends() can push ACKs out, then the next poll() returns
    /// immediately (EV_CLEAR edge-triggered re-arms after read).
    const MAX_RX_PER_POLL: usize = 256;

    pub struct KqueueDriver {
        kq: Kqueue,
        socket: std::net::UdpSocket,
        socket_fd: RawFd,
        local_addr: SocketAddr,
        unsent: VecDeque<TxDatagram>,
        write_interest_registered: bool,
        event_buf: Vec<KEvent>,
        recv_buf: Vec<u8>,
        rx_pool: AdaptiveBufferPool,
        /// Buffers from successfully sent packets, ready for pool recycling.
        recycled_tx: Vec<Vec<u8>>,
    }

    /// Waker for the kqueue driver. Holds an `Arc<OwnedFd>` (a `dup`'d copy
    /// of the kqueue fd) so the fd outlives the driver if a Waker clone is
    /// still held — fixes audit finding #5 (UAF on driver drop). The kernel
    /// serializes concurrent `kevent()` calls; we only trigger EVFILT_USER,
    /// which is an atomic wakeup with no shared mutable state.
    #[derive(Clone)]
    pub struct KqueueWaker {
        kq_fd: Arc<OwnedFd>,
    }

    impl Driver for KqueueDriver {
        type Waker = KqueueWaker;

        fn new(socket: std::net::UdpSocket) -> io::Result<(Self, Self::Waker)> {
            let kq = Kqueue::new().map_err(nix_to_io)?;
            let socket_fd = socket.as_raw_fd();
            let local_addr = socket.local_addr()?;

            // Audit finding #20: enable IP_RECVDSTADDR / IPV6_RECVPKTINFO so
            // recvmsg's cmsg carries the per-datagram destination IP. Servers
            // bound to 0.0.0.0 need this to route by the actual local IP each
            // datagram arrived on (matters for connection-ID DCID multiplexing
            // on multi-homed hosts).
            crate::transport::socket::set_pktinfo(&socket);
            // Audit finding #18: enable IP_RECVTOS / IPV6_RECVTCLASS so
            // recvmsg's cmsg carries the per-datagram ECN code point.
            // quiche 0.28 doesn't expose ECN, so this is observability only;
            // see reactor_metrics::record_ecn_recv.
            crate::transport::socket::set_recv_ecn(&socket);

            // Register EVFILT_READ permanently (EV_ADD | EV_CLEAR = edge-triggered, auto-rearm)
            let read_ev = KEvent::new(
                socket_fd as usize,
                EventFilter::EVFILT_READ,
                EventFlag::EV_ADD | EventFlag::EV_CLEAR,
                FilterFlag::empty(),
                0,
                0,
            );
            // Register EVFILT_USER for waker (initially unarmed, fires on NOTE_TRIGGER)
            let waker_ev = KEvent::new(
                WAKER_IDENT,
                EventFilter::EVFILT_USER,
                EventFlag::EV_ADD | EventFlag::EV_CLEAR,
                FilterFlag::empty(),
                0,
                0,
            );
            let empty: &mut [KEvent] = &mut [];
            kq.kevent(&[read_ev, waker_ev], empty, None)
                .map_err(nix_to_io)?;

            // dup the kqueue fd into an OwnedFd shared with the waker, so a
            // Waker clone outliving the driver doesn't dangle.
            #[allow(unsafe_code)]
            let waker_fd = unsafe { libc::dup(kq.as_fd().as_raw_fd()) };
            if waker_fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: waker_fd is a valid fd from a successful dup().
            #[allow(unsafe_code)]
            let waker_owned = unsafe { OwnedFd::from_raw_fd(waker_fd) };
            let waker = KqueueWaker { kq_fd: Arc::new(waker_owned) };
            Ok((
                Self {
                    kq,
                    socket,
                    socket_fd,
                    local_addr,
                    unsent: VecDeque::new(),
                    write_interest_registered: false,
                    recycled_tx: Vec::new(),
                    event_buf: vec![
                        KEvent::new(
                            0,
                            EventFilter::EVFILT_READ,
                            EventFlag::empty(),
                            FilterFlag::empty(),
                            0,
                            0,
                        );
                        32
                    ],
                    recv_buf: vec![0u8; RX_BUF_SIZE],
                    rx_pool: AdaptiveBufferPool::new(MAX_RX_PER_POLL, RX_BUF_SIZE),
                },
                waker,
            ))
        }

        fn poll(&mut self, deadline: Option<Instant>) -> io::Result<PollOutcome> {
            let timeout = deadline
                .map(|d| {
                    let dur = d.saturating_duration_since(Instant::now());
                    libc::timespec {
                        tv_sec: dur.as_secs() as libc::time_t,
                        tv_nsec: dur.subsec_nanos() as libc::c_long,
                    }
                })
                .unwrap_or(libc::timespec {
                    tv_sec: 0,
                    tv_nsec: 100_000_000, // 100ms default
                });

            // Manage EVFILT_WRITE: register only when unsent queue is non-empty
            let mut changes: Vec<KEvent> = Vec::new();
            if !self.unsent.is_empty() && !self.write_interest_registered {
                changes.push(KEvent::new(
                    self.socket_fd as usize,
                    EventFilter::EVFILT_WRITE,
                    EventFlag::EV_ADD | EventFlag::EV_ONESHOT,
                    FilterFlag::empty(),
                    0,
                    0,
                ));
                self.write_interest_registered = true;
            } else if self.unsent.is_empty() && self.write_interest_registered {
                changes.push(KEvent::new(
                    self.socket_fd as usize,
                    EventFilter::EVFILT_WRITE,
                    EventFlag::EV_DELETE,
                    FilterFlag::empty(),
                    0,
                    0,
                ));
                self.write_interest_registered = false;
            }

            let n = self
                .kq
                .kevent(&changes, &mut self.event_buf, Some(timeout))
                .map_err(nix_to_io)?;

            let mut outcome = PollOutcome {
                rx: Vec::new(),
                woken: false,
                timer_expired: false,
            };

            // Check deadline
            if deadline.is_some_and(|d| Instant::now() >= d) {
                outcome.timer_expired = true;
            }
            if n == 0 {
                outcome.timer_expired = true; // kevent returned 0 = timeout
            }

            let mut writable = false;

            for ev in &self.event_buf[..n] {
                match ev.filter().unwrap_or(EventFilter::EVFILT_READ) {
                    EventFilter::EVFILT_READ => {} // handled by recv loop below
                    EventFilter::EVFILT_WRITE => {
                        writable = true;
                        self.write_interest_registered = false; // ONESHOT auto-disarms
                    }
                    EventFilter::EVFILT_USER => outcome.woken = true,
                    _ => {}
                }
            }

            // Drain unsent queue if writable
            if writable {
                reactor_metrics::record_kqueue_write_wakeup();
                self.drain_unsent();
            }

            // Drain socket via recvmsg (audit finding #20) so the cmsg
            // delivers per-datagram destination IP. Cap iterations to
            // avoid starving the send path. Under fan-out an unbounded
            // loop delays ACKs and causes congestion-window stalls; the
            // cap lets flush_sends() run between batches and the next
            // poll() returns immediately because EV_CLEAR re-arms on any
            // remaining data.
            let bound_to_specific = !self.local_addr.ip().is_unspecified();
            for _ in 0..MAX_RX_PER_POLL {
                match self.recvmsg_once() {
                    Ok((len, peer, parsed_local)) => {
                        let (data, reused) =
                            self.rx_pool.copy_from_slice(&self.recv_buf[..len]);
                        reactor_metrics::record_rx_buffer_checkout(reused, len);
                        // When bound to a concrete IP, use it directly — the
                        // bind addr is authoritative and avoids picking up an
                        // alternate-family pktinfo on dual-stack sockets.
                        // When bound to 0.0.0.0 or [::], use the parsed cmsg
                        // local so multi-homed servers can route by the actual
                        // local IP each datagram arrived on.
                        let local = if bound_to_specific {
                            self.local_addr
                        } else {
                            parsed_local.map_or(self.local_addr, |ip| {
                                SocketAddr::new(ip, self.local_addr.port())
                            })
                        };
                        outcome.rx.push(RxDatagram {
                            data,
                            peer,
                            local,
                            segment_size: None,
                        });
                    }
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }

            Ok(outcome)
        }

        fn submit_sends(&mut self, packets: Vec<TxDatagram>) -> io::Result<()> {
            for pkt in packets {
                match self.socket.send_to(&pkt.data, pkt.to) {
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                        reactor_metrics::record_kqueue_would_block_send();
                        self.unsent.push_back(pkt);
                        reactor_metrics::record_kqueue_unsent_depth(self.unsent.len());
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
            Ok(self.local_addr)
        }

        fn driver_kind(&self) -> RuntimeDriverKind {
            RuntimeDriverKind::Kqueue
        }

        fn recycle_rx_buffers(&mut self, buffers: Vec<Vec<u8>>) {
            for buf in buffers {
                let retained = self.rx_pool.checkin(buf);
                reactor_metrics::record_rx_buffer_checkin(retained);
            }
        }
    }

    impl KqueueDriver {
        /// Receive a single datagram via `recvmsg`, returning the payload
        /// length, peer address, and any per-packet local IP parsed from
        /// the control message (`IP_RECVDSTADDR` / `IPV6_PKTINFO`).
        ///
        /// Used in place of `recv_from` so that servers bound to `0.0.0.0`
        /// can recover the actual destination IP each datagram arrived on
        /// (audit finding #20). The recv buffer is `self.recv_buf`; the
        /// caller copies out before the next call.
        fn recvmsg_once(
            &mut self,
        ) -> io::Result<(usize, SocketAddr, Option<std::net::IpAddr>)> {
            // SAFETY: zeroed sockaddr_storage is a valid initial state.
            #[allow(unsafe_code)]
            let mut name: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
            let mut control = [0u8; crate::transport::socket::CMSG_CONTROL_LEN];
            let mut iov = libc::iovec {
                iov_base: self.recv_buf.as_mut_ptr().cast(),
                iov_len: self.recv_buf.len(),
            };
            // SAFETY: zeroed msghdr is valid; we wire up name/iov/control below.
            #[allow(unsafe_code)]
            let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
            msg.msg_name = (&raw mut name).cast();
            msg.msg_namelen =
                std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
            msg.msg_iov = &raw mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = control.as_mut_ptr().cast();
            msg.msg_controllen = control.len() as libc::socklen_t;

            // SAFETY: msg points to a valid msghdr; name, iov.iov_base, and
            // control all live for the duration of the recvmsg call.
            #[allow(unsafe_code)]
            let n = unsafe { libc::recvmsg(self.socket_fd, &raw mut msg, 0) };
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            let len = n as usize;

            let peer = crate::transport::socket::sockaddr_to_socketaddr(&name, msg.msg_namelen)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "unrecognised peer address")
                })?;
            let parsed_local = if msg.msg_controllen > 0 {
                let parsed = crate::transport::socket::parse_recv_cmsgs(
                    &control[..msg.msg_controllen as usize],
                );
                if let Some(tos) = parsed.tos {
                    reactor_metrics::record_ecn_recv(
                        crate::transport::socket::EcnCodePoint::from_tos(tos),
                    );
                }
                parsed.local_ip
            } else {
                None
            };
            Ok((len, peer, parsed_local))
        }

        fn drain_unsent(&mut self) {
            while let Some(front) = self.unsent.front() {
                match self.socket.send_to(&front.data, front.to) {
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                        reactor_metrics::record_kqueue_would_block_send();
                        reactor_metrics::record_kqueue_unsent_depth(self.unsent.len());
                        return;
                    }
                    Ok(_) | Err(_) => {
                        if let Some(pkt) = self.unsent.pop_front() {
                            self.recycled_tx.push(pkt.data);
                        }
                    }
                }
            }
        }
    }

    impl DriverWaker for KqueueWaker {
        fn wake(&self) -> io::Result<()> {
            // Trigger EVFILT_USER on the kqueue fd. The Arc<OwnedFd> keeps
            // the fd valid even if the driver has been dropped concurrently.
            let ev = KEvent::new(
                WAKER_IDENT,
                EventFilter::EVFILT_USER,
                EventFlag::empty(),
                FilterFlag::NOTE_TRIGGER,
                0,
                0,
            );
            let changelist = [ev];
            let timeout = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            // SAFETY: self.kq_fd points to a valid, owned kqueue fd.
            // changelist is stack-allocated and lives for the kevent() call.
            // We pass 0 for nevents, so the eventlist pointer is irrelevant.
            #[allow(unsafe_code)]
            let rc = unsafe {
                libc::kevent(
                    self.kq_fd.as_raw_fd(),
                    changelist.as_ptr().cast(),
                    1,
                    std::ptr::null_mut(),
                    0,
                    &timeout,
                )
            };
            if rc < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        }
    }

    fn nix_to_io(e: nix::errno::Errno) -> io::Error {
        io::Error::from_raw_os_error(e as i32)
    }
}

#[cfg(target_os = "macos")]
pub(crate) use inner::{KqueueDriver, KqueueWaker};

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::inner::KqueueDriver;
    use crate::transport::{Driver, DriverWaker};

    /// Audit finding #5: a Waker clone must remain usable after the
    /// driver has been dropped. Before the fix the waker held a bare
    /// RawFd — calling wake() after driver drop hit kevent() on a closed
    /// (and possibly recycled) fd. With Arc<OwnedFd>, the dup'd fd stays
    /// alive as long as a clone is held.
    #[test]
    fn waker_outlives_driver() {
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind");
        socket.set_nonblocking(true).expect("nonblocking");
        let (driver, waker) = KqueueDriver::new(socket).expect("kqueue new");

        let cloned = waker.clone();
        drop(waker);
        drop(driver);

        // Must not panic, must not return EBADF — the dup'd fd is owned
        // by the Arc inside the cloned waker.
        cloned.wake().expect("wake after driver drop");
    }
}
