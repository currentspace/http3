//! KqueueDriver: uses `nix::sys::event` for kqueue/kevent on macOS.
//! Readiness-based: `poll()` does `kevent()` then `recv_from` loop.
//! `submit_sends()` does `send_to` immediately, queueing `WouldBlock` packets.
//! Wakeup via `EVFILT_USER` + `NOTE_TRIGGER` — zero-copy and atomic.

#[cfg(target_os = "macos")]
mod inner {
    use std::collections::VecDeque;
    use std::io;
    use std::net::SocketAddr;
    use std::os::unix::io::{AsFd, AsRawFd, RawFd};
    use std::time::Instant;

    use nix::sys::event::{EventFilter, EventFlag, FilterFlag, KEvent, Kqueue};

    use crate::transport::{Driver, DriverWaker, PollOutcome, RxDatagram, TxDatagram};

    const WAKER_IDENT: usize = 0xCAFE;

    /// Max datagrams to recv per poll iteration.  Prevents the recv loop from
    /// starving the send path under fan-out: after this many packets the loop
    /// yields so flush_sends() can push ACKs out, then the next poll() returns
    /// immediately (EV_CLEAR edge-triggered re-arms after read).
    const MAX_RX_PER_POLL: usize = 256;

    pub struct KqueueDriver {
        kq: Kqueue,
        socket: std::net::UdpSocket,
        socket_fd: RawFd,
        unsent: VecDeque<TxDatagram>,
        write_interest_registered: bool,
        event_buf: Vec<KEvent>,
        recv_buf: Vec<u8>,
        /// Buffers from successfully sent packets, ready for pool recycling.
        recycled_tx: Vec<Vec<u8>>,
    }

    #[derive(Clone)]
    pub struct KqueueWaker {
        /// Raw fd of the kqueue for cross-thread kevent() calls.
        kq_fd: RawFd,
    }

    // SAFETY: kqueue fds are safe to use from any thread via kevent(). The kernel
    // serializes concurrent kevent() calls. KqueueWaker only triggers EVFILT_USER,
    // which is an atomic wakeup — no shared mutable state between threads.
    #[allow(unsafe_code)]
    unsafe impl Send for KqueueWaker {}
    #[allow(unsafe_code)]
    unsafe impl Sync for KqueueWaker {}

    impl Driver for KqueueDriver {
        type Waker = KqueueWaker;

        fn new(socket: std::net::UdpSocket) -> io::Result<(Self, Self::Waker)> {
            let kq = Kqueue::new().map_err(nix_to_io)?;
            let socket_fd = socket.as_raw_fd();

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

            let kq_fd = kq.as_fd().as_raw_fd();
            let waker = KqueueWaker { kq_fd };
            Ok((
                Self {
                    kq,
                    socket,
                    socket_fd,
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
                    recv_buf: vec![0u8; 65535],
                },
                waker,
            ))
        }

        fn poll(&mut self, deadline: Option<Instant>) -> io::Result<PollOutcome> {
            let timeout = deadline.map(|d| {
                let dur = d.saturating_duration_since(Instant::now());
                libc::timespec {
                    tv_sec: dur.as_secs() as libc::time_t,
                    tv_nsec: dur.subsec_nanos() as libc::c_long,
                }
            }).unwrap_or(libc::timespec {
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
                self.drain_unsent();
            }

            // Drain socket: recv_from loop, capped to avoid starving the send path.
            // Under fan-out (many connections), an unbounded loop delays ACKs and
            // causes congestion-window stalls.  The cap lets flush_sends() run
            // between batches; the next poll() returns immediately because
            // EV_CLEAR re-arms on any remaining data.
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
    }

    impl KqueueDriver {
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

    impl DriverWaker for KqueueWaker {
        fn wake(&self) -> io::Result<()> {
            // Trigger EVFILT_USER on the kqueue fd.
            // We must use raw libc kevent() here because Kqueue is !Sync and
            // we only have the raw fd on the waker thread.
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
            // SAFETY: kq_fd is a valid kqueue fd. changelist is stack-allocated and
            // lives for the duration of the kevent() call. We pass 0 for nevents
            // (no output), so the eventlist pointer is irrelevant.
            #[allow(unsafe_code)]
            let rc = unsafe {
                libc::kevent(
                    self.kq_fd,
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
pub(crate) use inner::KqueueDriver;
