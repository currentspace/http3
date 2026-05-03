//! Safe wrappers around Darwin's private `sendmsg_x` / `recvmsg_x` syscalls.
//!
//! The ABI is intentionally kept private to this module. Callers get message
//! counts and owned initialized buffers; raw pointers never escape.

#![allow(unsafe_code)]

use std::io;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::os::fd::{AsRawFd, RawFd};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::buffer_pool::AdaptiveBufferPool;
use crate::transport::{TxDatagram, socket};

pub(crate) const MSG_X_BATCH_SIZE: usize = 64;

const SYS_RECVMSG_X: libc::c_int = 480;
const SYS_SENDMSG_X: libc::c_int = 481;
const PROBE_BYTE: u8 = 0x5a;

#[repr(C)]
#[derive(Clone, Copy)]
struct MsgHdrX {
    msg_name: *mut libc::c_void,
    msg_namelen: libc::socklen_t,
    msg_iov: *mut libc::iovec,
    msg_iovlen: libc::c_int,
    msg_control: *mut libc::c_void,
    msg_controllen: libc::socklen_t,
    msg_flags: libc::c_int,
    msg_datalen: usize,
}

impl MsgHdrX {
    fn zeroed() -> Self {
        // SAFETY: `msghdr_x` is a plain C record; all-zero is its required
        // initialization state before individual pointer/length fields are set.
        unsafe { std::mem::zeroed() }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MsgXConfig {
    Off,
    Auto,
    On,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MsgXSelection {
    pub send_enabled: bool,
    pub recv_enabled: bool,
    pub probe_attempted: bool,
    pub probe_ok: bool,
}

pub(crate) struct RecvMsgXDatagram {
    pub data: Vec<u8>,
    pub peer: SocketAddr,
    pub local_ip: Option<IpAddr>,
    pub tos: Option<u8>,
    pub reused: bool,
    pub flags: libc::c_int,
}

pub(crate) fn selection_from_env() -> MsgXSelection {
    let default = config_var("HTTP3_MACOS_MSG_X");
    let send_cfg =
        config_var("HTTP3_MACOS_SENDMSG_X").unwrap_or(default.unwrap_or(MsgXConfig::Auto));
    let recv_cfg =
        config_var("HTTP3_MACOS_RECVMSG_X").unwrap_or(default.unwrap_or(MsgXConfig::Auto));

    let wants_send = send_cfg != MsgXConfig::Off;
    let wants_recv = recv_cfg != MsgXConfig::Off;
    if !wants_send && !wants_recv {
        return MsgXSelection {
            send_enabled: false,
            recv_enabled: false,
            probe_attempted: false,
            probe_ok: false,
        };
    }

    let probe_ok = probe_msg_x();
    MsgXSelection {
        send_enabled: wants_send && probe_ok,
        recv_enabled: wants_recv && probe_ok,
        probe_attempted: true,
        probe_ok,
    }
}

fn config_var(name: &str) -> Option<MsgXConfig> {
    let value = std::env::var(name).ok()?;
    match value.trim().to_ascii_lowercase().as_str() {
        "0" | "false" | "off" | "disabled" | "disable" | "no" => Some(MsgXConfig::Off),
        "1" | "true" | "on" | "enabled" | "enable" | "yes" => Some(MsgXConfig::On),
        "auto" | "" => Some(MsgXConfig::Auto),
        other => {
            log::warn!("ignoring invalid {name}={other:?}; expected 0, 1, or auto");
            Some(MsgXConfig::Auto)
        }
    }
}

pub(crate) fn probe_msg_x() -> bool {
    static PROBE: OnceLock<bool> = OnceLock::new();
    *PROBE.get_or_init(|| match probe_msg_x_inner() {
        Ok(()) => true,
        Err(error) => {
            log::debug!("Darwin msg_x probe failed: {error}");
            false
        }
    })
}

fn probe_msg_x_inner() -> io::Result<()> {
    let receiver = UdpSocket::bind("127.0.0.1:0")?;
    receiver.set_nonblocking(true)?;
    let sender = UdpSocket::bind("127.0.0.1:0")?;
    sender.set_nonblocking(true)?;

    let packet = TxDatagram::from_payload(vec![PROBE_BYTE], receiver.local_addr()?, None);
    let sent = send_batch(sender.as_raw_fd(), std::slice::from_ref(&packet))?;
    if sent != 1 {
        return Err(io::Error::other(format!(
            "sendmsg_x probe sent {sent} messages"
        )));
    }

    let mut pool = AdaptiveBufferPool::new(1, 64);
    let deadline = Instant::now() + Duration::from_millis(20);
    loop {
        match recv_batch(receiver.as_raw_fd(), &mut pool, 1, 64) {
            Ok(datagrams) => {
                if datagrams.len() == 1 && datagrams[0].data.as_slice() == [PROBE_BYTE] {
                    return Ok(());
                }
                return Err(io::Error::other(
                    "recvmsg_x probe received unexpected payload",
                ));
            }
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(error) => return Err(error),
        }
    }
}

pub(crate) fn send_batch(fd: RawFd, packets: &[TxDatagram]) -> io::Result<usize> {
    let count = packets.len().min(MSG_X_BATCH_SIZE);
    if count == 0 {
        return Ok(0);
    }

    let mut names = Vec::with_capacity(count);
    for packet in &packets[..count] {
        names.push(socketaddr_to_storage(packet.to));
    }

    let mut iovs = Vec::with_capacity(count);
    for packet in &packets[..count] {
        iovs.push(libc::iovec {
            iov_base: packet.payload().as_ptr().cast::<libc::c_void>().cast_mut(),
            iov_len: packet.payload().len(),
        });
    }

    let mut hdrs = vec![MsgHdrX::zeroed(); count];
    for i in 0..count {
        hdrs[i].msg_name = (&raw mut names[i].0).cast();
        hdrs[i].msg_namelen = names[i].1;
        hdrs[i].msg_iov = &raw mut iovs[i];
        hdrs[i].msg_iovlen = 1;
    }

    loop {
        // SAFETY: `hdrs` points to `count` initialized msghdr_x entries.
        // Each entry references storage/iovec arrays that live until the
        // syscall returns. Payload buffers are borrowed from `packets` and
        // are not mutated or moved during the call.
        let n = unsafe {
            libc::syscall(
                SYS_SENDMSG_X,
                fd,
                hdrs.as_ptr(),
                count as libc::c_uint,
                libc::MSG_DONTWAIT,
            )
        };
        if n >= 0 {
            return Ok(n as usize);
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(error);
    }
}

pub(crate) fn recv_batch(
    fd: RawFd,
    pool: &mut AdaptiveBufferPool,
    max_count: usize,
    buf_size: usize,
) -> io::Result<Vec<RecvMsgXDatagram>> {
    let count = max_count.min(MSG_X_BATCH_SIZE);
    if count == 0 {
        return Ok(Vec::new());
    }

    let mut buffers = Vec::with_capacity(count);
    for _ in 0..count {
        buffers.push(pool.checkout_empty_for_os_recv(buf_size));
    }

    let mut names = vec![zeroed_sockaddr_storage(); count];
    let mut controls = vec![[0u8; socket::CMSG_CONTROL_LEN]; count];
    let mut iovs = Vec::with_capacity(count);
    for (buf, _) in &mut buffers {
        iovs.push(libc::iovec {
            iov_base: buf.as_mut_ptr().cast(),
            iov_len: buf.capacity().min(buf_size),
        });
    }

    let mut hdrs = vec![MsgHdrX::zeroed(); count];
    for i in 0..count {
        hdrs[i].msg_name = (&raw mut names[i]).cast();
        hdrs[i].msg_namelen = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        hdrs[i].msg_iov = &raw mut iovs[i];
        hdrs[i].msg_iovlen = 1;
        hdrs[i].msg_control = controls[i].as_mut_ptr().cast();
        hdrs[i].msg_controllen = controls[i].len() as libc::socklen_t;
    }

    let received = loop {
        // SAFETY: `hdrs` points to initialized msghdr_x entries. Each entry
        // references name/control/iovec storage that lives until the syscall
        // returns. Iovec bases point to vector spare capacity; we set vector
        // lengths only after the kernel reports per-message byte counts.
        let n = unsafe {
            libc::syscall(
                SYS_RECVMSG_X,
                fd,
                hdrs.as_mut_ptr(),
                count as libc::c_uint,
                libc::MSG_DONTWAIT,
            )
        };
        if n >= 0 {
            break n as usize;
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        for (buf, _) in buffers {
            let _ = pool.checkin(buf);
        }
        return Err(error);
    };

    let mut out = Vec::with_capacity(received);
    for i in 0..received {
        let (mut data, reused) = std::mem::replace(&mut buffers[i], (Vec::new(), false));
        let len = hdrs[i].msg_datalen;
        if len > data.capacity() {
            for (buf, _) in buffers.into_iter().skip(i) {
                if !buf.is_empty() || buf.capacity() > 0 {
                    let _ = pool.checkin(buf);
                }
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "recvmsg_x reported more bytes than buffer capacity",
            ));
        }
        // SAFETY: recvmsg_x reported that it initialized exactly `len` bytes
        // in this slot's iovec. The check above proves `len <= capacity`.
        unsafe {
            data.set_len(len);
        }
        let peer =
            socket::sockaddr_to_socketaddr(&names[i], hdrs[i].msg_namelen).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "recvmsg_x returned unrecognised peer address",
                )
            })?;

        let control_len = (hdrs[i].msg_controllen as usize).min(controls[i].len());
        let parsed = socket::parse_recv_cmsgs(&controls[i][..control_len]);
        out.push(RecvMsgXDatagram {
            data,
            peer,
            local_ip: parsed.local_ip,
            tos: parsed.tos,
            reused,
            flags: hdrs[i].msg_flags,
        });
    }

    for (buf, _) in buffers.into_iter().skip(received) {
        if !buf.is_empty() || buf.capacity() > 0 {
            let _ = pool.checkin(buf);
        }
    }

    Ok(out)
}

fn socketaddr_to_storage(addr: SocketAddr) -> (libc::sockaddr_storage, libc::socklen_t) {
    let mut storage = zeroed_sockaddr_storage();
    match addr {
        SocketAddr::V4(addr) => {
            let sin = libc::sockaddr_in {
                sin_len: std::mem::size_of::<libc::sockaddr_in>() as u8,
                sin_family: libc::AF_INET as u8,
                sin_port: addr.port().to_be(),
                sin_addr: libc::in_addr {
                    s_addr: u32::from(*addr.ip()).to_be(),
                },
                sin_zero: [0; 8],
            };
            // SAFETY: storage is large enough and properly aligned for
            // sockaddr_in; the destination is otherwise unaliased.
            unsafe {
                std::ptr::write((&raw mut storage).cast::<libc::sockaddr_in>(), sin);
            }
            (
                storage,
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        }
        SocketAddr::V6(addr) => {
            let sin6 = libc::sockaddr_in6 {
                sin6_len: std::mem::size_of::<libc::sockaddr_in6>() as u8,
                sin6_family: libc::AF_INET6 as u8,
                sin6_port: addr.port().to_be(),
                sin6_flowinfo: addr.flowinfo(),
                sin6_addr: libc::in6_addr {
                    s6_addr: addr.ip().octets(),
                },
                sin6_scope_id: addr.scope_id(),
            };
            // SAFETY: storage is large enough and properly aligned for
            // sockaddr_in6; the destination is otherwise unaliased.
            unsafe {
                std::ptr::write((&raw mut storage).cast::<libc::sockaddr_in6>(), sin6);
            }
            (
                storage,
                std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
            )
        }
    }
}

fn zeroed_sockaddr_storage() -> libc::sockaddr_storage {
    // SAFETY: all-zero sockaddr_storage is a valid initial state.
    unsafe { std::mem::zeroed() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_parser_accepts_expected_values() {
        let _guard = crate::reactor_metrics::test_metrics_guard();
        // SAFETY: the test holds the process-wide metrics/config lock while
        // mutating environment variables.
        unsafe { std::env::set_var("HTTP3_MACOS_MSG_X_TEST", "0") };
        assert_eq!(config_var("HTTP3_MACOS_MSG_X_TEST"), Some(MsgXConfig::Off));
        // SAFETY: see above.
        unsafe { std::env::set_var("HTTP3_MACOS_MSG_X_TEST", "1") };
        assert_eq!(config_var("HTTP3_MACOS_MSG_X_TEST"), Some(MsgXConfig::On));
        // SAFETY: see above.
        unsafe { std::env::set_var("HTTP3_MACOS_MSG_X_TEST", "auto") };
        assert_eq!(config_var("HTTP3_MACOS_MSG_X_TEST"), Some(MsgXConfig::Auto));
        // SAFETY: see above.
        unsafe { std::env::remove_var("HTTP3_MACOS_MSG_X_TEST") };
    }

    #[test]
    fn socketaddr_roundtrip_ipv4() {
        let original = SocketAddr::from((std::net::Ipv4Addr::new(127, 0, 0, 1), 4433));
        let (storage, len) = socketaddr_to_storage(original);
        assert_eq!(
            socket::sockaddr_to_socketaddr(&storage, len),
            Some(original)
        );
    }

    #[test]
    fn socketaddr_roundtrip_ipv6() {
        let original = SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, 4433));
        let (storage, len) = socketaddr_to_storage(original);
        assert_eq!(
            socket::sockaddr_to_socketaddr(&storage, len),
            Some(original)
        );
    }

    #[test]
    fn probe_loopback_send_and_recv() {
        let _guard = crate::reactor_metrics::test_metrics_guard();
        assert!(probe_msg_x());
    }
}
