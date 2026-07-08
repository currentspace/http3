//! Shared ABI conventions: the error-code enum, the global last-error slot
//! (`handle = 0`), `wasm_alloc`/`wasm_free`, and the raw-pointer helpers
//! every `h3c_*`/`qc_*` export builds on. See the crate-level doc comment
//! for the full normative contract.

use std::cell::RefCell;

/// Retryable: blocked/would-block (`stream_send` backpressure; a
/// `send_request` message containing `StreamBlocked`).
pub(crate) const ERR_AGAIN: i64 = -1;
/// Protocol or config error — fetch the message via `*_last_error`.
pub(crate) const ERR_PROTOCOL: i64 = -2;
/// The handle does not refer to a live session (already freed, or never
/// valid).
pub(crate) const ERR_INVALID_HANDLE: i64 = -3;
/// Malformed arguments (bad JSON, wrong shape) that never reached the
/// protocol layer.
pub(crate) const ERR_BAD_ARGS: i64 = -4;

/// Fixed RX/TX datagram staging region size (§5.3 "fixed 64 KiB … staging
/// region").
pub(crate) const RX_TX_BUFFER_LEN: usize = 65536;

thread_local! {
    static GLOBAL_LAST_ERROR: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Set by any export that fails before (or without) a live handle —
/// notably `h3c_new`/`qc_new` construction failures, which return handle
/// `0` and expect the caller to read this slot via `*_last_error(0, …)`.
pub(crate) fn set_global_error(msg: String) {
    GLOBAL_LAST_ERROR.with(|cell| *cell.borrow_mut() = Some(msg));
}

pub(crate) fn take_global_error_for_read() -> Option<String> {
    GLOBAL_LAST_ERROR.with(|cell| cell.borrow().clone())
}

/// Allocate `size` bytes in this module's own linear memory and return a
/// pointer callers can write into before passing it to another export
/// (options JSON, header JSON, stream payloads). Caller must release it
/// with `wasm_free(ptr, size)` — the same `size` used to allocate.
///
/// # Safety
/// Exported to an untrusted-in-the-Rust-type-system-sense host; safe in
/// practice because wasm's linear memory model means "the pointer" is
/// just an offset into memory only this module and its host can touch.
#[unsafe(no_mangle)]
pub extern "C" fn wasm_alloc(size: u32) -> u32 {
    let mut buf: Vec<u8> = Vec::with_capacity(size as usize);
    let ptr = buf.as_mut_ptr();
    // Leak intentionally: ownership transfers to the host until it calls
    // `wasm_free` with the same (ptr, size).
    std::mem::forget(buf);
    ptr as u32
}

/// # Safety
/// `ptr`/`size` must be exactly the pair most recently returned by
/// `wasm_alloc` for that allocation (not yet freed).
#[unsafe(no_mangle)]
pub extern "C" fn wasm_free(ptr: u32, size: u32) {
    if ptr == 0 {
        return;
    }
    // Reconstruct and drop — `len = 0` is fine for `u8` (no per-element
    // Drop glue); only the allocation's (ptr, capacity) need to match what
    // the global allocator originally handed out.
    let buf = unsafe { Vec::from_raw_parts(ptr as *mut u8, 0, size as usize) };
    drop(buf);
}

/// # Safety
/// `ptr`/`len` must describe a valid, readable byte range of at least
/// `len` bytes in this module's linear memory, for the duration of the
/// borrow (i.e. the caller must not also be concurrently freeing it —
/// trivially true, wasm32-wasip1 is single-threaded and this module never
/// calls back into the host mid-export).
pub(crate) unsafe fn bytes_in<'a>(ptr: u32, len: u32) -> &'a [u8] {
    if ptr == 0 || len == 0 {
        return &[];
    }
    unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) }
}

/// Like [`bytes_in`], but validates UTF-8 (returns `None` on invalid
/// UTF-8 rather than trapping).
///
/// # Safety
/// Same as [`bytes_in`].
pub(crate) unsafe fn str_in<'a>(ptr: u32, len: u32) -> Option<&'a str> {
    std::str::from_utf8(unsafe { bytes_in(ptr, len) }).ok()
}

