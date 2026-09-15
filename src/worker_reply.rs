//! Bounded waits for synchronous commands sent to native workers.

#![deny(unsafe_code)]

use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError};

/// Wait for a worker response without scheduler-yield backoff on macOS.
///
/// Crossbeam's bounded `recv_timeout` spins, then calls `thread::yield_now`
/// before parking. On macOS those yields can cost a scheduler timeslice even
/// when the worker has already replied. Repeated synchronous request opens
/// accumulated roughly 10 ms per call and stalled the Node event loop.
/// `Select` registers the receive and parks directly, preserving the channel's
/// timeout, disconnection, and queued-response semantics.
pub(crate) fn recv_worker_reply<T>(
    receiver: &Receiver<T>,
    timeout: Duration,
) -> Result<T, RecvTimeoutError> {
    #[cfg(target_os = "macos")]
    {
        let mut select = crossbeam_channel::Select::new_biased();
        select.recv(receiver);
        let operation = select
            .select_timeout(timeout)
            .map_err(|_| RecvTimeoutError::Timeout)?;
        operation
            .recv(receiver)
            .map_err(|_| RecvTimeoutError::Disconnected)
    }
    #[cfg(not(target_os = "macos"))]
    receiver.recv_timeout(timeout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn receives_queued_reply_after_sender_disconnects() {
        let (sender, receiver) = crossbeam_channel::bounded(1);
        sender.send(42).unwrap();
        drop(sender);
        assert_eq!(recv_worker_reply(&receiver, Duration::ZERO), Ok(42));
        assert_eq!(
            recv_worker_reply(&receiver, Duration::ZERO),
            Err(RecvTimeoutError::Disconnected)
        );
    }

    #[test]
    fn wakes_for_reply_from_another_thread() {
        let (sender, receiver) = crossbeam_channel::bounded(1);
        thread::scope(|scope| {
            scope.spawn(move || {
                thread::sleep(Duration::from_millis(20));
                sender.send(42).unwrap();
            });
            assert_eq!(recv_worker_reply(&receiver, Duration::from_secs(2)), Ok(42));
        });
    }

    #[test]
    fn wakes_when_worker_drops_reply_sender() {
        let (sender, receiver) = crossbeam_channel::bounded::<u32>(1);
        thread::scope(|scope| {
            scope.spawn(move || {
                thread::sleep(Duration::from_millis(20));
                drop(sender);
            });
            assert_eq!(
                recv_worker_reply(&receiver, Duration::from_secs(2)),
                Err(RecvTimeoutError::Disconnected)
            );
        });
    }

    #[test]
    fn timeout_unregisters_receive_and_preserves_late_reply() {
        let (sender, receiver) = crossbeam_channel::bounded(1);
        assert_eq!(
            recv_worker_reply(&receiver, Duration::from_millis(5)),
            Err(RecvTimeoutError::Timeout)
        );
        sender.send(42).unwrap();
        assert_eq!(recv_worker_reply(&receiver, Duration::ZERO), Ok(42));
    }

    #[test]
    fn repeated_handoffs_do_not_lose_wakeups() {
        let (sender, receiver) = crossbeam_channel::bounded(1);
        thread::scope(|scope| {
            scope.spawn(move || {
                for value in 0..1000 {
                    sender.send(value).unwrap();
                }
            });
            for expected in 0..1000 {
                assert_eq!(
                    recv_worker_reply(&receiver, Duration::from_secs(2)),
                    Ok(expected)
                );
            }
        });
    }
}
