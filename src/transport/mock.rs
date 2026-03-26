use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TryRecvError, after, bounded, unbounded};

use crate::transport::{
    Driver, DriverWaker, PollOutcome, RuntimeDriverKind, RxDatagram, TxDatagram,
};

#[allow(dead_code)]
#[derive(Clone, Debug)]
pub(crate) struct MockTraceDatagram {
    pub from: SocketAddr,
    pub to: SocketAddr,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Default)]
pub(crate) struct MockTraceRecorder {
    datagrams: Arc<Mutex<Vec<MockTraceDatagram>>>,
}

impl MockTraceRecorder {
    pub(crate) fn record_datagram(&self, from: SocketAddr, to: SocketAddr, bytes: &[u8]) {
        if let Ok(mut datagrams) = self.datagrams.lock() {
            datagrams.push(MockTraceDatagram {
                from,
                to,
                bytes: bytes.to_vec(),
            });
        }
    }

    pub(crate) fn snapshot(&self) -> Vec<MockTraceDatagram> {
        self.datagrams
            .lock()
            .map_or_else(|_| Vec::new(), |datagrams| datagrams.clone())
    }
}

#[derive(Clone)]
pub struct MockWaker {
    wake_tx: Sender<()>,
}

struct MockInboundDatagram {
    from: SocketAddr,
    bytes: Vec<u8>,
}

/// Simulates network packet loss for testing QUIC retransmission.
/// Drop rate is expressed as a percentage (0–100). Thread-safe so both
/// sides of a mock pair can share the same config.
#[derive(Clone)]
pub struct PacketLossConfig {
    /// Drop rate in percent (0 = no loss, 10 = 10% loss, 100 = drop all).
    drop_pct: Arc<AtomicU32>,
    /// Simple counter-based PRNG seed — not cryptographic, just fast and
    /// deterministic enough for tests.
    counter: Arc<AtomicU64>,
    /// Total packets sent (including dropped).
    pub(crate) total_sent: Arc<AtomicU64>,
    /// Total packets dropped.
    pub(crate) total_dropped: Arc<AtomicU64>,
}

