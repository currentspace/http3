use std::io;
use std::time::Instant;

use slab::Slab;

use crate::event_loop::EventBatcher;
use crate::h3_event::JsH3Event;
use crate::timer_heap::TimerHeap;
use crate::transport::{Driver, TxDatagram};

pub(crate) fn emit_runtime_error<S, D: Driver, F>(
    sessions: &mut Slab<S>,
    driver: &D,
    syscall: &str,
    reason_code: &str,
    err: &io::Error,
    mut get_batcher: F,
) where
    F: FnMut(&mut S) -> &mut EventBatcher,
{
    for (handle, session) in sessions.iter_mut() {
        let batcher = get_batcher(session);
        batcher.batch.push(JsH3Event::runtime_error(
            handle as u32,
            driver.driver_kind().as_str(),
            syscall,
            reason_code,
            err,
        ));
        if !batcher.flush() {
            break;
        }
    }
}

pub(crate) fn sync_timer<S, F>(
    timer_heap: &mut TimerHeap,
    handle: usize,
    session: &S,
    get_deadline: F,
) where
    F: Fn(&S) -> Option<Instant>,
{
    timer_heap.set_deadline(handle, get_deadline(session));
}

pub(crate) fn flush_round_robin_sends<S, F>(
    sessions: &mut Slab<S>,
    handles_buf: &mut Vec<usize>,
    outbound: &mut Vec<TxDatagram>,
    mut try_send: F,
) where
    F: FnMut(&mut S) -> Option<TxDatagram>,
{
    handles_buf.clear();
    handles_buf.extend(sessions.iter().map(|(handle, _)| handle));
    if handles_buf.is_empty() {
        return;
    }

    let count = handles_buf.len();
    let mut done = vec![false; count];
    let mut active = count;
    while active > 0 {
        for idx in 0..count {
            if done[idx] {
                continue;
            }
            let handle = handles_buf[idx];
            let sent = sessions
                .get_mut(handle)
                .and_then(|session| try_send(session))
                .map(|packet| {
                    outbound.push(packet);
                    true
                })
                .unwrap_or(false);
            if !sent {
                done[idx] = true;
                active -= 1;
            }
        }
    }
}
