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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_display_quiche_error() {
        let err = Http3NativeError::Quiche(quiche::Error::Done);
        let msg = err.to_string();
        assert!(msg.starts_with("QUIC error:"), "got: {msg}");
    }

    #[test]
    fn test_display_h3_error() {
        let err = Http3NativeError::H3(quiche::h3::Error::Done);
        let msg = err.to_string();
        assert!(msg.starts_with("HTTP/3 error:"), "got: {msg}");
    }

    #[test]
    fn test_display_io_error() {
        let err = Http3NativeError::Io(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "pipe broke",
        ));
        let msg = err.to_string();
        assert!(msg.starts_with("IO error:"), "got: {msg}");
        assert!(msg.contains("pipe broke"), "got: {msg}");
    }

    #[test]
    fn test_display_fast_path_unavailable() {
        let err = Http3NativeError::FastPathUnavailable {
            driver: "io_uring",
            syscall: "sendmsg",
            errno: Some(22),
            source: std::io::Error::from_raw_os_error(22),
        };
        let msg = err.to_string();
        assert!(msg.contains("driver=io_uring"), "got: {msg}");
        assert!(msg.contains("syscall=sendmsg"), "got: {msg}");
        assert!(msg.contains("errno=22"), "got: {msg}");
    }

    #[test]
    fn test_display_runtime_io() {
        let err = Http3NativeError::RuntimeIo {
            driver: "kqueue",
            syscall: "kevent",
            errno: Some(9),
            reason_code: "bad-fd",
            source: std::io::Error::from_raw_os_error(9),
        };
        let msg = err.to_string();
        assert!(msg.contains("reason_code=bad-fd"), "got: {msg}");
    }

    #[test]
    fn test_display_invalid_state() {
        let err = Http3NativeError::InvalidState("oops".to_string());
        let msg = err.to_string();
        assert!(msg.contains("invalid state:"), "got: {msg}");
        assert!(msg.contains("oops"), "got: {msg}");
    }

    #[test]
    fn test_display_config() {
        let err = Http3NativeError::Config("bad value".to_string());
        let msg = err.to_string();
        assert!(msg.contains("config error:"), "got: {msg}");
        assert!(msg.contains("bad value"), "got: {msg}");
    }

    #[test]
    fn test_display_connection_not_found() {
        let err = Http3NativeError::ConnectionNotFound(42);
        let msg = err.to_string();
        assert!(msg.contains("connection not found:"), "got: {msg}");
        assert!(msg.contains("handle=42"), "got: {msg}");
    }

    #[test]
    fn test_from_quiche_error() {
        let err: Http3NativeError = quiche::Error::BufferTooShort.into();
        assert!(
            matches!(err, Http3NativeError::Quiche(quiche::Error::BufferTooShort)),
            "expected Quiche(BufferTooShort), got: {err:?}"
        );
    }

    #[test]
    fn test_from_h3_error() {
        let err: Http3NativeError = quiche::h3::Error::ExcessiveLoad.into();
        assert!(
            matches!(err, Http3NativeError::H3(quiche::h3::Error::ExcessiveLoad)),
            "expected H3(ExcessiveLoad), got: {err:?}"
        );
    }

    #[test]
    fn test_from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "gone");
        let err: Http3NativeError = io_err.into();
        assert!(
            matches!(err, Http3NativeError::Io(_)),
            "expected Io variant, got: {err:?}"
        );
    }

    #[test]
    fn test_fast_path_unavailable_constructor() {
        let io_err = std::io::Error::from_raw_os_error(22);
        let err = Http3NativeError::fast_path_unavailable("poll", "sendmsg", io_err);
        match err {
            Http3NativeError::FastPathUnavailable {
                driver,
                syscall,
                errno,
                ..
            } => {
                assert_eq!(driver, "poll");
                assert_eq!(syscall, "sendmsg");
                assert_eq!(errno, Some(22));
            }
            _ => panic!("expected FastPathUnavailable, got: {err:?}"),
        }
    }

    #[test]
    fn test_runtime_io_constructor() {
        let io_err = std::io::Error::from_raw_os_error(9);
        let err =
            Http3NativeError::runtime_io("kqueue", "kevent", "bad-fd", io_err);
        match err {
            Http3NativeError::RuntimeIo {
                driver,
                syscall,
                errno,
                reason_code,
                ..
            } => {
                assert_eq!(driver, "kqueue");
                assert_eq!(syscall, "kevent");
                assert_eq!(errno, Some(9));
                assert_eq!(reason_code, "bad-fd");
            }
            _ => panic!("expected RuntimeIo, got: {err:?}"),
        }
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
