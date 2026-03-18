//! PollDriver: portable Linux readiness-based UDP driver.
//! Uses `poll(2)` on the UDP socket plus an `eventfd` waker.

#[cfg(target_os = "linux")]
mod inner {
    use std::collections::VecDeque;
    use std::io;
    use std::net::SocketAddr;
    use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::sync::Arc;
    use std::time::Instant;

    use crate::transport::{
        Driver, DriverWaker, PollOutcome, RuntimeDriverKind, RxDatagram, TxDatagram,
    };

    const MAX_RX_PER_POLL: usize = 256;

    pub struct PollDriver {
        socket: std::net::UdpSocket,
        socket_fd: RawFd,
        eventfd: OwnedFd,
        unsent: VecDeque<TxDatagram>,
        recv_buf: Vec<u8>,
        recycled_tx: Vec<Vec<u8>>,
        waker_buf: [u8; 8],
    }

    #[derive(Clone)]
    pub struct PollWaker {
        eventfd: Arc<OwnedFd>,
    }

    impl Driver for PollDriver {
        type Waker = PollWaker;

        fn new(socket: std::net::UdpSocket) -> io::Result<(Self, Self::Waker)> {
            let socket_fd = socket.as_raw_fd();
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
                eventfd,
                unsent: VecDeque::new(),
                recv_buf: vec![0u8; 65535],
                recycled_tx: Vec::new(),
                waker_buf: [0u8; 8],
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
                for _ in 0..MAX_RX_PER_POLL {
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
            }

            Ok(outcome)
        }

        fn submit_sends(&mut self, packets: Vec<TxDatagram>) -> io::Result<()> {
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

        fn driver_kind(&self) -> RuntimeDriverKind {
            RuntimeDriverKind::Poll
        }
    }

    impl PollDriver {
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
}

#[cfg(target_os = "linux")]
pub(crate) use inner::{PollDriver, PollWaker};
