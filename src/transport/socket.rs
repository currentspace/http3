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
/// Target UDP socket buffer size (2 MB). QUIC implementations typically need
/// large buffers to absorb packet bursts without kernel drops.  The kernel may
/// cap this at `net.core.rmem_max` / `net.core.wmem_max`, but `setsockopt`
/// silently clamps — it never fails.
const SOCKET_BUF_SIZE: usize = 2 * 1024 * 1024;

/// Try to enlarge the socket's receive and send buffers.  Best-effort: if the
/// kernel clamps the value we still proceed with whatever size we got.
/// Logs the effective sizes so operators can diagnose buffer-related drops.
fn set_socket_buffer_sizes(socket: &socket2::Socket) {
    let _ = socket.set_recv_buffer_size(SOCKET_BUF_SIZE);
    let _ = socket.set_send_buffer_size(SOCKET_BUF_SIZE);
    let effective_rcv = socket.recv_buffer_size().unwrap_or(0);
    let effective_snd = socket.send_buffer_size().unwrap_or(0);
    if effective_rcv < SOCKET_BUF_SIZE || effective_snd < SOCKET_BUF_SIZE {
        log::warn!(
            "UDP socket buffer sizes clamped by kernel: rcvbuf={}KB (wanted {}KB) sndbuf={}KB (wanted {}KB). \
             Raise net.core.rmem_max / net.core.wmem_max to at least {} for best QUIC performance.",
            effective_rcv / 1024,
            SOCKET_BUF_SIZE / 1024,
            effective_snd / 1024,
            SOCKET_BUF_SIZE / 1024,
            SOCKET_BUF_SIZE,
        );
    }
}

pub(crate) fn bind_worker_socket(
    bind_addr: SocketAddr,
    reuse_port: bool,
) -> Result<UdpSocket, Http3NativeError> {
    use socket2::{Domain, Protocol, Socket, Type};

    let domain = if bind_addr.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };
    let socket =
        Socket::new(domain, Type::DGRAM, Some(Protocol::UDP)).map_err(Http3NativeError::Io)?;

    set_socket_buffer_sizes(&socket);

    if reuse_port {
        socket
            .set_reuse_address(true)
            .map_err(Http3NativeError::Io)?;
        #[cfg(unix)]
        set_unix_reuse_port(&socket).map_err(Http3NativeError::Io)?;
    }
    socket
        .bind(&bind_addr.into())
        .map_err(Http3NativeError::Io)?;
    Ok(socket.into())
}

// ── Path MTU query ──────────────────────────────────────────────────

/// Maximum QUIC packet size quiche will use for data (2-byte varint limit).
const QUIC_MAX_PACKET_SIZE: usize = 16383;

/// Query the link-layer MTU for the path to `peer` and return the maximum
/// useful PMTUD probe ceiling.
///
/// Creates a temporary connected UDP socket to `peer`, calls
/// `getsockopt(IP_MTU)`, and returns `min(mtu - headers, 16383)`.
/// Returns `None` if the query fails (non-Linux, permission error, etc.),
/// in which case the caller should fall back to a conservative default.
///
/// This is NOT a loopback hack — it queries the kernel routing table for
/// the actual interface MTU on the path to any destination.
pub(crate) fn query_path_mtu(peer: &SocketAddr) -> Option<usize> {
    #[cfg(target_os = "linux")]
    {
        use std::net::UdpSocket as StdUdpSocket;

        // IP + UDP header overhead
        let header_overhead: usize = if peer.is_ipv4() { 28 } else { 48 };

        let probe_socket = StdUdpSocket::bind(if peer.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        })
        .ok()?;
        probe_socket.connect(peer).ok()?;

        // IP_MTU = 14 on Linux
        let raw_fd = {
            use std::os::unix::io::AsRawFd;
            probe_socket.as_raw_fd()
        };
        let mut mtu: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        let rc = unsafe {
            libc::getsockopt(
                raw_fd,
                libc::IPPROTO_IP,
                libc::IP_MTU,
                &mut mtu as *mut _ as *mut libc::c_void,
                &mut len,
            )
        };
        if rc != 0 || mtu <= 0 {
            return None;
        }

        let max_payload = (mtu as usize).saturating_sub(header_overhead);
        Some(max_payload.min(QUIC_MAX_PACKET_SIZE))
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = peer;
        None
    }
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

