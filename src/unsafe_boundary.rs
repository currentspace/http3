//! Narrow wrappers for invariants that otherwise tend to leak unsafe code.

use bytes::BufMut;

use crate::proof_core::{recv_buf_model::RecvBufModel, ring_layout};

/// A packet buffer whose bytes are initialized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitializedPacketBuf(Vec<u8>);

impl InitializedPacketBuf {
    pub fn zeroed(len: usize) -> Self {
        Self(vec![0; len])
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.0
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }

    pub fn truncate(&mut self, len: usize) {
        self.0.truncate(len);
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.0
    }
}

/// Owned receive output buffer for quiche APIs that write into `&mut [u8]`.
///
/// The vector length stays at zero while bytes are being filled, so safe Rust
/// code cannot accidentally observe stale bytes from a previous Buffer lease.
/// Receive calls use quiche's `*_buf` APIs over `bytes::BufMut`, avoiding an
/// uninitialized `&mut [u8]` escape hatch at the crate boundary.
#[derive(Debug)]
pub struct QuicheRecvBuf {
    buf: Vec<u8>,
    model: RecvBufModel,
}

impl QuicheRecvBuf {
    pub fn with_capacity(target_len: usize) -> Self {
        Self::from_vec(Vec::with_capacity(target_len), target_len)
    }

    pub fn from_vec(mut buf: Vec<u8>, target_len: usize) -> Self {
        buf.clear();
        if buf.capacity() < target_len {
            buf.reserve_exact(target_len);
        }
        Self {
            buf,
            model: RecvBufModel::new(target_len),
        }
    }

    pub fn initialized_len(&self) -> usize {
        self.model.initialized_len()
    }

    pub fn is_empty(&self) -> bool {
        self.model.is_empty()
    }

    pub fn is_full(&self) -> bool {
        self.model.is_full()
    }

    pub fn remaining(&self) -> usize {
        self.model.remaining()
    }

    pub fn recv_h3_body<F>(
        &mut self,
        h3_conn: &mut quiche::h3::Connection,
        quiche_conn: &mut quiche::Connection<F>,
        stream_id: u64,
    ) -> quiche::h3::Result<usize>
    where
        F: quiche::BufFactory,
    {
        let before = self.model.initialized_len();
        let remaining = self.remaining();
        let mut out = (&mut self.buf).limit(remaining);
        let written = h3_conn.recv_body_buf(quiche_conn, stream_id, &mut out)?;
        self.validate_reported_write(before, remaining, written);
        Ok(written)
    }

    pub fn recv_quic_stream<F>(
        &mut self,
        quiche_conn: &mut quiche::Connection<F>,
        stream_id: u64,
    ) -> quiche::Result<bool>
    where
        F: quiche::BufFactory,
    {
        let before = self.model.initialized_len();
        let remaining = self.remaining();
        let mut out = (&mut self.buf).limit(remaining);
        let (written, fin) = quiche_conn.stream_recv_buf(stream_id, &mut out)?;
        self.validate_reported_write(before, remaining, written);
        Ok(fin)
    }

    pub fn append_initialized(&mut self, data: &[u8]) -> usize {
        let len = self.model.append_len(data.len());
        self.buf.extend_from_slice(&data[..len]);
        self.model = self.model.after_append(len);
        len
    }

    fn validate_reported_write(&mut self, before: usize, remaining: usize, written: usize) {
        assert!(
            RecvBufModel::reported_write_is_valid(before, remaining, written, self.buf.len()),
            "quiche receive API reported {written} bytes for {remaining}-byte buffer"
        );
        self.model = self.model.after_append(written);
    }

    pub fn into_initialized_vec(self) -> Vec<u8> {
        self.buf
    }

    pub fn into_recycle_vec(mut self) -> Vec<u8> {
        self.buf.clear();
        self.buf
    }
}

/// Validated io_uring provided-buffer ID.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct ProvidedBufferId(u16);

