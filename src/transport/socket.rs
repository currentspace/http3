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

/// Minimum socket buffer size we'll accept after kernel clamping. Below this
/// QUIC will likely drop packets under bursts; we warn the operator.
const MIN_SOCKET_BUF_SIZE: usize = 2 * 1024 * 1024;

/// Try to enlarge the socket's receive and send buffers using the same 8 MB
/// → 4 MB → 2 MB tier list as `set_socket_buffers`. Logs a `warn!` if the
/// kernel clamped below the minimum acceptable size. Audit finding #21:
/// previously this function only attempted 2 MB, while
/// `set_socket_buffers` (used by client paths) tried 8 MB first — server
/// sockets ended up at 2 MB while clients got 8 MB.
fn set_socket_buffer_sizes(socket: &socket2::Socket) {
    for &size in BUFFER_SIZES {
        if socket.set_send_buffer_size(size).is_ok() && socket.set_recv_buffer_size(size).is_ok() {
            break;
        }
    }
    let effective_rcv = socket.recv_buffer_size().unwrap_or(0);
    let effective_snd = socket.send_buffer_size().unwrap_or(0);
    if effective_rcv < MIN_SOCKET_BUF_SIZE || effective_snd < MIN_SOCKET_BUF_SIZE {
        log::warn!(
            "UDP socket buffer sizes clamped by kernel: rcvbuf={}KB sndbuf={}KB (wanted at least {}KB). \
             Raise net.core.rmem_max / net.core.wmem_max for best QUIC performance.",
            effective_rcv / 1024,
            effective_snd / 1024,
            MIN_SOCKET_BUF_SIZE / 1024,
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
/// On Linux, creates a temporary connected UDP socket to `peer` and calls
/// `getsockopt(IP_MTU)`. On macOS, matches `peer` against the interface list
/// from `getifaddrs` and reads the link MTU for the best-prefix interface.
/// Returns `min(mtu - headers, 16383)`.
/// Returns `None` if the query fails, in which case the caller should fall
/// back to a conservative default.
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
        query_path_mtu_from_interfaces(peer)
    }
}

#[cfg(target_os = "macos")]
fn query_path_mtu_from_interfaces(peer: &SocketAddr) -> Option<usize> {
    use std::ffi::CStr;
    use std::net::IpAddr;

    struct IfAddrs(*mut libc::ifaddrs);

    impl Drop for IfAddrs {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: `self.0` was returned by `getifaddrs` and is freed
                // exactly once by this guard.
                unsafe {
                    libc::freeifaddrs(self.0);
                }
            }
        }
    }

    let mut addrs: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: `getifaddrs` initializes `addrs` on success; the guard releases
    // the linked list with `freeifaddrs`.
    if unsafe { libc::getifaddrs(&mut addrs) } != 0 || addrs.is_null() {
        return None;
    }
    let _guard = IfAddrs(addrs);

    let mut interface_mtu: Vec<(Vec<u8>, usize)> = Vec::new();
    let mut current = addrs;
    while !current.is_null() {
        // SAFETY: `current` walks the valid `getifaddrs` linked list until
        // the null terminator.
        let ifa = unsafe { &*current };
        if !ifa.ifa_name.is_null() && !ifa.ifa_addr.is_null() && !ifa.ifa_data.is_null() {
            // SAFETY: `ifa_addr` is a valid sockaddr for this list entry.
            let family = unsafe { (*ifa.ifa_addr).sa_family as libc::c_int };
            if family == libc::AF_LINK {
                // SAFETY: Darwin exposes link-layer MTU in `if_data` for
                // AF_LINK entries from `getifaddrs`.
                let if_data = unsafe { &*(ifa.ifa_data.cast::<libc::if_data>()) };
                let mtu = if_data.ifi_mtu as usize;
                if mtu > 0 {
                    // SAFETY: `ifa_name` is a NUL-terminated interface name
                    // owned by the live `getifaddrs` list.
                    let name = unsafe { CStr::from_ptr(ifa.ifa_name) }.to_bytes().to_vec();
                    interface_mtu.push((name, mtu));
                }
            }
        }
        current = ifa.ifa_next;
    }

    let mut best: Option<(u32, usize)> = None;
    let mut current = addrs;
    while !current.is_null() {
        // SAFETY: `current` walks the valid `getifaddrs` linked list until
        // the null terminator.
        let ifa = unsafe { &*current };
        if !ifa.ifa_name.is_null() && !ifa.ifa_addr.is_null() && !ifa.ifa_netmask.is_null() {
            // SAFETY: `ifa_addr` is a valid sockaddr for this list entry.
            let family = unsafe { (*ifa.ifa_addr).sa_family as libc::c_int };
            let candidate = match (family, peer.ip()) {
                (libc::AF_INET, IpAddr::V4(peer_ip)) => {
                    // SAFETY: family checks prove these sockaddr pointers have
                    // IPv4 layout for this entry.
                    let addr = unsafe { *(ifa.ifa_addr.cast::<libc::sockaddr_in>()) };
                    let mask = unsafe { *(ifa.ifa_netmask.cast::<libc::sockaddr_in>()) };
                    let addr = u32::from_be(addr.sin_addr.s_addr);
                    let mask = u32::from_be(mask.sin_addr.s_addr);
                    let peer = u32::from(peer_ip);
                    ((peer & mask) == (addr & mask)).then_some(mask.count_ones())
                }
                (libc::AF_INET6, IpAddr::V6(peer_ip)) => {
                    // SAFETY: family checks prove these sockaddr pointers have
                    // IPv6 layout for this entry.
                    let addr = unsafe { *(ifa.ifa_addr.cast::<libc::sockaddr_in6>()) };
                    let mask = unsafe { *(ifa.ifa_netmask.cast::<libc::sockaddr_in6>()) };
                    let peer = peer_ip.octets();
                    let addr = addr.sin6_addr.s6_addr;
                    let mask = mask.sin6_addr.s6_addr;
                    let matches = peer
                        .iter()
                        .zip(addr.iter())
                        .zip(mask.iter())
                        .all(|((peer, addr), mask)| (peer & mask) == (addr & mask));
                    matches.then_some(mask.iter().map(|byte| byte.count_ones()).sum())
                }
                _ => None,
            };

            if let Some(prefix_len) = candidate {
                // SAFETY: `ifa_name` is a NUL-terminated interface name owned
                // by the live `getifaddrs` list.
                let name = unsafe { CStr::from_ptr(ifa.ifa_name) }.to_bytes();
                if let Some((_, mtu)) = interface_mtu.iter().find(|(mtu_name, _)| mtu_name == name)
                {
                    if best.is_none_or(|(best_prefix, _)| prefix_len > best_prefix) {
                        best = Some((prefix_len, *mtu));
                    }
                }
            }
        }
        current = ifa.ifa_next;
    }

    let mtu = best?.1;
    let header_overhead = if peer.is_ipv4() { 28 } else { 48 };
    Some(
        mtu.saturating_sub(header_overhead)
            .min(QUIC_MAX_PACKET_SIZE),
    )
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn query_path_mtu_from_interfaces(_peer: &SocketAddr) -> Option<usize> {
    None
}