#[cfg(all(target_os = "linux", test))]
pub(crate) fn cmsg_data_offset_for_test() -> usize {
    cmsg_data_offset()
}

// ── UDP GSO (Generic Segmentation Offload) ──────────────────────────

/// `SOL_UDP` / `UDP_SEGMENT` may be missing from older libc crate versions.
#[cfg(target_os = "linux")]
pub(crate) const SOL_UDP: libc::c_int = 17;
#[cfg(target_os = "linux")]
pub(crate) const UDP_SEGMENT: libc::c_int = 103;

/// Probe whether the kernel supports UDP GSO (Generic Segmentation Offload).
/// Uses `getsockopt` (read-only) instead of `setsockopt` to avoid leaving a
/// persistent `UDP_SEGMENT` socket option that could alter kernel send code
/// paths on the worker thread.
#[cfg(target_os = "linux")]
pub(crate) fn probe_gso(socket: &UdpSocket) -> bool {
    use std::os::fd::AsRawFd;
    let fd = socket.as_raw_fd();
    let mut val: libc::c_int = 0;
    let mut len = std::mem::size_of_val(&val) as libc::socklen_t;
    // SAFETY: fd is a valid socket, val/len point to valid stack memory.
    #[allow(unsafe_code)]
    let rc = unsafe {
        libc::getsockopt(
            fd,
            SOL_UDP,
            UDP_SEGMENT,
            &mut val as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    let supported = rc == 0;
    log::debug!(
        "probe_gso: fd={fd} getsockopt(SOL_UDP, UDP_SEGMENT) rc={rc} supported={supported}"
    );
    supported
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

// ── UDP GRO (Generic Receive Offload) ───────────────────────────────

#[cfg(target_os = "linux")]
pub(crate) const UDP_GRO: libc::c_int = 104;

/// Enable UDP GRO so the kernel coalesces same-source, same-size datagrams
/// into a single large buffer with a `UDP_GRO` cmsg indicating segment size.
/// Linux ≥5.0 only; silently no-ops on older kernels.
#[cfg(target_os = "linux")]
pub(crate) fn enable_gro(socket: &UdpSocket) {
    use std::os::fd::AsRawFd;
    let fd = socket.as_raw_fd();
    let enable: libc::c_int = 1;
    // SAFETY: fd is a valid socket descriptor, enable points to a valid int.
    #[allow(unsafe_code)]
    unsafe {
        libc::setsockopt(
            fd,
            SOL_UDP,
            UDP_GRO,
            &enable as *const _ as *const libc::c_void,
            std::mem::size_of_val(&enable) as libc::socklen_t,
        );
    }
}

/// Parse cmsg control data for `UDP_GRO` segment size.
/// Returns the segment size if a GRO cmsg is present.
#[cfg(target_os = "linux")]
pub(crate) fn parse_gro_cmsg(control: &[u8]) -> Option<u16> {
    let mut offset = 0;
    while offset + std::mem::size_of::<libc::cmsghdr>() <= control.len() {
        let hdr: libc::cmsghdr =
            unsafe { std::ptr::read_unaligned(control.as_ptr().add(offset).cast()) };
        if hdr.cmsg_len == 0 {
            break;
        }
        let data_off = offset + cmsg_data_offset();

        if hdr.cmsg_level == SOL_UDP && hdr.cmsg_type == UDP_GRO {
            if data_off + std::mem::size_of::<u16>() <= control.len() {
                let seg: u16 =
                    unsafe { std::ptr::read_unaligned(control.as_ptr().add(data_off).cast()) };
                return Some(seg);
            }
        }

        offset += cmsg_align(hdr.cmsg_len as usize);
    }
    None
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
