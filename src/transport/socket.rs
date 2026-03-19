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

// ── Control message (cmsg) utilities ────────────────────────────────

/// Control message buffer size for IP_PKTINFO / IPV6_PKTINFO.
#[cfg(target_os = "linux")]
pub(crate) const CMSG_CONTROL_LEN: usize = 128;

/// Enable IP_PKTINFO (v4) and IPV6_RECVPKTINFO (v6) on the socket so that
/// `recvmsg` returns the per-packet local address as a cmsg.
#[cfg(target_os = "linux")]
pub(crate) fn set_pktinfo(socket: &UdpSocket) {
    use std::os::fd::AsRawFd;
    let fd = socket.as_raw_fd();
    let enable: libc::c_int = 1;
    // SAFETY: fd is a valid socket descriptor, enable points to a valid int.
    // We try both v4 and v6 — the wrong one silently fails.
    #[allow(unsafe_code)]
    unsafe {
        libc::setsockopt(
            fd,
            libc::IPPROTO_IP,
            libc::IP_PKTINFO,
            &enable as *const _ as *const libc::c_void,
            std::mem::size_of_val(&enable) as libc::socklen_t,
        );
        libc::setsockopt(
            fd,
            libc::IPPROTO_IPV6,
            libc::IPV6_RECVPKTINFO,
            &enable as *const _ as *const libc::c_void,
            std::mem::size_of_val(&enable) as libc::socklen_t,
        );
    }
}

/// Parse cmsg control data for IP_PKTINFO / IPV6_PKTINFO.
/// Returns the local IP address if found.
#[cfg(target_os = "linux")]
pub(crate) fn parse_pktinfo_cmsg(control: &[u8]) -> Option<std::net::IpAddr> {
    let mut offset = 0;
    while offset + std::mem::size_of::<libc::cmsghdr>() <= control.len() {
        // SAFETY: bounds checked above; read_unaligned handles alignment.
        let hdr: libc::cmsghdr =
            unsafe { std::ptr::read_unaligned(control.as_ptr().add(offset).cast()) };
        if hdr.cmsg_len == 0 {
            break;
        }
        let data_off = offset + cmsg_data_offset();

        if hdr.cmsg_level == libc::IPPROTO_IP && hdr.cmsg_type == libc::IP_PKTINFO {
            if data_off + std::mem::size_of::<libc::in_pktinfo>() <= control.len() {
                let info: libc::in_pktinfo =
                    unsafe { std::ptr::read_unaligned(control.as_ptr().add(data_off).cast()) };
                let ip = std::net::Ipv4Addr::from(u32::from_be(info.ipi_spec_dst.s_addr));
                return Some(std::net::IpAddr::V4(ip));
            }
        } else if hdr.cmsg_level == libc::IPPROTO_IPV6
            && hdr.cmsg_type == libc::IPV6_PKTINFO
        {
            if data_off + std::mem::size_of::<libc::in6_pktinfo>() <= control.len() {
                let info: libc::in6_pktinfo =
                    unsafe { std::ptr::read_unaligned(control.as_ptr().add(data_off).cast()) };
                let ip = std::net::Ipv6Addr::from(info.ipi6_addr.s6_addr);
                return Some(std::net::IpAddr::V6(ip));
            }
        }

        offset += cmsg_align(hdr.cmsg_len as usize);
    }
    None
}

#[cfg(target_os = "linux")]
fn cmsg_align(len: usize) -> usize {
    let align = std::mem::size_of::<usize>();
    (len + align - 1) & !(align - 1)
}

#[cfg(target_os = "linux")]
fn cmsg_data_offset() -> usize {
    cmsg_align(std::mem::size_of::<libc::cmsghdr>())
}

// ── UDP GSO (Generic Segmentation Offload) ──────────────────────────

/// `SOL_UDP` / `UDP_SEGMENT` may be missing from older libc crate versions.
#[cfg(target_os = "linux")]
pub(crate) const SOL_UDP: libc::c_int = 17;
#[cfg(target_os = "linux")]
pub(crate) const UDP_SEGMENT: libc::c_int = 103;

/// Probe whether the kernel supports UDP GSO (Generic Segmentation Offload).
/// Sets `UDP_SEGMENT` as a socket option; returns `true` if the kernel accepts it.
#[cfg(target_os = "linux")]
pub(crate) fn probe_gso(socket: &UdpSocket) -> bool {
    use std::os::fd::AsRawFd;
    let fd = socket.as_raw_fd();
    let segment: libc::c_int = 0;
    // SAFETY: fd is a valid socket, segment points to a valid int.
    #[allow(unsafe_code)]
    let rc = unsafe {
        libc::setsockopt(
            fd,
            SOL_UDP,
            UDP_SEGMENT,
            &segment as *const _ as *const libc::c_void,
            std::mem::size_of_val(&segment) as libc::socklen_t,
        )
    };
    rc == 0
}

/// Build a `UDP_SEGMENT` (GSO) cmsg into the provided buffer.
/// Returns the total byte length of the cmsg (aligned).
/// `buf` must be at least 32 bytes.
#[cfg(target_os = "linux")]
pub(crate) fn build_gso_cmsg(buf: &mut [u8], segment_size: u16) -> usize {
    debug_assert!(buf.len() >= 32);
    let cmsg_len = std::mem::size_of::<libc::cmsghdr>() + std::mem::size_of::<u16>();
    // SAFETY: buf is large enough for the cmsghdr + u16 data.
    #[allow(unsafe_code)]
    unsafe {
        let hdr = libc::cmsghdr {
            cmsg_len: cmsg_len as _,
            cmsg_level: SOL_UDP,
            cmsg_type: UDP_SEGMENT,
        };
        std::ptr::write(buf.as_mut_ptr().cast(), hdr);
        let data_ptr = buf.as_mut_ptr().add(cmsg_data_offset());
        std::ptr::write(data_ptr.cast::<u16>(), segment_size);
    }
    cmsg_align(cmsg_len)
}

#[cfg(unix)]
fn set_unix_reuse_port(socket: &socket2::Socket) -> Result<(), std::io::Error> {
    use std::os::fd::AsRawFd;

    let fd = socket.as_raw_fd();
    let enable: libc::c_int = 1;
    // SAFETY: `fd` is a valid socket descriptor and we pass a valid pointer
    // to an initialized integer option value with the correct length.
    #[allow(unsafe_code)]
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
