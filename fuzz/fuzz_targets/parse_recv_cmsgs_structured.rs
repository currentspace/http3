#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;

#[derive(Arbitrary, Debug)]
struct CmsgProgram {
    messages: Vec<Cmsg>,
    trailing: Vec<u8>,
}

#[derive(Arbitrary, Debug)]
enum Cmsg {
    Ipv4Tos {
        tos: u8,
    },
    Ipv6Tclass {
        tclass: i32,
    },
    Ipv6Pktinfo {
        addr: [u8; 16],
        ifindex: u32,
    },
    #[cfg(target_os = "linux")]
    Ipv4Pktinfo {
        spec_dst: [u8; 4],
    },
    #[cfg(target_os = "linux")]
    UdpGro {
        segment_size: u16,
    },
    Unknown {
        level: i32,
        ty: i32,
        payload: Vec<u8>,
    },
    Truncated {
        bytes: Vec<u8>,
    },
    HugeLen {
        level: i32,
        ty: i32,
    },
}

fuzz_target!(|program: CmsgProgram| {
    let mut control = Vec::new();
    for message in program.messages.into_iter().take(16) {
        match message {
            Cmsg::Ipv4Tos { tos } => {
                push_cmsg(&mut control, libc::IPPROTO_IP, libc::IP_TOS, &[tos], None);
            }
            Cmsg::Ipv6Tclass { tclass } => {
                push_cmsg(
                    &mut control,
                    libc::IPPROTO_IPV6,
                    libc::IPV6_TCLASS,
                    &tclass.to_ne_bytes(),
                    None,
                );
            }
            Cmsg::Ipv6Pktinfo { addr, ifindex } => {
                let mut payload = Vec::with_capacity(20);
                payload.extend_from_slice(&addr);
                payload.extend_from_slice(&ifindex.to_ne_bytes());
                push_cmsg(
                    &mut control,
                    libc::IPPROTO_IPV6,
                    libc::IPV6_PKTINFO,
                    &payload,
                    None,
                );
            }
            #[cfg(target_os = "linux")]
            Cmsg::Ipv4Pktinfo { spec_dst } => {
                let mut payload = Vec::with_capacity(12);
                payload.extend_from_slice(&0i32.to_ne_bytes());
                payload.extend_from_slice(&spec_dst);
                payload.extend_from_slice(&[0u8; 4]);
                push_cmsg(
                    &mut control,
                    libc::IPPROTO_IP,
                    libc::IP_PKTINFO,
                    &payload,
                    None,
                );
            }
            #[cfg(target_os = "linux")]
            Cmsg::UdpGro { segment_size } => {
                push_cmsg(&mut control, 17, 104, &segment_size.to_ne_bytes(), None);
            }
            Cmsg::Unknown {
                level,
                ty,
                mut payload,
            } => {
                payload.truncate(64);
                push_cmsg(&mut control, level, ty, &payload, None);
            }
            Cmsg::Truncated { mut bytes } => {
                bytes.truncate(std::mem::size_of::<libc::cmsghdr>() - 1);
                control.extend_from_slice(&bytes);
            }
            Cmsg::HugeLen { level, ty } => {
                push_cmsg(&mut control, level, ty, &[], Some(usize::MAX));
            }
        }
    }

    control.extend(program.trailing.into_iter().take(32));
    http3::fuzz_exports::parse_recv_cmsgs(&control);
});

fn cmsg_align(len: usize) -> Option<usize> {
    #[cfg(target_os = "linux")]
    let align = std::mem::size_of::<usize>();
    #[cfg(target_os = "macos")]
    let align = std::mem::size_of::<u32>();
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let align = 1usize;

    len.checked_add(align - 1).map(|len| len & !(align - 1))
}

fn cmsg_data_offset() -> usize {
    cmsg_align(std::mem::size_of::<libc::cmsghdr>()).unwrap_or(std::mem::size_of::<libc::cmsghdr>())
}

fn push_cmsg(
    control: &mut Vec<u8>,
    level: libc::c_int,
    ty: libc::c_int,
    payload: &[u8],
    cmsg_len_override: Option<usize>,
) {
    let start = control.len();
    let data_off = cmsg_data_offset();
    let cmsg_len = cmsg_len_override.unwrap_or(data_off + payload.len());
    let storage_len = cmsg_align(data_off + payload.len())
        .unwrap_or(data_off + payload.len())
        .min(data_off + payload.len() + 16);

    control.resize(start + storage_len, 0);
    let hdr = libc::cmsghdr {
        cmsg_len: cmsg_len as _,
        cmsg_level: level,
        cmsg_type: ty,
    };

    #[allow(unsafe_code)]
    unsafe {
        std::ptr::write_unaligned(control[start..].as_mut_ptr().cast(), hdr);
    }

    let payload_start = start + data_off;
    let payload_end = payload_start + payload.len();
    if payload_end <= control.len() {
        control[payload_start..payload_end].copy_from_slice(payload);
    }
}
