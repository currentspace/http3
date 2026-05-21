//! Shared pending-write queue state for H3 and raw QUIC streams.

#![deny(unsafe_code)]

use std::collections::VecDeque;

#[cfg(any(test, feature = "fuzzing"))]
use quiche::BufSplit;

use crate::arc_buf::ArcBuf;
use crate::chunk_pool::Chunk;
use crate::proof_core::pending_write_model::{self, PendingWriteSnapshot};

/// Buffered partial write for a stream blocked by local/quiche flow control.
pub(crate) struct PendingWrite {
    chunks: VecDeque<ArcBuf>,
    fin: bool,
}

impl PendingWrite {
    pub(crate) fn new(remainder: ArcBuf, fin: bool) -> Self {
        let mut chunks = VecDeque::new();
        chunks.push_back(remainder);
        Self { chunks, fin }
    }

    pub(crate) fn push_chunk(&mut self, chunk: Chunk) -> usize {
        let queued = chunk.remaining_len();
        if queued > 0 {
            self.chunks.push_back(ArcBuf::from_chunk(chunk));
        }
        queued
    }

    pub(crate) fn set_fin(&mut self) {
        self.fin = true;
    }

    pub(crate) fn queued_bytes(&self) -> usize {
        self.chunks.iter().map(ArcBuf::remaining_len).sum()
    }

    pub(crate) fn queued_units(&self) -> usize {
        self.snapshot().queued_units()
    }

    pub(crate) fn snapshot(&self) -> PendingWriteSnapshot {
        PendingWriteSnapshot::new(self.queued_bytes(), self.fin)
    }
}

pub(crate) struct PendingWriteSendOutcome {
    pub(crate) written: usize,
    pub(crate) fin_accepted: bool,
    pub(crate) remainder: Option<ArcBuf>,
}

pub(crate) struct PendingWriteFlushOutcome {
    pub(crate) done: bool,
    pub(crate) released_units: usize,
}

pub(crate) fn flush_pending_write_with_progress<E, F>(
    pw: &mut PendingWrite,
    mut send: F,
) -> Result<PendingWriteFlushOutcome, E>
where
    F: FnMut(ArcBuf, bool) -> Result<PendingWriteSendOutcome, E>,
{
    let mut released_units = 0usize;
    loop {
        if let Some(buf) = pw.chunks.pop_front() {
            let payload_len = buf.remaining_len();
            let send_fin = pw.fin && pw.chunks.is_empty();
            let outcome = send(buf, send_fin)?;
            released_units += pending_write_model::released_units_for_send(
                payload_len,
                send_fin,
                outcome.written,
                outcome.fin_accepted,
            );
            if let Some(remainder) = outcome.remainder {
                pw.chunks.push_front(remainder);
                return Ok(PendingWriteFlushOutcome {
                    done: false,
                    released_units,
                });
            }
            if send_fin {
                return Ok(PendingWriteFlushOutcome {
                    done: outcome.fin_accepted,
                    released_units,
                });
            }
            continue;
        }

        if pw.fin {
            let payload_len = 0;
            let outcome = send(ArcBuf::from_vec(Vec::new()), true)?;
            released_units += pending_write_model::released_units_for_send(
                payload_len,
                true,
                outcome.written,
                outcome.fin_accepted,
            );
            if let Some(remainder) = outcome.remainder {
                pw.chunks.push_front(remainder);
                return Ok(PendingWriteFlushOutcome {
                    done: false,
                    released_units,
                });
            }
            return Ok(PendingWriteFlushOutcome {
                done: outcome.fin_accepted,
                released_units,
            });
        }

        return Ok(PendingWriteFlushOutcome {
            done: true,
            released_units,
        });
    }
}

pub(crate) fn flush_pending_write<E, F>(pw: &mut PendingWrite, send: F) -> Result<bool, E>
where
    F: FnMut(ArcBuf, bool) -> Result<PendingWriteSendOutcome, E>,
{
    Ok(flush_pending_write_with_progress(pw, send)?.done)
}

#[cfg(feature = "fuzzing")]
pub(crate) fn fuzz_pending_write_state(data: &[u8]) {
    let ops = data.chunks(3).map(|op| {
        (
            op.first().copied().unwrap_or(0),
            op.get(1).copied().unwrap_or(0),
            op.get(2).copied().unwrap_or(0),
        )
    });
    fuzz_pending_write_state_ops(ops);
}

