//! In-memory NSS-format TLS keylog sink.
//!
//! `quiche::Connection::set_keylog` takes a boxed `Write + Send + Sync`. On native,
//! `lib/keylog.ts` tails a file the OS driver opens for that purpose. A
//! sans-IO caller (unit tests today; a future wasm build) has no
//! filesystem, so this sink accumulates keylog lines in an owned
//! `Vec<u8>` behind a mutex instead, drained on demand via
//! `H3ClientHandler::take_keylog_lines` / `QuicClientHandler::take_keylog_lines`.
//! Purely additive: native file-based keylog behavior is unchanged, this is
//! an alternate sink installed only when the caller opts in.

use std::io;
use std::sync::{Arc, Mutex};

/// Cloneable handle around the shared buffer; the `Write` impl is installed
/// into quiche, the handle itself is retained by the handler so it can drain
/// the buffer independently of the writer quiche owns.
#[derive(Clone, Default)]
pub(crate) struct KeylogBuffer(Arc<Mutex<Vec<u8>>>);

impl KeylogBuffer {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Boxed `Write + Send + Sync` sink to hand to
    /// `quiche::Connection::set_keylog`.
    pub(crate) fn writer(&self) -> Box<dyn io::Write + Send + Sync> {
        Box::new(KeylogWriter(self.0.clone()))
    }

    /// Drain and return all accumulated NSS-format keylog bytes.
    pub(crate) fn take(&self) -> Vec<u8> {
        self.0.lock().map(|mut buf| std::mem::take(&mut *buf)).unwrap_or_default()
    }
}

struct KeylogWriter(Arc<Mutex<Vec<u8>>>);

impl io::Write for KeylogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Ok(mut guard) = self.0.lock() {
            guard.extend_from_slice(buf);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn writer_accumulates_into_shared_buffer() {
        let buf = KeylogBuffer::new();
        let mut writer = buf.writer();
        writer.write_all(b"CLIENT_HANDSHAKE_TRAFFIC_SECRET line one\n").unwrap();
        writer.write_all(b"line two\n").unwrap();

        let drained = buf.take();
        assert_eq!(
            drained,
            b"CLIENT_HANDSHAKE_TRAFFIC_SECRET line one\nline two\n"
        );
        // Draining empties the buffer.
        assert!(buf.take().is_empty());
    }
}
