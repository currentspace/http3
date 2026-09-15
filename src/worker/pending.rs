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

pub(super) struct PendingResponseFlushOutcome {
    pub(super) done: bool,
    pub(super) released_units: usize,
}

pub(super) fn flush_one_h3_pending_response(
    conn: &mut H3Connection,
    stream_id: u64,
    response: &mut PendingResponse,
) -> Result<PendingResponseFlushOutcome, Http3NativeError> {
    if !response.headers_sent {
        let headers = h3_headers(&response.headers);
        let fin = response.headers_fin && response.body.is_none();
        match conn.send_response(stream_id, &headers, fin) {
            Ok(()) => {
                response.headers_sent = true;
            }
            Err(e) if is_h3_stream_blocked(&e) => {
                return Ok(PendingResponseFlushOutcome {
                    done: false,
                    released_units: 0,
                });
            }
            Err(e) => return Err(e),
        }
    }

    let Some(body) = response.body.as_mut() else {
        return Ok(PendingResponseFlushOutcome {
            done: true,
            released_units: 0,
        });
    };

    let outcome = flush_one_h3_pending_write(conn, stream_id, body)?;
    if outcome.done {
        response.body = None;
    }

    Ok(PendingResponseFlushOutcome {
        done: outcome.done,
        released_units: outcome.released_units,
    })
}

/// Flush buffered partial writes for all streams. Always compiled (used by
/// both native's `ProtocolHandler::flush_pending_writes` and the
/// direct-call `flush_all_pending_writes`, since `ConnectionMap` itself no
/// longer requires `os-runtime`).
pub(super) fn flush_pending_writes(
    conn_map: &mut ConnectionMap,
    pending: &mut HashMap<(u32, u64), PendingWrite>,
    _pool: &mut AdaptiveBufferPool,
) -> PendingWriteFlushEvents<(u32, u64)> {
    let mut flushed = Vec::new();
    let mut released_units = 0usize;
    pending.retain(|&(conn_handle, stream_id), pw| {
        let before = pw.queued_bytes();
        let before_units = pw.queued_units();
        let Some(conn) = conn_map.get_mut(conn_handle as usize) else {
            // PendingWrite drops -> chunk recycles to pool
            reactor_metrics::record_outbound_pending_write_removed(before);
            released_units += before_units;
            return false;
        };
        if conn.quiche_conn.stream_closed(stream_id) {
            reactor_metrics::record_outbound_pending_write_removed(before);
            released_units += before_units;
            return false;
        }
        match flush_one_h3_pending_write(conn, stream_id, pw) {
            Ok(outcome) if outcome.done => {
                flushed.push((conn_handle, stream_id));
                released_units += outcome.released_units;
                reactor_metrics::record_outbound_pending_write_removed(before);
                // PendingWrite drops -> chunk recycles to pool
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
                false // Remove — stream is dead
            }
        }
    });
    PendingWriteFlushEvents {
        drained: flushed,
        released_units,
    }
}

/// Flush buffered partial writes for client streams.
pub(super) fn flush_client_pending_writes(
    conn: &mut H3Connection,
    pending: &mut HashMap<u64, PendingWrite>,
    _pool: &mut AdaptiveBufferPool,
) -> PendingWriteFlushEvents<u64> {
    let mut flushed = Vec::new();
    let mut released_units = 0usize;
    pending.retain(|&stream_id, pw| {
        let before = pw.queued_bytes();
        let before_units = pw.queued_units();
        if conn.quiche_conn.stream_closed(stream_id) {
            reactor_metrics::record_outbound_pending_write_removed(before);
            released_units += before_units;
            return false;
        }
        match flush_one_h3_pending_write(conn, stream_id, pw) {
            Ok(outcome) if outcome.done => {
                flushed.push(stream_id);
                released_units += outcome.released_units;
                reactor_metrics::record_outbound_pending_write_removed(before);
                // PendingWrite drops -> chunk recycles to pool
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
                false // Remove — stream is dead
            }
        }
    });
    PendingWriteFlushEvents {
        drained: flushed,
        released_units,
    }
}

pub(super) fn flush_one_h3_pending_write(
    conn: &mut H3Connection,
    stream_id: u64,
    pw: &mut PendingWrite,
) -> Result<PendingWriteFlushOutcome, Http3NativeError> {
    flush_pending_write_with_progress(pw, |buf, send_fin| {
        let outcome = conn.send_body_arcbuf(stream_id, buf, send_fin)?;
        Ok(PendingWriteSendOutcome {
            written: outcome.written,
            fin_accepted: outcome.fin_accepted,
            remainder: outcome.remainder,
        })
    })
}
