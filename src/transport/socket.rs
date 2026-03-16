//! Socket utilities: binding, buffer sizing, SO_REUSEPORT.
//!
//! Extracted from worker.rs to be shared by all spawn functions.

use std::net::{SocketAddr, UdpSocket};

use crate::error::Http3NativeError;

/// Preferred buffer sizes, tried in order until one succeeds.
/// macOS caps at kern.ipc.maxsockbuf (typically 8MB).
const BUFFER_SIZES: &[usize] = &[
    8 * 1024 * 1024, // 8 MB — ideal for fan-out (30+ connections)
    4 * 1024 * 1024, // 4 MB
    2 * 1024 * 1024, // 2 MB — minimum acceptable
];

/// Set OS-level send and receive buffer sizes on a UDP socket.
/// Tries progressively smaller sizes until the OS accepts one.
/// The `hint` parameter is used as a final fallback if none of the
/// preferred sizes are accepted by the kernel.
pub(crate) fn set_socket_buffers(socket: &UdpSocket, hint: usize) -> Result<(), std::io::Error> {
    let sock_ref = socket2::SockRef::from(socket);
    for &size in BUFFER_SIZES {
        if sock_ref.set_send_buffer_size(size).is_ok()
            && sock_ref.set_recv_buffer_size(size).is_ok()
        {
            return Ok(());
        }
    }
    // Fallback: try the caller's hint
    sock_ref.set_send_buffer_size(hint)?;
    sock_ref.set_recv_buffer_size(hint)?;
    Ok(())
}

/// Bind a UDP socket, optionally with `SO_REUSEPORT`.
pub(crate) fn bind_worker_socket(
    bind_addr: SocketAddr,
    reuse_port: bool,
) -> Result<UdpSocket, Http3NativeError> {
    if !reuse_port {
        return UdpSocket::bind(bind_addr).map_err(Http3NativeError::Io);
    }

    use socket2::{Domain, Protocol, Socket, Type};

    let domain = if bind_addr.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };
    let socket =
        Socket::new(domain, Type::DGRAM, Some(Protocol::UDP)).map_err(Http3NativeError::Io)?;
    socket
        .set_reuse_address(true)
        .map_err(Http3NativeError::Io)?;
    #[cfg(unix)]
    set_unix_reuse_port(&socket).map_err(Http3NativeError::Io)?;
    socket
        .bind(&bind_addr.into())
        .map_err(Http3NativeError::Io)?;
    Ok(socket.into())
}

#[cfg(unix)]
fn set_unix_reuse_port(socket: &socket2::Socket) -> Result<(), std::io::Error> {
    use std::os::fd::AsRawFd;

    let fd = socket.as_raw_fd();
    let enable: libc::c_int = 1;
    // SAFETY: `fd` is a valid socket descriptor and we pass a valid pointer
    // to an initialized integer option value with the correct length.
    let rc = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_REUSEPORT,
            &enable as *const _ as *const libc::c_void,
            std::mem::size_of_val(&enable) as libc::socklen_t,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}
