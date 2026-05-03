#![deny(unsafe_code)]

use std::collections::VecDeque;
use std::time::Instant;

struct PendingPing {
    started_at: Instant,
    target_sent_packets: usize,
    baseline_acked_bytes: u64,
}

#[derive(Default)]
pub(crate) struct PingState {
    pending: VecDeque<PendingPing>,
}

impl PingState {
    pub(crate) fn queue(&mut self, stats: &quiche::Stats) {
        self.pending.push_back(PendingPing {
            started_at: Instant::now(),
            target_sent_packets: stats.sent.saturating_add(1),
            baseline_acked_bytes: stats.acked_bytes,
        });
    }

    pub(crate) fn complete_acked(&mut self, stats: &quiche::Stats) -> Vec<f64> {
        let now = Instant::now();
        let mut completed = Vec::new();
        while self.pending.front().is_some_and(|ping| {
            stats.sent >= ping.target_sent_packets && stats.acked_bytes > ping.baseline_acked_bytes
        }) {
            let Some(ping) = self.pending.pop_front() else {
                break;
            };
            completed.push(now.duration_since(ping.started_at).as_secs_f64() * 1000.0);
        }
        completed
    }
}