// ── Control message (cmsg) utilities ────────────────────────────────

/// Control message buffer size for IP_PKTINFO / IPV6_PKTINFO (Linux) or
/// IP_RECVDSTADDR / IPV6_PKTINFO (Darwin).
#[cfg(any(target_os = "linux", target_os = "macos"))]
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

// ── macOS / Darwin variants ─────────────────────────────────────────

/// Enable `IP_RECVDSTADDR` (v4) and `IPV6_RECVPKTINFO` (v6) on the socket so
/// that `recvmsg` returns the per-packet *destination* address as a cmsg.
/// Audit finding #20: needed so a server bound to `0.0.0.0` can route by
/// the actual local IP each datagram arrived on, not the bind address.
#[cfg(target_os = "macos")]
pub(crate) fn set_pktinfo(socket: &UdpSocket) {
    use std::os::fd::AsRawFd;
    let fd = socket.as_raw_fd();
    let enable: libc::c_int = 1;
    // SAFETY: fd is a valid socket descriptor, enable points to a valid int.
    // The wrong family silently fails (per setsockopt semantics).
    #[allow(unsafe_code)]
    unsafe {
        libc::setsockopt(
            fd,
            libc::IPPROTO_IP,
            libc::IP_RECVDSTADDR,
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

// ── ECN (audit #18) ─────────────────────────────────────────────────
//
// quiche 0.28 has no `ecn` field on RecvInfo and explicitly states
// "sending ECN is not supported at this time" (lib.rs:4501), so we
// can't feed observed ECN into the QUIC congestion controller. But
// ECN bits live in the IP header, not the QUIC payload — the socket
// layer can still observe them. Surface CE / ECT(0) / ECT(1) /
// Not-ECT counts as telemetry so operators can see whether their
// path is congested without waiting on a quiche bump.

/// ECN code points (RFC 3168, lower 2 bits of the IP TOS byte).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EcnCodePoint {
    NotEct = 0b00,
    Ect1 = 0b01,
    Ect0 = 0b10,
    Ce = 0b11,
}

impl EcnCodePoint {
    pub(crate) fn from_tos(tos: u8) -> Self {
        match tos & 0b11 {
            0b00 => Self::NotEct,
            0b01 => Self::Ect1,
            0b10 => Self::Ect0,
            _ => Self::Ce,
        }
    }
}

/// Enable `IP_RECVTOS` (v4) and `IPV6_RECVTCLASS` (v6) on the socket so
/// `recvmsg` returns the per-packet TOS byte (which holds the ECN bits)
/// as a cmsg. Best-effort: silently no-ops if the kernel rejects the
/// option — ECN telemetry just stays at zero in that case.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn set_recv_ecn(socket: &UdpSocket) {
    use std::os::fd::AsRawFd;
    let fd = socket.as_raw_fd();
    let enable: libc::c_int = 1;
    // SAFETY: fd is a valid socket; enable points to a valid int.
    #[allow(unsafe_code)]
    unsafe {
        libc::setsockopt(
            fd,
            libc::IPPROTO_IP,
            libc::IP_RECVTOS,
            &enable as *const _ as *const libc::c_void,
            std::mem::size_of_val(&enable) as libc::socklen_t,
        );
        libc::setsockopt(
            fd,
            libc::IPPROTO_IPV6,
            libc::IPV6_RECVTCLASS,
            &enable as *const _ as *const libc::c_void,
            std::mem::size_of_val(&enable) as libc::socklen_t,
        );
    }
}

// ── Unified cmsg / sockaddr helpers ────────────────────────────────

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn cmsg_align(len: usize) -> usize {
    // Linux's `CMSG_ALIGN` uses sizeof(size_t) = 8 on 64-bit; Darwin's
    // `__DARWIN_ALIGN32` uses sizeof(uint32_t) = 4. Mismatching the
    // alignment makes `cmsg_data_offset()` skip past the actual payload
    // and silently bounds-fail every read. (Pre-existing bug — audit #20
    // shipped because the IPv4 destination addr it extracts was never
    // observably consumed.)
    #[cfg(target_os = "linux")]
    let align = std::mem::size_of::<usize>();
    #[cfg(target_os = "macos")]
    let align = std::mem::size_of::<u32>();
    (len + align - 1) & !(align - 1)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn cmsg_data_offset() -> usize {
    cmsg_align(std::mem::size_of::<libc::cmsghdr>())
}

#[cfg(all(target_os = "linux", test))]
pub(crate) fn cmsg_data_offset_for_test() -> usize {
    cmsg_data_offset()
}

/// Everything the recv path extracts from a single cmsg control buffer.
///
/// `parse_recv_cmsgs` walks the buffer once and emits all three fields,
/// avoiding the 2–3× duplicate scans that separate per-cmsg-type parsers
/// produced on the hot path (one inbound datagram = one walk).
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ParsedRecvCmsgs {
    /// Per-datagram destination IP. From `IP_PKTINFO` / `IP_RECVDSTADDR` /
    /// `IPV6_PKTINFO` cmsgs (audit #20).
    pub local_ip: Option<std::net::IpAddr>,
    /// Inbound TOS byte. The low 2 bits hold the ECN code point
    /// (audit #18).
    pub tos: Option<u8>,
    /// UDP GRO segment size — Linux only; always `None` on macOS.
    pub segment_size: Option<u16>,
}

/// Walk the recvmsg control buffer once and extract pktinfo + TOS + GRO
/// segment-size fields together. See `ParsedRecvCmsgs`.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn parse_recv_cmsgs(control: &[u8]) -> ParsedRecvCmsgs {
    let mut out = ParsedRecvCmsgs::default();
    let mut offset = 0;
    while offset + std::mem::size_of::<libc::cmsghdr>() <= control.len() {
        // SAFETY: bounds checked above; read_unaligned handles alignment.
        #[allow(unsafe_code)]
        let hdr: libc::cmsghdr =
            unsafe { std::ptr::read_unaligned(control.as_ptr().add(offset).cast()) };
        if hdr.cmsg_len == 0 {
            break;
        }
        let data_off = offset + cmsg_data_offset();

        // IPv6 destination address — same cmsg type on Linux and macOS.
        if hdr.cmsg_level == libc::IPPROTO_IPV6 && hdr.cmsg_type == libc::IPV6_PKTINFO {
            if data_off + std::mem::size_of::<libc::in6_pktinfo>() <= control.len() {
                #[allow(unsafe_code)]
                let info: libc::in6_pktinfo =
                    unsafe { std::ptr::read_unaligned(control.as_ptr().add(data_off).cast()) };
                let ip = std::net::Ipv6Addr::from(info.ipi6_addr.s6_addr);
                out.local_ip = Some(std::net::IpAddr::V6(ip));
            }
        } else if hdr.cmsg_level == libc::IPPROTO_IPV6 && hdr.cmsg_type == libc::IPV6_TCLASS {
            // IPV6_TCLASS payload is an int (4 bytes); only the low byte
            // holds the traffic class.
            if data_off + std::mem::size_of::<libc::c_int>() <= control.len() {
                #[allow(unsafe_code)]
                let tclass: libc::c_int =
                    unsafe { std::ptr::read_unaligned(control.as_ptr().add(data_off).cast()) };
                out.tos = Some((tclass & 0xff) as u8);
            }
        } else if hdr.cmsg_level == libc::IPPROTO_IP
            && (hdr.cmsg_type == libc::IP_TOS
                // Darwin delivers the TOS byte under cmsg_type = IP_RECVTOS
                // (the same option used to enable the cmsg) rather than
                // IP_TOS the way Linux does. Match either to stay portable.
                || cfg!(target_os = "macos") && hdr.cmsg_type == libc::IP_RECVTOS)
        {
            // Payload is a single byte, padded to int alignment.
            if data_off < control.len() {
                out.tos = Some(control[data_off]);
            }
        } else {
            #[cfg(target_os = "linux")]
            {
                if hdr.cmsg_level == libc::IPPROTO_IP && hdr.cmsg_type == libc::IP_PKTINFO {
                    if data_off + std::mem::size_of::<libc::in_pktinfo>() <= control.len() {
                        #[allow(unsafe_code)]
                        let info: libc::in_pktinfo = unsafe {
                            std::ptr::read_unaligned(control.as_ptr().add(data_off).cast())
                        };
                        let ip = std::net::Ipv4Addr::from(u32::from_be(info.ipi_spec_dst.s_addr));
                        out.local_ip = Some(std::net::IpAddr::V4(ip));
                    }
                } else if hdr.cmsg_level == SOL_UDP && hdr.cmsg_type == UDP_GRO {
                    if data_off + std::mem::size_of::<u16>() <= control.len() {
                        #[allow(unsafe_code)]
                        let seg: u16 = unsafe {
                            std::ptr::read_unaligned(control.as_ptr().add(data_off).cast())
                        };
                        out.segment_size = Some(seg);
                    }
                }
            }
            #[cfg(target_os = "macos")]
            {
                if hdr.cmsg_level == libc::IPPROTO_IP
                    && hdr.cmsg_type == libc::IP_RECVDSTADDR
                    && data_off + std::mem::size_of::<libc::in_addr>() <= control.len()
                {
                    // IP_RECVDSTADDR delivers a bare `struct in_addr` (4 bytes).
                    #[allow(unsafe_code)]
                    let addr: libc::in_addr =
                        unsafe { std::ptr::read_unaligned(control.as_ptr().add(data_off).cast()) };
                    let ip = std::net::Ipv4Addr::from(u32::from_be(addr.s_addr));
                    out.local_ip = Some(std::net::IpAddr::V4(ip));
                }
            }
        }

        offset += cmsg_align(hdr.cmsg_len as usize);
    }
    out
}

/// Convert a `sockaddr_storage` (filled by `recvmsg` / `recvmmsg`) into a
/// `SocketAddr`. Returns `None` for unrecognised address families.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn sockaddr_to_socketaddr(
    addr: &libc::sockaddr_storage,
    len: libc::socklen_t,
) -> Option<std::net::SocketAddr> {
    if len as usize >= std::mem::size_of::<libc::sockaddr_in>()
        && i32::from(addr.ss_family) == libc::AF_INET
    {
        // SAFETY: ss_family is AF_INET and len covers sockaddr_in.
        #[allow(unsafe_code)]
        let sin: &libc::sockaddr_in =
            unsafe { &*std::ptr::from_ref(addr).cast::<libc::sockaddr_in>() };
        let ip = std::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
        let port = u16::from_be(sin.sin_port);
        Some(std::net::SocketAddr::from((ip, port)))
    } else if len as usize >= std::mem::size_of::<libc::sockaddr_in6>()
        && i32::from(addr.ss_family) == libc::AF_INET6
    {
        // SAFETY: ss_family is AF_INET6 and len covers sockaddr_in6.
        #[allow(unsafe_code)]
        let sin6: &libc::sockaddr_in6 =
            unsafe { &*std::ptr::from_ref(addr).cast::<libc::sockaddr_in6>() };
        let ip = std::net::Ipv6Addr::from(sin6.sin6_addr.s6_addr);
        let port = u16::from_be(sin6.sin6_port);
        Some(std::net::SocketAddr::from((ip, port)))
    } else {
        None
    }
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

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;

    #[test]
    fn ecn_code_point_decodes_lower_two_tos_bits() {
        // RFC 3168 §5: ECN bits live in the low 2 bits of TOS / Traffic
        // Class. Upper 6 bits are DSCP and must be ignored.
        assert_eq!(EcnCodePoint::from_tos(0b0000_0000), EcnCodePoint::NotEct);
        assert_eq!(EcnCodePoint::from_tos(0b0000_0001), EcnCodePoint::Ect1);
        assert_eq!(EcnCodePoint::from_tos(0b0000_0010), EcnCodePoint::Ect0);
        assert_eq!(EcnCodePoint::from_tos(0b0000_0011), EcnCodePoint::Ce);
        // DSCP bits (high 6) must not affect classification.
        assert_eq!(EcnCodePoint::from_tos(0b1110_1011), EcnCodePoint::Ce);
        assert_eq!(EcnCodePoint::from_tos(0b1110_1010), EcnCodePoint::Ect0);
    }

    #[test]
    fn query_path_mtu_finds_loopback_ceiling() {
        let peer = SocketAddr::from(([127, 0, 0, 1], 4433));
        let ceiling = query_path_mtu(&peer).expect("loopback path MTU should be discoverable");
        assert!(ceiling >= crate::config::FALLBACK_MAX_UDP_PAYLOAD);
        assert!(ceiling <= QUIC_MAX_PACKET_SIZE);
    }
}
