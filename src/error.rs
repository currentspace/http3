use napi::Status;

#[derive(Debug)]
pub enum Http3NativeError {
    Quiche(quiche::Error),
    H3(quiche::h3::Error),
    Io(std::io::Error),
    InvalidState(String),
    Config(String),
    ConnectionNotFound(u32),
}

impl std::fmt::Display for Http3NativeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Quiche(e) => write!(f, "QUIC error: {e}"),
            Self::H3(e) => write!(f, "HTTP/3 error: {e}"),
            Self::Io(e) => write!(f, "IO error: {e}"),
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

impl From<Http3NativeError> for napi::Error {
    fn from(err: Http3NativeError) -> napi::Error {
        let status = match &err {
            Http3NativeError::Quiche(_) | Http3NativeError::H3(_) | Http3NativeError::Io(_) => {
                Status::GenericFailure
            }
            Http3NativeError::InvalidState(_)
            | Http3NativeError::Config(_)
            | Http3NativeError::ConnectionNotFound(_) => Status::InvalidArg,
        };
        napi::Error::new(status, err.to_string())
    }
}