impl PacketLossConfig {
    /// Create a new packet loss config with the given drop percentage.
    pub fn new(drop_pct: u32) -> Self {
        Self {
            drop_pct: Arc::new(AtomicU32::new(drop_pct.min(100))),
            counter: Arc::new(AtomicU64::new(0)),
            total_sent: Arc::new(AtomicU64::new(0)),
            total_dropped: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Returns true if this packet should be dropped.
    fn should_drop(&self) -> bool {
        let pct = self.drop_pct.load(Ordering::Relaxed);
        if pct == 0 {
            return false;
        }
        if pct >= 100 {
            return true;
        }
        // Fast counter-based pseudo-random: hash the counter to get
        // a value in [0, 100). Drop if < drop_pct.
        let c = self.counter.fetch_add(1, Ordering::Relaxed);
        let hash = c.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        (hash % 100) < u64::from(pct)
    }

    /// Record a send attempt.
    fn record_send(&self, dropped: bool) {
        self.total_sent.fetch_add(1, Ordering::Relaxed);
        if dropped {
            self.total_dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Snapshot of loss stats.
    pub fn stats(&self) -> (u64, u64) {
        (
            self.total_sent.load(Ordering::Relaxed),
            self.total_dropped.load(Ordering::Relaxed),
        )
    }
}

pub struct MockDriver {
    local_addr: SocketAddr,
    inbound_rx: Receiver<MockInboundDatagram>,
    outbound_tx: Sender<MockInboundDatagram>,
    wake_rx: Receiver<()>,
    recycled_tx: Vec<Vec<u8>>,
    trace: Option<MockTraceRecorder>,
    packet_loss: Option<PacketLossConfig>,
}

impl MockDriver {
    pub fn pair(
        left_addr: SocketAddr,
        right_addr: SocketAddr,
    ) -> ((Self, MockWaker), (Self, MockWaker)) {
        Self::pair_with_options(left_addr, right_addr, None, None)
    }

    /// Create a pair with optional packet loss simulation.
    pub fn pair_with_loss(
        left_addr: SocketAddr,
        right_addr: SocketAddr,
        loss: PacketLossConfig,
    ) -> ((Self, MockWaker), (Self, MockWaker)) {
        Self::pair_with_options(left_addr, right_addr, None, Some(loss))
    }

    pub(crate) fn pair_with_trace(
        left_addr: SocketAddr,
        right_addr: SocketAddr,
        trace: Option<MockTraceRecorder>,
    ) -> ((Self, MockWaker), (Self, MockWaker)) {
        Self::pair_with_options(left_addr, right_addr, trace, None)
    }

    pub(crate) fn pair_with_options(
        left_addr: SocketAddr,
        right_addr: SocketAddr,
        trace: Option<MockTraceRecorder>,
        packet_loss: Option<PacketLossConfig>,
    ) -> ((Self, MockWaker), (Self, MockWaker)) {
        let (left_tx, left_rx) = unbounded();
        let (right_tx, right_rx) = unbounded();
        let (left_wake_tx, left_wake_rx) = bounded(1024);
        let (right_wake_tx, right_wake_rx) = bounded(1024);

        let left = Self {
            local_addr: left_addr,
            inbound_rx: left_rx,
            outbound_tx: right_tx,
            wake_rx: left_wake_rx,
            recycled_tx: Vec::new(),
            trace: trace.clone(),
            packet_loss: packet_loss.clone(),
        };
        let right = Self {
            local_addr: right_addr,
            inbound_rx: right_rx,
            outbound_tx: left_tx,
            wake_rx: right_wake_rx,
            recycled_tx: Vec::new(),
            trace,
            packet_loss,
        };

        (
            (
                left,
                MockWaker {
                    wake_tx: left_wake_tx,
                },
            ),
            (
                right,
                MockWaker {
                    wake_tx: right_wake_tx,
                },
            ),
        )
    }
}

impl Driver for MockDriver {
    type Waker = MockWaker;

    fn new(socket: UdpSocket) -> io::Result<(Self, Self::Waker)> {
        let local_addr = socket.local_addr()?;
        let (driver, _peer) = Self::pair(local_addr, local_addr);
        Ok(driver)
    }

    fn poll(&mut self, deadline: Option<Instant>) -> io::Result<PollOutcome> {
        let timeout = deadline.map_or(Duration::from_millis(100), |value| {
            value.saturating_duration_since(Instant::now())
        });

        let mut outcome = PollOutcome {
            rx: Vec::new(),
            woken: false,
            timer_expired: false,
        };

        loop {
            match self.inbound_rx.try_recv() {
                Ok(datagram) => {
                    outcome.rx.push(RxDatagram {
                        data: datagram.bytes,
                        peer: datagram.from,
                        local: self.local_addr,
                        segment_size: None,
                    });
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
        if !outcome.rx.is_empty() {
            return Ok(outcome);
        }

        match self.wake_rx.try_recv() {
            Ok(()) => {
                outcome.woken = true;
                return Ok(outcome);
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {}
        }

        crossbeam_channel::select! {
            recv(self.inbound_rx) -> msg => {
                if let Ok(datagram) = msg {
                    outcome.rx.push(RxDatagram {
                        data: datagram.bytes,
                        peer: datagram.from,
                        local: self.local_addr,
                        segment_size: None,
                    });
                    while let Ok(extra) = self.inbound_rx.try_recv() {
                        outcome.rx.push(RxDatagram {
                            data: extra.bytes,
                            peer: extra.from,
                            local: self.local_addr,
                            segment_size: None,
                        });
                    }
                } else {
                    outcome.timer_expired = true;
                }
            }
            recv(self.wake_rx) -> _ => {
                outcome.woken = true;
            }
            recv(after(timeout)) -> _ => {
                outcome.timer_expired = true;
            }
        }

        Ok(outcome)
    }

    fn submit_sends(&mut self, packets: Vec<TxDatagram>) -> io::Result<()> {
        for packet in packets {
            if let Some(trace) = &self.trace {
                trace.record_datagram(self.local_addr, packet.to, &packet.data);
            }
            // Simulate packet loss if configured.
            let dropped = self
                .packet_loss
                .as_ref()
                .is_some_and(|loss| loss.should_drop());
            if let Some(loss) = &self.packet_loss {
                loss.record_send(dropped);
            }
            if !dropped {
                self.outbound_tx
                    .send(MockInboundDatagram {
                        from: self.local_addr,
                        bytes: packet.data.clone(),
                    })
                    .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "mock peer closed"))?;
            }
            self.recycled_tx.push(packet.data);
        }
        Ok(())
    }

    fn pending_tx_count(&self) -> usize {
        0
    }

    fn drain_recycled_tx(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.recycled_tx)
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local_addr)
    }

    fn driver_kind(&self) -> RuntimeDriverKind {
        RuntimeDriverKind::Mock
    }
}

impl DriverWaker for MockWaker {
    fn wake(&self) -> io::Result<()> {
        let _ = self.wake_tx.try_send(());
        Ok(())
    }
}
