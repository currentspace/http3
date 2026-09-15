//! Protocol-specific pending write flushing and admission accounting.

use super::*;

pub(super) struct PendingWriteFlushEvents<T> {
    pub(super) drained: Vec<T>,
    pub(super) released_units: usize,
}

impl<T> IntoIterator for PendingWriteFlushEvents<T> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        self.drained.into_iter()
    }
}

/// Always compiled — used by both native's
/// `ProtocolHandler::flush_pending_writes` (via `flush_all_pending_writes`)
/// and the direct-call surface, since `QuicConnectionMap` itself no
/// longer requires `os-runtime`.
pub(super) fn flush_quic_pending_writes(
    conn_map: &mut QuicConnectionMap,
    pending: &mut HashMap<(u32, u64), PendingWrite>,
) -> PendingWriteFlushEvents<(u32, u64)> {
    let mut flushed = Vec::new();
    let mut released_units = 0usize;
    pending.retain(|&(conn_handle, stream_id), pw| {
        let before = pw.queued_bytes();
        let before_units = pw.queued_units();
        let Some(conn) = conn_map.get_mut(conn_handle as usize) else {
            reactor_metrics::record_outbound_pending_write_removed(before);
            released_units += before_units;
            return false;
        };
        match flush_one_quic_pending_write(conn, stream_id, pw) {
            Ok(outcome) if outcome.done => {
                flushed.push((conn_handle, stream_id));
                released_units += outcome.released_units;
                reactor_metrics::record_outbound_pending_write_removed(before);
                false
            }
            Ok(outcome) => {
                released_units += outcome.released_units;
                reactor_metrics::record_outbound_pending_write_change(before, pw.queued_bytes());
                true
            }
            Err(e) => {
                log::warn!("flush pending write failed for stream {stream_id}: {e}");
                reactor_metrics::record_outbound_pending_write_removed(before);
                released_units += before_units;
                false
            }
        }
    });
    PendingWriteFlushEvents {
        drained: flushed,
        released_units,
    }
}

pub(super) fn flush_quic_client_pending_writes(
    conn: &mut QuicConnection,
    pending: &mut HashMap<u64, PendingWrite>,
) -> PendingWriteFlushEvents<u64> {
    let mut flushed = Vec::new();
    let mut released_units = 0usize;
    pending.retain(|&stream_id, pw| {
        let before = pw.queued_bytes();
        let before_units = pw.queued_units();
        match flush_one_quic_pending_write(conn, stream_id, pw) {
            Ok(outcome) if outcome.done => {
                flushed.push(stream_id);
                released_units += outcome.released_units;
                reactor_metrics::record_outbound_pending_write_removed(before);
                false
            }
            Ok(outcome) => {
                released_units += outcome.released_units;
                reactor_metrics::record_outbound_pending_write_change(before, pw.queued_bytes());
                true
            }
            Err(e) => {
                log::warn!("flush pending write failed for stream {stream_id}: {e}");
                reactor_metrics::record_outbound_pending_write_removed(before);
                released_units += before_units;
                false
            }
        }
    });
    PendingWriteFlushEvents {
        drained: flushed,
        released_units,
    }
}

pub(super) fn flush_one_quic_pending_write(
    conn: &mut QuicConnection,
    stream_id: u64,
    pw: &mut PendingWrite,
) -> Result<PendingWriteFlushOutcome, Http3NativeError> {
    flush_pending_write_with_progress(pw, |buf, send_fin| {
        let outcome = conn.stream_send_arcbuf(stream_id, buf, send_fin)?;
        Ok(PendingWriteSendOutcome {
            written: outcome.written,
            fin_accepted: outcome.fin_accepted,
            remainder: outcome.remainder,
        })
    })
}
