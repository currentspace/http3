//! Crate error types with conversions to `napi::Error` so Rust failures
//! surface as proper JS exceptions.

#[cfg(feature = "node-api")]
use napi::Status;

#[derive(Debug)]
pub enum Http3NativeError {
    Quiche(quiche::Error),
    H3(quiche::h3::Error),
    Io(std::io::Error),
    FastPathUnavailable {
        driver: &'static str,
        syscall: &'static str,
        errno: Option<i32>,
        source: std::io::Error,
    },
    RuntimeIo {
        driver: &'static str,
        syscall: &'static str,
        errno: Option<i32>,
        reason_code: &'static str,
        source: std::io::Error,
    },
    InvalidState(String),
    Config(String),
    ConnectionNotFound(u32),
}

impl Http3NativeError {
    pub fn fast_path_unavailable(
        driver: &'static str,
        syscall: &'static str,
        source: std::io::Error,
    ) -> Self {
        let errno = source.raw_os_error();
        Self::FastPathUnavailable {
            driver,
            syscall,
            errno,
            source,
        }
    }

    pub fn runtime_io(
        driver: &'static str,
        syscall: &'static str,
        reason_code: &'static str,
        source: std::io::Error,
    ) -> Self {
        let errno = source.raw_os_error();
        Self::RuntimeIo {
            driver,
            syscall,
            errno,
            reason_code,
            source,
        }
    }
}

impl std::fmt::Display for Http3NativeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Quiche(e) => write!(f, "QUIC error: {e}"),
            Self::H3(e) => write!(f, "HTTP/3 error: {e}"),
            Self::Io(e) => write!(f, "IO error: {e}"),
            Self::FastPathUnavailable {
                driver,
                syscall,
                errno,
                source,
            } => write!(
                f,
                "ERR_HTTP3_FAST_PATH_UNAVAILABLE driver={driver} syscall={syscall} errno={} reason_code=fast-path-unavailable: {source}",
                errno.map_or_else(|| "unknown".into(), |value| value.to_string())
            ),
            Self::RuntimeIo {
                driver,
                syscall,
                errno,
                reason_code,
                source,
            } => write!(
                f,
                "ERR_HTTP3_RUNTIME_UNSUPPORTED driver={driver} syscall={syscall} errno={} reason_code={reason_code}: {source}",
                errno.map_or_else(|| "unknown".into(), |value| value.to_string())
            ),
            Self::InvalidState(s) => write!(f, "invalid state: {s}"),
            Self::Config(s) => write!(f, "config error: {s}"),
            Self::ConnectionNotFound(h) => write!(f, "connection not found: handle={h}"),
        }
    }
}

impl std::error::Error for Http3NativeError {}

impl From<quiche::Error> for Http3NativeError {
    fn from(e: quiche::Error) -> Self {
        Self::Quiche(e)
    }
}

impl From<quiche::h3::Error> for Http3NativeError {
    fn from(e: quiche::h3::Error) -> Self {
        Self::H3(e)
    }
}

impl From<std::io::Error> for Http3NativeError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

#[cfg(feature = "node-api")]
impl From<Http3NativeError> for napi::Error {
    fn from(err: Http3NativeError) -> napi::Error {
        let status = match &err {
            Http3NativeError::Quiche(_)
            | Http3NativeError::H3(_)
            | Http3NativeError::Io(_)
            | Http3NativeError::FastPathUnavailable { .. }
            | Http3NativeError::RuntimeIo { .. } => Status::GenericFailure,
            Http3NativeError::InvalidState(_)
            | Http3NativeError::Config(_)
            | Http3NativeError::ConnectionNotFound(_) => Status::InvalidArg,
        };
        napi::Error::new(status, err.to_string())
    }
}