#[cfg(feature = "fuzzing")]
pub(crate) fn fuzz_pending_write_state_ops<I>(ops: I)
where
    I: IntoIterator<Item = (u8, u8, u8)>,
{
    struct FakeSender {
        max_accept: usize,
        accepted: usize,
        fin_accepted: bool,
        done: bool,
    }

    let mut pending: Option<PendingWrite> = None;
    let mut expected_queued = 0usize;
    let mut closed = false;
    let mut fin_requested = false;
    let mut fin_accepted = false;

    for (tag_byte, len_byte, arg) in ops {
        let tag = tag_byte % 5;
        let len = len_byte as usize % 65;

        match tag {
            // Append a data chunk, optionally carrying FIN.
            0 => {
                if closed {
                    continue;
                }
                let wants_fin = (arg & 0x1) != 0;
                if wants_fin {
                    fin_requested = true;
                }
                let chunk = Chunk::unpooled(vec![arg; len]);
                match pending.as_mut() {
                    Some(pw) => {
                        expected_queued += pw.push_chunk(chunk);
                        if wants_fin {
                            pw.set_fin();
                        }
                    }
                    None => {
                        let write = PendingWrite::new(ArcBuf::from_chunk(chunk), wants_fin);
                        expected_queued = write.queued_bytes();
                        pending = Some(write);
                    }
                }
            }
            // Flush with a bounded local/quiche accept budget.
            1 => {
                if closed {
                    pending = None;
                    expected_queued = 0;
                    continue;
                }
                if let Some(pw) = pending.as_mut() {
                    let mut sender = FakeSender {
                        max_accept: len,
                        accepted: 0,
                        fin_accepted: false,
                        done: false,
                    };
                    let done = flush_pending_write(pw, |buf, send_fin| {
                        let remaining = buf.remaining_len();
                        if sender.done || sender.accepted >= sender.max_accept {
                            sender.done = true;
                            return Ok::<PendingWriteSendOutcome, ()>(PendingWriteSendOutcome {
                                written: 0,
                                fin_accepted: false,
                                remainder: Some(buf),
                            });
                        }

                        let accepted = remaining.min(sender.max_accept - sender.accepted);
                        sender.accepted += accepted;
                        let remainder = if accepted < remaining {
                            let mut head = buf;
                            Some(head.split_at(accepted))
                        } else {
                            None
                        };
                        let accepted_fin = send_fin && remainder.is_none();
                        sender.fin_accepted |= accepted_fin;
                        Ok(PendingWriteSendOutcome {
                            written: accepted,
                            fin_accepted: accepted_fin,
                            remainder,
                        })
                    })
                    .expect("fake sender cannot fail");

                    expected_queued = expected_queued.saturating_sub(sender.accepted);
                    if sender.fin_accepted {
                        assert!(fin_requested);
                        fin_accepted = true;
                    }
                    if done {
                        assert_eq!(pw.queued_bytes(), 0);
                        pending = None;
                        expected_queued = 0;
                    }
                }
            }
            // Done/no-progress notification: state must be unchanged.
            2 => {
                if let Some(pw) = pending.as_ref() {
                    assert_eq!(pw.queued_bytes(), expected_queued);
                }
            }
            // Close/reset: production paths drop the pending write.
            3 => {
                closed = true;
                pending = None;
                expected_queued = 0;
            }
            // Re-open a fresh model stream after a close.
            _ => {
                closed = false;
                fin_requested = false;
                fin_accepted = false;
            }
        }

        if let Some(pw) = pending.as_ref() {
            assert_eq!(pw.queued_bytes(), expected_queued);
        } else {
            assert_eq!(expected_queued, 0);
        }
        assert!(!fin_accepted || fin_requested);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flush_fin_only_pending_write_accepts_fin() {
        let mut pending = PendingWrite::new(ArcBuf::from_vec(Vec::new()), true);
        let outcome = flush_pending_write_with_progress(&mut pending, |buf, send_fin| {
            assert_eq!(buf.remaining_len(), 0);
            assert!(send_fin);
            Ok::<_, ()>(PendingWriteSendOutcome {
                written: 0,
                fin_accepted: true,
                remainder: None,
            })
        })
        .unwrap();

        assert!(outcome.done);
        assert_eq!(outcome.released_units, 1);
        assert_eq!(pending.queued_bytes(), 0);
    }

    #[test]
    fn flush_partial_accept_preserves_remainder() {
        let mut pending = PendingWrite::new(ArcBuf::from_vec(vec![1; 8]), true);
        let outcome = flush_pending_write_with_progress(&mut pending, |buf, send_fin| {
            assert!(send_fin);
            Ok::<_, ()>(PendingWriteSendOutcome {
                written: 3,
                fin_accepted: false,
                remainder: {
                    let mut head = buf;
                    Some(head.split_at(3))
                },
            })
        })
        .unwrap();

        assert!(!outcome.done);
        assert_eq!(outcome.released_units, 3);
        assert_eq!(pending.queued_bytes(), 5);
    }

    #[test]
    fn flush_full_payload_with_deferred_fin_keeps_one_unit() {
        let mut pending = PendingWrite::new(ArcBuf::from_vec(vec![1; 8]), true);
        let outcome = flush_pending_write_with_progress(&mut pending, |buf, send_fin| {
            assert!(send_fin);
            Ok::<_, ()>(PendingWriteSendOutcome {
                written: buf.remaining_len(),
                fin_accepted: false,
                remainder: None,
            })
        })
        .unwrap();

        assert!(!outcome.done);
        assert_eq!(outcome.released_units, 7);
        assert_eq!(pending.queued_units(), 1);
    }
}