impl ProvidedBufferId {
    pub fn new(bid: u16, ring_size: u16) -> Option<Self> {
        ring_layout::provided_buffer_id_is_valid(bid, ring_size).then_some(Self(bid))
    }

    pub fn get(self) -> u16 {
        self.0
    }
}

#[cfg(feature = "node-api")]
mod external_vec_lease {
    use std::ffi::c_void;
    use std::sync::Arc;
    #[cfg(test)]
    use std::sync::Mutex;

    use crate::buffer_pool::BufferRecycler;

    #[cfg(test)]
    use std::sync::atomic::{AtomicI32, Ordering};

    #[cfg(test)]
    static FORCED_EXTERNAL_BUFFER_STATUS: AtomicI32 = AtomicI32::new(FORCED_STATUS_NONE);
    #[cfg(test)]
    const FORCED_STATUS_NONE: napi::sys::napi_status = i32::MIN;
    #[cfg(test)]
    static EXTERNAL_BUFFER_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// Owns a `Vec<u8>` while it is being transferred to a N-API external
    /// Buffer. Public methods keep the raw-pointer lifetime accounting local
    /// to this module.
    pub(crate) struct ExternalVecLease {
        data: Vec<u8>,
        recycler: Option<Arc<BufferRecycler>>,
    }

    impl ExternalVecLease {
        pub(crate) fn new(data: Vec<u8>, recycler: Option<Arc<BufferRecycler>>) -> Self {
            Self { data, recycler }
        }