/// Copies up to `cap` bytes of `msg` into `buf_ptr` (a caller-owned
/// buffer), returning the number of bytes written (`0` if `msg` is
/// `None`). Used by `*_last_error`.
pub(crate) fn write_out_message(msg: Option<String>, buf_ptr: u32, cap: u32) -> i32 {
    let Some(msg) = msg else { return 0 };
    if buf_ptr == 0 || cap == 0 {
        return 0;
    }
    let bytes = msg.as_bytes();
    let n = bytes.len().min(cap as usize);
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf_ptr as *mut u8, n);
    }
    n as i32
}

/// Writes `scratch.as_ptr()` (an absolute linear-memory address) into the
/// `u32` slot at `out_ptr_ptr`, returning `scratch.len()`. This is the
/// `(handle, out_ptr_ptr) -> len` convention used by `drain_events`,
/// `session_metrics`, `remote_settings`, and `take_keylog`: the pointer
/// written back is only valid until the next call on the same handle
/// (`scratch` is a per-session buffer reused on every such call).
pub(crate) fn write_out_ptr_len(scratch: &[u8], out_ptr_ptr: u32) -> i64 {
    if out_ptr_ptr != 0 {
        unsafe {
            std::ptr::write(out_ptr_ptr as *mut u32, scratch.as_ptr() as u32);
        }
    }
    scratch.len() as i64
}

#[cfg(test)]
mod tests {
    use super::write_out_message;

    // The tests below round-trip a *real* pointer through this module's
    // `u32` ABI convention. That convention is only sound where pointers
    // truly are 32 bits — i.e. the actual wasm32-wasip1 target this crate
    // ships for, where "the pointer" is an offset into a <4 GiB linear
    // memory. On a 64-bit host test run, a real heap/stack address
    // routinely exceeds `u32::MAX`; truncating it and dereferencing the
    // truncated value is exactly the SIGSEGV this cfg-gate avoids. Gate
    // to `target_pointer_width = "32"` rather than deleting the coverage
    // outright — the logic remains exercised on any 32-bit target.
    #[cfg(target_pointer_width = "32")]
    mod pointer_truncation_is_sound_here {
        use super::super::{bytes_in, wasm_alloc, wasm_free, write_out_ptr_len};

        #[test]
        fn alloc_write_read_free_round_trips() {
            let ptr = wasm_alloc(4);
            assert_ne!(ptr, 0);
            unsafe {
                std::ptr::copy_nonoverlapping([1u8, 2, 3, 4].as_ptr(), ptr as *mut u8, 4);
            }
            let read = unsafe { bytes_in(ptr, 4) };
            assert_eq!(read, &[1, 2, 3, 4]);
            wasm_free(ptr, 4);
        }

        #[test]
        fn write_out_message_truncates_to_cap() {
            let mut buf = [0u8; 3];
            let n = super::super::write_out_message(
                Some("hello".to_string()),
                buf.as_mut_ptr() as u32,
                3,
            );
            assert_eq!(n, 3);
            assert_eq!(&buf, b"hel");
        }

        #[test]
        fn write_out_ptr_len_writes_pointer_and_returns_len() {
            let scratch = vec![9u8, 9, 9];
            let mut slot: u32 = 0;
            let len = write_out_ptr_len(&scratch, std::ptr::addr_of_mut!(slot) as u32);
            assert_eq!(len, 3);
            assert_eq!(slot, scratch.as_ptr() as u32);
        }
    }

    #[test]
    fn write_out_message_none_is_zero() {
        // Safe unconditionally: `msg = None` returns before the pointer
        // is ever dereferenced, so the (unused) truncated address is
        // harmless garbage rather than a dangling pointer that gets read.
        let mut buf = [0u8; 3];
        let n = write_out_message(None, buf.as_mut_ptr() as u32, 3);
        assert_eq!(n, 0);
    }
}