        pub(crate) fn into_napi_value(
            self,
            env: napi::sys::napi_env,
        ) -> napi::Result<napi::sys::napi_value> {
            if self.data.is_empty() {
                return create_empty_buffer(env);
            }

            match self.recycler {
                Some(recycler) => create_recyclable_external_buffer(env, self.data, recycler),
                None => {
                    let buf = napi::bindgen_prelude::Buffer::from(self.data);
                    unsafe { napi::bindgen_prelude::ToNapiValue::to_napi_value(env, buf) }
                }
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn force_external_buffer_generic_failure(force: bool) {
        force_external_buffer_status(force.then_some(napi::sys::Status::napi_generic_failure));
    }

    #[cfg(test)]
    pub(crate) fn force_external_buffer_status(status: Option<napi::sys::napi_status>) {
        FORCED_EXTERNAL_BUFFER_STATUS.store(status.unwrap_or(FORCED_STATUS_NONE), Ordering::SeqCst);
    }

    fn create_empty_buffer(env: napi::sys::napi_env) -> napi::Result<napi::sys::napi_value> {
        let mut ret = std::ptr::null_mut();
        let status =
            unsafe { napi::sys::napi_create_buffer(env, 0, std::ptr::null_mut(), &mut ret) };
        if status != napi::sys::Status::napi_ok {
            return Err(napi::Error::from_status(status.into()));
        }
        Ok(ret)
    }

    fn create_recyclable_external_buffer(
        env: napi::sys::napi_env,
        data: Vec<u8>,
        recycler: Arc<BufferRecycler>,
    ) -> napi::Result<napi::sys::napi_value> {
        create_recyclable_external_buffer_with_copy(env, data, recycler, copy_vec_to_napi)
    }

    fn create_recyclable_external_buffer_with_copy<F>(
        env: napi::sys::napi_env,
        mut data: Vec<u8>,
        recycler: Arc<BufferRecycler>,
        copy_fallback: F,
    ) -> napi::Result<napi::sys::napi_value>
    where
        F: FnOnce(napi::sys::napi_env, Vec<u8>) -> napi::Result<napi::sys::napi_value>,
    {
        let len = data.len();
        let ptr = data.as_mut_ptr();
        let cap = data.capacity();
        std::mem::forget(data);

        let hint = Box::new(RecycleHint { ptr, cap, recycler });
        let hint_ptr = Box::into_raw(hint);
        let mut ret = std::ptr::null_mut();
        let status = create_external_buffer(
            env,
            len,
            ptr.cast::<c_void>(),
            Some(finalize_recycle),
            hint_ptr.cast::<c_void>(),
            &mut ret,
        );

        if status == napi::sys::Status::napi_no_external_buffers_allowed {
            // N-API did not take ownership. Rebuild the Vec and use the
            // standard copy path.
            drop(unsafe { Box::from_raw(hint_ptr) });
            let data = unsafe { Vec::from_raw_parts(ptr, len, cap) };
            return copy_fallback(env, data);
        }

        if status != napi::sys::Status::napi_ok {
            // N-API did not register the finalizer. Reclaim everything here.
            drop(unsafe { Box::from_raw(hint_ptr) });
            drop(unsafe { Vec::from_raw_parts(ptr, len, cap) });
            return Err(napi::Error::from_status(status.into()));
        }

        Ok(ret)
    }

    fn copy_vec_to_napi(
        env: napi::sys::napi_env,
        data: Vec<u8>,
    ) -> napi::Result<napi::sys::napi_value> {
        let buf = napi::bindgen_prelude::Buffer::from(data);
        unsafe { napi::bindgen_prelude::ToNapiValue::to_napi_value(env, buf) }
    }

    fn create_external_buffer(
        env: napi::sys::napi_env,
        len: usize,
        data: *mut c_void,
        finalize_cb: napi::sys::napi_finalize,
        finalize_hint: *mut c_void,
        result: *mut napi::sys::napi_value,
    ) -> napi::sys::napi_status {
        #[cfg(test)]
        {
            let status = FORCED_EXTERNAL_BUFFER_STATUS.load(Ordering::SeqCst);
            if status != FORCED_STATUS_NONE {
                return status;
            }
        }

        unsafe {
            napi::sys::napi_create_external_buffer(
                env,
                len,
                data,
                finalize_cb,
                finalize_hint,
                result,
            )
        }
    }

    struct RecycleHint {
        ptr: *mut u8,
        cap: usize,
        recycler: Arc<BufferRecycler>,
    }

    // SAFETY: the pointer is only reconstructed in the finalizer after V8 no
    // longer exposes the buffer to JS.
    unsafe impl Send for RecycleHint {}

    unsafe extern "C" fn finalize_recycle(
        _env: napi::sys::napi_env,
        _data: *mut c_void,
        hint: *mut c_void,
    ) {
        let hint = unsafe { Box::from_raw(hint.cast::<RecycleHint>()) };
        let buf = unsafe { Vec::from_raw_parts(hint.ptr, 0, hint.cap) };
        hint.recycler.recycle(buf);
    }

    #[cfg(test)]
    fn finalize_recycle_for_test(mut data: Vec<u8>, recycler: Arc<BufferRecycler>) {
        let ptr = data.as_mut_ptr();
        let cap = data.capacity();
        std::mem::forget(data);
        let hint = Box::new(RecycleHint { ptr, cap, recycler });
        let hint_ptr = Box::into_raw(hint);
        unsafe { finalize_recycle(std::ptr::null_mut(), std::ptr::null_mut(), hint_ptr.cast()) };
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn generic_failure_reclaims_vec_instead_of_recycling() {
            let _guard = EXTERNAL_BUFFER_TEST_LOCK
                .lock()
                .expect("test lock poisoned");
            let (_pool, recycler, rx) = crate::buffer_pool::AdaptiveBufferPool::with_recycler(8, 1);
            force_external_buffer_generic_failure(true);
            let result = ExternalVecLease::new(vec![1, 2, 3, 4], Some(recycler))
                .into_napi_value(std::ptr::null_mut());
            force_external_buffer_generic_failure(false);

            assert!(result.is_err());
            assert!(rx.try_recv().is_err());
        }

        #[test]
        fn no_external_buffers_allowed_uses_copy_fallback_without_recycling() {
            let _guard = EXTERNAL_BUFFER_TEST_LOCK
                .lock()
                .expect("test lock poisoned");
            let (_pool, recycler, rx) = crate::buffer_pool::AdaptiveBufferPool::with_recycler(8, 1);
            force_external_buffer_status(Some(napi::sys::Status::napi_no_external_buffers_allowed));

            let result = create_recyclable_external_buffer_with_copy(
                std::ptr::null_mut(),
                vec![5, 6, 7, 8],
                recycler,
                |env, data| {
                    assert!(env.is_null());
                    assert_eq!(data, vec![5, 6, 7, 8]);
                    Err(napi::Error::from_status(napi::Status::GenericFailure))
                },
            );
            force_external_buffer_status(None);

            assert!(result.is_err());
            assert!(rx.try_recv().is_err());
        }

        #[test]
        fn finalizer_recycles_vec_with_original_capacity() {
            let (_pool, recycler, rx) = crate::buffer_pool::AdaptiveBufferPool::with_recycler(8, 1);
            let mut data = Vec::with_capacity(64);
            data.extend_from_slice(&[1, 2, 3, 4]);
            let original_capacity = data.capacity();

            finalize_recycle_for_test(data, recycler);

            let recycled = rx.try_recv().expect("finalizer should recycle buffer");
            assert_eq!(recycled.len(), 0);
            assert_eq!(recycled.capacity(), original_capacity);
            assert!(rx.try_recv().is_err());
        }
    }
}

#[cfg(feature = "node-api")]
pub(crate) use external_vec_lease::ExternalVecLease;

#[cfg(all(feature = "node-api", test))]
pub(crate) use external_vec_lease::force_external_buffer_generic_failure;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialized_packet_buf_is_zeroed_and_mutable() {
        let mut buf = InitializedPacketBuf::zeroed(4);
        assert_eq!(buf.as_slice(), &[0, 0, 0, 0]);
        buf.as_mut_slice()[1] = 7;
        buf.truncate(2);
        assert_eq!(buf.into_vec(), vec![0, 7]);
    }

    #[test]
    fn provided_buffer_id_validates_range() {
        assert_eq!(
            ProvidedBufferId::new(0, 4).map(ProvidedBufferId::get),
            Some(0)
        );
        assert_eq!(
            ProvidedBufferId::new(3, 4).map(ProvidedBufferId::get),
            Some(3)
        );
        assert!(ProvidedBufferId::new(4, 4).is_none());
    }

    #[test]
    fn quiche_recv_buf_exposes_only_appended_bytes() {
        let mut buf = QuicheRecvBuf::with_capacity(8);
        let written = buf.append_initialized(b"abc");

        assert_eq!(written, 3);
        assert_eq!(buf.initialized_len(), 3);
        assert_eq!(buf.remaining(), 5);
        assert_eq!(buf.into_initialized_vec(), b"abc");
    }

    #[test]
    fn quiche_recv_buf_appends_multiple_initialized_writes() {
        let mut buf = QuicheRecvBuf::with_capacity(5);
        assert_eq!(buf.append_initialized(b"he"), 2);
        assert_eq!(buf.append_initialized(b"llo"), 3);
        assert!(buf.is_full());
        assert_eq!(buf.into_initialized_vec(), b"hello");
    }

    #[test]
    fn quiche_recv_buf_recycle_vec_hides_prior_length() {
        let mut buf = QuicheRecvBuf::with_capacity(4);
        assert_eq!(buf.append_initialized(b"data"), 4);

        let recycled = buf.into_recycle_vec();
        assert_eq!(recycled.len(), 0);
        assert!(recycled.capacity() >= 4);
    }

    #[test]
    fn quiche_recv_buf_caps_test_append_at_target_len() {
        let mut buf = QuicheRecvBuf::with_capacity(4);
        assert_eq!(buf.append_initialized(b"hello"), 4);
        assert!(buf.is_full());
        assert_eq!(buf.into_initialized_vec(), b"hell");
    }
}
