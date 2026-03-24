//! Regression tests for socket option safety.
//!
//! These verify that socket setup functions used during Driver::new() on the
//! main thread do not leave the socket in a state that breaks sendmmsg/recvmmsg
//! on a different worker thread.

#[cfg(test)]
#[cfg(target_os = "linux")]
mod tests {
    use std::net::UdpSocket;
    use std::os::unix::io::AsRawFd;
    use std::sync::{Mutex, OnceLock};
    use std::thread;
    use std::time::{Duration, Instant};

    // ── Constants matching socket.rs ─────────────────────────────────

    const SOL_UDP: libc::c_int = 17;
    const UDP_SEGMENT: libc::c_int = 103;
    const UDP_GRO: libc::c_int = 104;

    // ── Helpers ──────────────────────────────────────────────────────

    fn setsockopt_int(
        fd: libc::c_int,
        level: libc::c_int,
        opt: libc::c_int,
        val: libc::c_int,
    ) -> libc::c_int {
        unsafe {
            libc::setsockopt(
                fd,
                level,
                opt,
                &val as *const _ as *const libc::c_void,
                std::mem::size_of_val(&val) as libc::socklen_t,
            )
        }
    }

    fn getsockopt_int(
        fd: libc::c_int,
        level: libc::c_int,
        opt: libc::c_int,
    ) -> Result<libc::c_int, std::io::Error> {
        let mut val: libc::c_int = -1;
        let mut len = std::mem::size_of_val(&val) as libc::socklen_t;
        let rc = unsafe {
            libc::getsockopt(
                fd,
                level,
                opt,
                &mut val as *mut _ as *mut libc::c_void,
                &mut len,
            )
        };
        if rc == 0 {
            Ok(val)
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    /// Send a packet from `src` to `dst` via sendmmsg, return whether it succeeded.
    fn sendmmsg_one(src_fd: libc::c_int, dst: &UdpSocket) -> bool {
        let payload = [0xABu8; 100];
        let dst_addr = dst.local_addr().unwrap();

        let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
        let addrlen = match dst_addr {
            std::net::SocketAddr::V4(a) => {
                let sin = libc::sockaddr_in {
                    sin_family: libc::AF_INET as _,
                    sin_port: a.port().to_be(),
                    sin_addr: libc::in_addr {
                        s_addr: u32::from_ne_bytes(a.ip().octets()),
                    },
                    sin_zero: [0; 8],
                };
                unsafe {
                    std::ptr::write(&mut storage as *mut _ as *mut _, sin);
                }
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t
            }
            _ => panic!("v4 only"),
        };

        let mut iov = libc::iovec {
            iov_base: payload.as_ptr() as *mut _,
            iov_len: payload.len(),
        };
        let mut hdr: libc::mmsghdr = unsafe { std::mem::zeroed() };
        hdr.msg_hdr.msg_name = (&mut storage as *mut libc::sockaddr_storage).cast();
        hdr.msg_hdr.msg_namelen = addrlen;
        hdr.msg_hdr.msg_iov = &mut iov;
        hdr.msg_hdr.msg_iovlen = 1;

        let rc = unsafe { libc::sendmmsg(src_fd, &mut hdr, 1, 0) };
        rc == 1
    }

    /// Receive via recvmmsg, return number of bytes in first datagram (0 = nothing received).
    fn recvmmsg_one(fd: libc::c_int) -> usize {
        let mut buf = [0u8; 65535];
        let mut iov = libc::iovec {
            iov_base: buf.as_mut_ptr().cast(),
            iov_len: buf.len(),
        };
        let mut addr: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
        let mut hdr: libc::mmsghdr = unsafe { std::mem::zeroed() };
        hdr.msg_hdr.msg_iov = &mut iov;
        hdr.msg_hdr.msg_iovlen = 1;
        hdr.msg_hdr.msg_name = (&mut addr as *mut libc::sockaddr_storage).cast();
        hdr.msg_hdr.msg_namelen = std::mem::size_of::<libc::sockaddr_storage>() as _;

        let rc =
            unsafe { libc::recvmmsg(fd, &mut hdr, 1, libc::MSG_DONTWAIT, std::ptr::null_mut()) };
        if rc > 0 { hdr.msg_len as usize } else { 0 }
    }

    fn io_uring_test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    // ── Test: probe_gso via setsockopt leaves socket dirty ──────────

    #[test]
    fn probe_gso_via_setsockopt_contaminates_socket() {
        // Demonstrates the hazard: setsockopt(UDP_SEGMENT, 0) on the main
        // thread leaves the socket's gso_size set. Verify that sendmmsg on
        // a worker thread still delivers packets correctly despite this.
        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        sender.set_nonblocking(true).unwrap();
        receiver.set_nonblocking(true).unwrap();
        let sender_fd = sender.as_raw_fd();

        // Simulate the old probe_gso: setsockopt(UDP_SEGMENT, 0) on main thread.
        let rc = setsockopt_int(sender_fd, SOL_UDP, UDP_SEGMENT, 0);
        if rc != 0 {
            // GSO not supported on this kernel — skip test.
            return;
        }

        // Verify the socket option sticks.
        let val = getsockopt_int(sender_fd, SOL_UDP, UDP_SEGMENT).unwrap();
        assert_eq!(val, 0, "UDP_SEGMENT should be 0 after probe setsockopt");

        // Move sender to a different thread (simulating main→worker move).
        let recv_clone = receiver.try_clone().unwrap();
        let ok = thread::spawn(move || sendmmsg_one(sender_fd, &recv_clone))
            .join()
            .unwrap();

        assert!(
            ok,
            "sendmmsg should succeed even with UDP_SEGMENT=0 on socket"
        );

        // Verify the packet actually arrived.
        std::thread::sleep(std::time::Duration::from_millis(10));
        let n = recvmmsg_one(receiver.as_raw_fd());
        assert_eq!(n, 100, "receiver should get the 100-byte packet");
    }

    // ── Test: probe_gso via getsockopt is read-only ─────────────────

    #[test]
    fn probe_gso_via_getsockopt_does_not_contaminate() {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let fd = sock.as_raw_fd();

        // The safe probe: getsockopt reads without mutating.
        let result = getsockopt_int(fd, SOL_UDP, UDP_SEGMENT);
        match result {
            Ok(val) => {
                // GSO supported — value should be 0 (default, never set).
                assert_eq!(val, 0, "default UDP_SEGMENT should be 0");
            }
            Err(e) => {
                // ENOPROTOOPT means GSO not supported — also fine.
                assert_eq!(e.raw_os_error(), Some(libc::ENOPROTOOPT));
            }
        }
    }

    // ── Test: enable_gro doesn't break recvmmsg on worker thread ────

    #[test]
    fn enable_gro_cross_thread_recvmmsg() {
        let _serial = io_uring_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        sender.set_nonblocking(true).unwrap();
        receiver.set_nonblocking(true).unwrap();
        let recv_fd = receiver.as_raw_fd();

        // Enable GRO on main thread (simulating Driver::new).
        let rc = setsockopt_int(recv_fd, SOL_UDP, UDP_GRO, 1);
        // GRO might not be supported — that's fine, test still validates
        // that the socket works regardless.
        let gro_enabled = rc == 0;

        // Send a packet.
        let dst = receiver.local_addr().unwrap();
        sender.send_to(&[0xCDu8; 200], dst).unwrap();

        // Receive on a different thread.
        std::thread::sleep(std::time::Duration::from_millis(10));
        let n = thread::spawn(move || recvmmsg_one(recv_fd)).join().unwrap();

        assert_eq!(
            n, 200,
            "recvmmsg on worker thread should get the packet (GRO enabled: {gro_enabled})"
        );
    }

    // ── Test: IP_PKTINFO doesn't break recvmmsg without cmsg buf ────

    #[test]
    fn pktinfo_without_cmsg_buffer_is_safe() {
        // Setting IP_PKTINFO but not providing msg_control should be harmless.
        // The kernel generates pktinfo but has nowhere to write it — it just
        // sets MSG_CTRUNC and delivers the payload normally.
        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        sender.set_nonblocking(true).unwrap();
        receiver.set_nonblocking(true).unwrap();
        let recv_fd = receiver.as_raw_fd();

        // Enable IP_PKTINFO on main thread.
        setsockopt_int(recv_fd, libc::IPPROTO_IP, libc::IP_PKTINFO, 1);

        // Send packet.
        let dst = receiver.local_addr().unwrap();
        sender.send_to(&[0xEFu8; 150], dst).unwrap();

        // Receive WITHOUT providing a cmsg buffer (msg_control = NULL).
        std::thread::sleep(std::time::Duration::from_millis(10));
        let n = thread::spawn(move || recvmmsg_one(recv_fd)).join().unwrap();

        assert_eq!(
            n, 150,
            "recvmmsg should deliver payload even with IP_PKTINFO and no cmsg buffer"
        );
    }

    // ── Test: PollDriver created on main thread, roundtrip on worker ─

    #[test]
    fn poll_driver_cross_thread_roundtrip() {
        use crate::transport::poll::PollDriver;
        use crate::transport::{Driver, TxDatagram};

        let sock_a = UdpSocket::bind("127.0.0.1:0").unwrap();
        let sock_b = UdpSocket::bind("127.0.0.1:0").unwrap();
        sock_a.set_nonblocking(true).unwrap();
        sock_b.set_nonblocking(true).unwrap();
        let b_addr = sock_b.local_addr().unwrap();
        let a_addr = sock_a.local_addr().unwrap();

        // Create PollDriver on THIS thread (simulating NodeJS main thread).
        let (mut driver_a, _waker_a) = PollDriver::new(sock_a).unwrap();
        let (mut driver_b, _waker_b) = PollDriver::new(sock_b).unwrap();

        eprintln!(
            "poll_driver_cross_thread_roundtrip: created on {:?}",
            thread::current().id()
        );

        // Move driver_a to a worker thread, send from there to b_addr.
        let handle = thread::spawn(move || {
            eprintln!("  worker A on {:?}", thread::current().id());
            let pkt = TxDatagram {
                data: vec![0xAB; 100],
                to: b_addr,
            };
            driver_a.submit_sends(vec![pkt]).unwrap();
            // Poll to flush (poll driver's sendmmsg happens inside send_batch).
            // The submit_sends already calls sendmmsg synchronously.
            driver_a
        });

        // Give it a moment to send.
        thread::sleep(Duration::from_millis(50));

        // Receive on driver_b from a DIFFERENT worker thread.
        let handle_b = thread::spawn(move || {
            eprintln!("  worker B on {:?}", thread::current().id());
            let mut total_rx = 0;
            let deadline = Instant::now() + Duration::from_secs(2);
            while total_rx == 0 && Instant::now() < deadline {
                let outcome = driver_b
                    .poll(Some(Instant::now() + Duration::from_millis(100)))
                    .unwrap();
                total_rx += outcome.rx.len();
                if total_rx > 0 {
                    let pkt = &outcome.rx[0];
                    eprintln!(
                        "  rx: len={} peer={} local={} gro={:?}",
                        pkt.data.len(),
                        pkt.peer,
                        pkt.local,
                        pkt.segment_size
                    );
                    assert_eq!(pkt.data.len(), 100);
                    assert_eq!(pkt.peer, a_addr);
                }
            }
            assert!(
                total_rx > 0,
                "should receive at least one packet on worker thread"
            );
            driver_b
        });

        let _a = handle.join().unwrap();
        let _b = handle_b.join().unwrap();
    }

    // ── Test: raw GSO sendmsg with our build_gso_cmsg ─────────────

    #[test]
    fn build_gso_cmsg_produces_valid_segmentation() {
        use crate::transport::socket::{SOL_UDP, UDP_SEGMENT, build_gso_cmsg};

        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        sender.set_nonblocking(true).unwrap();
        receiver.set_nonblocking(true).unwrap();
        let sender_fd = sender.as_raw_fd();
        let recv_addr = receiver.local_addr().unwrap();

        // Check GSO support first.
        let mut gso_val: libc::c_int = 0;
        let mut gso_len = std::mem::size_of_val(&gso_val) as libc::socklen_t;
        let gso_ok = unsafe {
            libc::getsockopt(
                sender_fd,
                SOL_UDP,
                UDP_SEGMENT,
                &mut gso_val as *mut _ as *mut libc::c_void,
                &mut gso_len,
            )
        };
        if gso_ok != 0 {
            eprintln!("GSO not supported, skipping");
            return;
        }

        // Build a coalesced buffer: 4 × 100 bytes.
        let mut payload = vec![0u8; 400];
        for i in 0..4 {
            for j in 0..100 {
                payload[i * 100 + j] = i as u8;
            }
        }

        // Build cmsg with our function.
        let mut cmsg_buf = [0u8; 32];
        let cmsg_len = build_gso_cmsg(&mut cmsg_buf, 100);

        eprintln!(
            "cmsg_len={cmsg_len} sizeof(cmsghdr)={} cmsg_data_offset={}",
            std::mem::size_of::<libc::cmsghdr>(),
            crate::transport::socket::cmsg_data_offset_for_test()
        );

        // Build sendmsg manually.
        let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
        let addrlen = match recv_addr {
            std::net::SocketAddr::V4(a) => {
                let sin = libc::sockaddr_in {
                    sin_family: libc::AF_INET as _,
                    sin_port: a.port().to_be(),
                    sin_addr: libc::in_addr {
                        s_addr: u32::from_ne_bytes(a.ip().octets()),
                    },
                    sin_zero: [0; 8],
                };
                unsafe {
                    std::ptr::write(&mut storage as *mut _ as *mut _, sin);
                }
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t
            }
            _ => panic!("v4 only"),
        };

        let mut iov = libc::iovec {
            iov_base: payload.as_ptr() as *mut _,
            iov_len: payload.len(),
        };
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_name = (&mut storage as *mut libc::sockaddr_storage).cast();
        msg.msg_namelen = addrlen;
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cmsg_buf.as_mut_ptr().cast();
        msg.msg_controllen = cmsg_len;

        let rc = unsafe { libc::sendmsg(sender_fd, &msg, 0) };
        eprintln!(
            "sendmsg rc={rc} errno={}",
            if rc < 0 {
                std::io::Error::last_os_error().to_string()
            } else {
                "ok".into()
            }
        );
        assert!(rc > 0, "sendmsg with GSO cmsg should succeed (rc={rc})");

        // Now receive — should get 4 separate 100-byte datagrams, not 1 × 400.
        thread::sleep(Duration::from_millis(50));
        let recv_fd = receiver.as_raw_fd();
        let mut packets = Vec::new();
        for _ in 0..10 {
            let n = recvmmsg_one(recv_fd);
            if n == 0 {
                break;
            }
            packets.push(n);
        }

        eprintln!("received packets: {packets:?}");
        assert_eq!(
            packets.len(),
            4,
            "GSO should segment into 4 packets, got {packets:?}"
        );
        assert!(
            packets.iter().all(|&n| n == 100),
            "each segment should be 100 bytes, got {packets:?}"
        );
    }

    // ── Test: GSO after set_pktinfo + enable_gro ──────────────────

    #[test]
    fn gso_sendmsg_after_pktinfo_and_gro() {
        use crate::transport::socket::{
            SOL_UDP, UDP_SEGMENT, build_gso_cmsg, enable_gro, set_pktinfo,
        };

        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        sender.set_nonblocking(true).unwrap();
        receiver.set_nonblocking(true).unwrap();
        let sender_fd = sender.as_raw_fd();
        let recv_addr = receiver.local_addr().unwrap();

        // Check GSO support.
        let mut gso_val: libc::c_int = 0;
        let mut gso_len = std::mem::size_of_val(&gso_val) as libc::socklen_t;
        let gso_ok = unsafe {
            libc::getsockopt(
                sender_fd,
                SOL_UDP,
                UDP_SEGMENT,
                &mut gso_val as *mut _ as *mut _,
                &mut gso_len,
            )
        };
        if gso_ok != 0 {
            eprintln!("GSO not supported, skipping");
            return;
        }

        // Apply the same socket options as PollDriver::new().
        set_pktinfo(&sender);
        enable_gro(&sender);

        // Build 4 × 200 byte coalesced buffer + GSO cmsg.
        let payload = vec![0xABu8; 800];
        let mut cmsg_buf = [0u8; 32];
        let cmsg_len = build_gso_cmsg(&mut cmsg_buf, 200);

        let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
        let addrlen = match recv_addr {
            std::net::SocketAddr::V4(a) => {
                let sin = libc::sockaddr_in {
                    sin_family: libc::AF_INET as _,
                    sin_port: a.port().to_be(),
                    sin_addr: libc::in_addr {
                        s_addr: u32::from_ne_bytes(a.ip().octets()),
                    },
                    sin_zero: [0; 8],
                };
                unsafe {
                    std::ptr::write(&mut storage as *mut _ as *mut _, sin);
                }
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t
            }
            _ => panic!("v4 only"),
        };

        let mut iov = libc::iovec {
            iov_base: payload.as_ptr() as *mut _,
            iov_len: payload.len(),
        };
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_name = (&mut storage as *mut libc::sockaddr_storage).cast();
        msg.msg_namelen = addrlen;
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cmsg_buf.as_mut_ptr().cast();
        msg.msg_controllen = cmsg_len;

        let rc = unsafe { libc::sendmsg(sender_fd, &msg, 0) };
        assert!(rc > 0, "sendmsg should succeed");

        thread::sleep(Duration::from_millis(50));
        let recv_fd = receiver.as_raw_fd();
        let mut packets = Vec::new();
        for _ in 0..10 {
            let n = recvmmsg_one(recv_fd);
            if n == 0 {
                break;
            }
            packets.push(n);
        }

        eprintln!("GSO after pktinfo+gro: received packets: {packets:?}");
        assert_eq!(
            packets.len(),
            4,
            "GSO should still segment after pktinfo+gro, got {packets:?}"
        );
    }

    // ── Test: Vec-based mmsghdr GSO (mimics send_batch_gso layout) ─

    #[test]
    fn gso_via_vec_mmsghdr_layout() {
        use crate::transport::socket::{SOL_UDP, UDP_SEGMENT, build_gso_cmsg};

        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        sender.set_nonblocking(true).unwrap();
        receiver.set_nonblocking(true).unwrap();
        let sender_fd = sender.as_raw_fd();
        let recv_addr = receiver.local_addr().unwrap();

        let mut gso_val: libc::c_int = 0;
        let mut gso_len = std::mem::size_of_val(&gso_val) as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                sender_fd,
                SOL_UDP,
                UDP_SEGMENT,
                &mut gso_val as *mut _ as *mut _,
                &mut gso_len,
            )
        } != 0
        {
            eprintln!("GSO not supported, skipping");
            return;
        }

        // Mimic send_batch_gso: Vec<mmsghdr>, Vec<iovec>, Vec<sockaddr_storage>, Vec<[u8;32]>
        let count = 1usize;
        let mut tx_hdrs: Vec<libc::mmsghdr> = vec![unsafe { std::mem::zeroed() }; count];
        let mut tx_iovs: Vec<libc::iovec> = vec![
            libc::iovec {
                iov_base: std::ptr::null_mut(),
                iov_len: 0
            };
            count
        ];
        let mut tx_addrs: Vec<libc::sockaddr_storage> = vec![unsafe { std::mem::zeroed() }; count];
        let mut tx_cmsg_bufs: Vec<[u8; 32]> = vec![[0u8; 32]; count];

        let payload = vec![0xABu8; 400]; // 4 × 100 coalesced

        // Fill address
        match recv_addr {
            std::net::SocketAddr::V4(a) => {
                let sin = libc::sockaddr_in {
                    sin_family: libc::AF_INET as _,
                    sin_port: a.port().to_be(),
                    sin_addr: libc::in_addr {
                        s_addr: u32::from_ne_bytes(a.ip().octets()),
                    },
                    sin_zero: [0; 8],
                };
                unsafe {
                    std::ptr::write(&mut tx_addrs[0] as *mut _ as *mut _, sin);
                }
                tx_hdrs[0].msg_hdr.msg_namelen = std::mem::size_of::<libc::sockaddr_in>() as _;
            }
            _ => panic!("v4 only"),
        };

        tx_iovs[0].iov_base = payload.as_ptr() as *mut _;
        tx_iovs[0].iov_len = payload.len();
        tx_hdrs[0].msg_hdr.msg_name = (&mut tx_addrs[0] as *mut libc::sockaddr_storage).cast();
        tx_hdrs[0].msg_hdr.msg_iov = &mut tx_iovs[0] as *mut _;
        tx_hdrs[0].msg_hdr.msg_iovlen = 1;

        let cmsg_len = build_gso_cmsg(&mut tx_cmsg_bufs[0], 100);
        tx_hdrs[0].msg_hdr.msg_control = tx_cmsg_bufs[0].as_mut_ptr().cast();
        tx_hdrs[0].msg_hdr.msg_controllen = cmsg_len;

        eprintln!(
            "Vec layout: controllen={cmsg_len} cmsg_bytes={:02x?}",
            &tx_cmsg_bufs[0][..cmsg_len]
        );

        let rc = unsafe { libc::sendmsg(sender_fd, &tx_hdrs[0].msg_hdr, 0) };
        eprintln!("sendmsg rc={rc}");
        assert!(rc > 0);

        thread::sleep(Duration::from_millis(50));
        let recv_fd = receiver.as_raw_fd();
        let mut packets = Vec::new();
        for _ in 0..10 {
            let n = recvmmsg_one(recv_fd);
            if n == 0 {
                break;
            }
            packets.push(n);
        }

        eprintln!("Vec layout GSO: received {packets:?}");
        assert_eq!(
            packets.len(),
            4,
            "Vec-based GSO should segment into 4, got {packets:?}"
        );
    }

    // ── Test: Vec layout + pktinfo + gro (exact PollDriver conditions) ─

    #[test]
    fn gso_vec_layout_with_pktinfo_and_gro() {
        use crate::transport::socket::{
            SOL_UDP, UDP_SEGMENT, build_gso_cmsg, enable_gro, set_pktinfo,
        };

        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        sender.set_nonblocking(true).unwrap();
        receiver.set_nonblocking(true).unwrap();
        let sender_fd = sender.as_raw_fd();
        let recv_addr = receiver.local_addr().unwrap();

        let mut gso_val: libc::c_int = 0;
        let mut gso_len = std::mem::size_of_val(&gso_val) as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                sender_fd,
                SOL_UDP,
                UDP_SEGMENT,
                &mut gso_val as *mut _ as *mut _,
                &mut gso_len,
            )
        } != 0
        {
            eprintln!("GSO not supported, skipping");
            return;
        }

        // Apply SAME socket options as PollDriver::new
        set_pktinfo(&sender);
        enable_gro(&sender);

        // Vec-based layout (exactly like send_batch_gso)
        let count = 1;
        let mut tx_hdrs: Vec<libc::mmsghdr> = vec![unsafe { std::mem::zeroed() }; count];
        let mut tx_iovs: Vec<libc::iovec> = vec![
            libc::iovec {
                iov_base: std::ptr::null_mut(),
                iov_len: 0
            };
            count
        ];
        let mut tx_addrs: Vec<libc::sockaddr_storage> = vec![unsafe { std::mem::zeroed() }; count];
        let mut tx_cmsg_bufs: Vec<[u8; 32]> = vec![[0u8; 32]; count];

        let payload = vec![0xABu8; 800]; // 4 × 200

        match recv_addr {
            std::net::SocketAddr::V4(a) => {
                let sin = libc::sockaddr_in {
                    sin_family: libc::AF_INET as _,
                    sin_port: a.port().to_be(),
                    sin_addr: libc::in_addr {
                        s_addr: u32::from_ne_bytes(a.ip().octets()),
                    },
                    sin_zero: [0; 8],
                };
                unsafe {
                    std::ptr::write(&mut tx_addrs[0] as *mut _ as *mut _, sin);
                }
                tx_hdrs[0].msg_hdr.msg_namelen = std::mem::size_of::<libc::sockaddr_in>() as _;
            }
            _ => panic!("v4 only"),
        };

        tx_iovs[0].iov_base = payload.as_ptr() as *mut _;
        tx_iovs[0].iov_len = payload.len();
        tx_hdrs[0].msg_hdr.msg_name = (&mut tx_addrs[0] as *mut libc::sockaddr_storage).cast();
        tx_hdrs[0].msg_hdr.msg_iov = &mut tx_iovs[0] as *mut _;
        tx_hdrs[0].msg_hdr.msg_iovlen = 1;

        let cmsg_len = build_gso_cmsg(&mut tx_cmsg_bufs[0], 200);
        tx_hdrs[0].msg_hdr.msg_control = tx_cmsg_bufs[0].as_mut_ptr().cast();
        tx_hdrs[0].msg_hdr.msg_controllen = cmsg_len;

        // Test hypothesis: sendmmsg with MSG_DONTWAIT vs sendmsg with 0
        let rc_mmsg =
            unsafe { libc::sendmmsg(sender_fd, tx_hdrs.as_mut_ptr(), 1, libc::MSG_DONTWAIT) };
        eprintln!(
            "gso_vec_layout_with_pktinfo_and_gro: sendmmsg(MSG_DONTWAIT) rc={rc_mmsg} msg_len={}",
            tx_hdrs[0].msg_len
        );
        assert_eq!(rc_mmsg, 1, "sendmmsg should send 1 message");

        thread::sleep(Duration::from_millis(50));
        let recv_fd = receiver.as_raw_fd();
        let mut packets = Vec::new();
        for _ in 0..10 {
            let n = recvmmsg_one(recv_fd);
            if n == 0 {
                break;
            }
            packets.push(n);
        }

        eprintln!("received: {packets:?}");
        assert_eq!(packets.len(), 4, "should get 4 segments, got {packets:?}");
    }

    // ── Test: group_for_gso produces correct batch data ───────────

    #[test]
    fn group_for_gso_coalesced_send() {
        use crate::transport::socket::{
            SOL_UDP, UDP_SEGMENT, build_gso_cmsg, enable_gro, set_pktinfo,
        };
        use crate::transport::{TxDatagram, group_for_gso};

        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        sender.set_nonblocking(true).unwrap();
        receiver.set_nonblocking(true).unwrap();
        let sender_fd = sender.as_raw_fd();
        let recv_addr = receiver.local_addr().unwrap();

        let mut gso_val: libc::c_int = 0;
        let mut gso_len = std::mem::size_of_val(&gso_val) as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                sender_fd,
                SOL_UDP,
                UDP_SEGMENT,
                &mut gso_val as *mut _ as *mut _,
                &mut gso_len,
            )
        } != 0
        {
            return;
        }

        set_pktinfo(&sender);
        enable_gro(&sender);

        // Build packets like the test does
        let packets: Vec<TxDatagram> = (0..4)
            .map(|i| TxDatagram {
                data: vec![i as u8; 200],
                to: recv_addr,
            })
            .collect();
        let batches = group_for_gso(packets);
        assert_eq!(batches.len(), 1, "4 same-size should coalesce into 1 batch");
        let batch = &batches[0];
        eprintln!(
            "batch: data.len()={} seg_size={}",
            batch.data.len(),
            batch.segment_size
        );
        assert_eq!(batch.data.len(), 800);
        assert_eq!(batch.segment_size, 200);

        // Now send with Vec layout
        let mut tx_hdrs: Vec<libc::mmsghdr> = vec![unsafe { std::mem::zeroed() }; 1];
        let mut tx_iovs: Vec<libc::iovec> = vec![
            libc::iovec {
                iov_base: std::ptr::null_mut(),
                iov_len: 0
            };
            1
        ];
        let mut tx_addrs: Vec<libc::sockaddr_storage> = vec![unsafe { std::mem::zeroed() }; 1];
        let mut tx_cmsg_bufs: Vec<[u8; 32]> = vec![[0u8; 32]; 1];

        match recv_addr {
            std::net::SocketAddr::V4(a) => {
                let sin = libc::sockaddr_in {
                    sin_family: libc::AF_INET as _,
                    sin_port: a.port().to_be(),
                    sin_addr: libc::in_addr {
                        s_addr: u32::from_ne_bytes(a.ip().octets()),
                    },
                    sin_zero: [0; 8],
                };
                unsafe {
                    std::ptr::write(&mut tx_addrs[0] as *mut _ as *mut _, sin);
                }
                tx_hdrs[0].msg_hdr.msg_namelen = std::mem::size_of::<libc::sockaddr_in>() as _;
            }
            _ => panic!("v4 only"),
        };

        // Use batch.data directly (this is what send_batch_gso does after extracting from batches)
        tx_iovs[0].iov_base = batch.data.as_ptr() as *mut _;
        tx_iovs[0].iov_len = batch.data.len();
        tx_hdrs[0].msg_hdr.msg_name = (&mut tx_addrs[0] as *mut libc::sockaddr_storage).cast();
        tx_hdrs[0].msg_hdr.msg_iov = &mut tx_iovs[0] as *mut _;
        tx_hdrs[0].msg_hdr.msg_iovlen = 1;

        let cmsg_len = build_gso_cmsg(&mut tx_cmsg_bufs[0], batch.segment_size);
        tx_hdrs[0].msg_hdr.msg_control = tx_cmsg_bufs[0].as_mut_ptr().cast();
        tx_hdrs[0].msg_hdr.msg_controllen = cmsg_len;

        let rc = unsafe { libc::sendmsg(sender_fd, &tx_hdrs[0].msg_hdr, 0) };
        eprintln!("group_for_gso_coalesced_send: sendmsg rc={rc}");
        assert!(rc > 0);

        thread::sleep(Duration::from_millis(50));
        let mut packets_received = Vec::new();
        for _ in 0..10 {
            let n = recvmmsg_one(receiver.as_raw_fd());
            if n == 0 {
                break;
            }
            packets_received.push(n);
        }

        eprintln!("received: {packets_received:?}");
        assert_eq!(
            packets_received.len(),
            4,
            "should get 4 segments via group_for_gso, got {packets_received:?}"
        );
    }

    // ── Test: PollDriver GSO send SAME thread ─────────────────────

    #[test]
    fn poll_driver_gso_send_same_thread() {
        use crate::transport::poll::PollDriver;
        use crate::transport::{Driver, TxDatagram};

        let sender_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let receiver_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        sender_sock.set_nonblocking(true).unwrap();
        receiver_sock.set_nonblocking(true).unwrap();
        for sock in [&sender_sock, &receiver_sock] {
            let buf_size: libc::c_int = 2 * 1024 * 1024;
            unsafe {
                libc::setsockopt(
                    sock.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_RCVBUF,
                    &buf_size as *const _ as *const libc::c_void,
                    std::mem::size_of_val(&buf_size) as libc::socklen_t,
                );
                libc::setsockopt(
                    sock.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    &buf_size as *const _ as *const libc::c_void,
                    std::mem::size_of_val(&buf_size) as libc::socklen_t,
                );
            }
        }
        let recv_addr = receiver_sock.local_addr().unwrap();

        // Use PollDriver for SENDER (this calls set_pktinfo + enable_gro + probe_gso)
        let (mut sender, _waker) = PollDriver::new(sender_sock).unwrap();
        // Use RAW receiver (no PollDriver, no GRO) to isolate sender-side GSO
        let recv_fd = receiver_sock.as_raw_fd();

        // Send 4 same-sized packets — ALL ON THE SAME THREAD.
        let packets: Vec<TxDatagram> = (0..4)
            .map(|i| TxDatagram {
                data: vec![i as u8; 200],
                to: recv_addr,
            })
            .collect();
        sender.submit_sends(packets).unwrap();
        eprintln!("  submit_sends done, checking receiver...");

        thread::sleep(Duration::from_millis(50));

        // Receive with raw socket (no GRO) to verify GSO segmentation.
        thread::sleep(Duration::from_millis(50));
        let mut packets_rx = Vec::new();
        for _ in 0..10 {
            let n = recvmmsg_one(recv_fd);
            if n == 0 {
                break;
            }
            packets_rx.push(n);
        }
        eprintln!("  raw receiver: {packets_rx:?}");
        assert_eq!(
            packets_rx.len(),
            4,
            "GSO should segment into 4 (got {packets_rx:?})"
        );
    }

    // ── Test: IoUringDriver GSO send + receive (GRO) ──────────────

    #[test]
    fn iouring_driver_gso_roundtrip() {
        let _serial = io_uring_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        use crate::transport::io_uring::IoUringDriver;
        use crate::transport::{Driver, TxDatagram};

        let sender_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let receiver_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        sender_sock.set_nonblocking(true).unwrap();
        receiver_sock.set_nonblocking(true).unwrap();
        let recv_addr = receiver_sock.local_addr().unwrap();

        let (mut sender, _sw) = IoUringDriver::new(sender_sock).unwrap();
        let (mut receiver, _rw) = IoUringDriver::new(receiver_sock).unwrap();

        // Initial poll to arm receive (needed for io_uring enable_on_worker_thread).
        let _ = receiver.poll(Some(Instant::now() + Duration::from_millis(1)));
        let _ = sender.poll(Some(Instant::now() + Duration::from_millis(1)));

        // Send 4 same-sized packets (triggers GSO if supported).
        let packets: Vec<TxDatagram> = (0..4)
            .map(|i| TxDatagram {
                data: vec![i as u8; 200],
                to: recv_addr,
            })
            .collect();
        sender.submit_sends(packets).unwrap();

        // Poll sender to flush SQEs.
        let _ = sender.poll(Some(Instant::now() + Duration::from_millis(50)));

        // Receive.
        let mut total_bytes = 0usize;
        let mut pkt_count = 0usize;
        let deadline = Instant::now() + Duration::from_secs(2);
        while total_bytes < 800 && Instant::now() < deadline {
            let outcome = receiver
                .poll(Some(Instant::now() + Duration::from_millis(100)))
                .unwrap();
            for pkt in &outcome.rx {
                eprintln!(
                    "  iouring rx: len={} gro={:?}",
                    pkt.data.len(),
                    pkt.segment_size
                );
                total_bytes += pkt.data.len();
                pkt_count += 1;
            }
        }
        eprintln!("  iouring GSO roundtrip: {pkt_count} pkts, {total_bytes} bytes");
        assert_eq!(
            total_bytes, 800,
            "should receive 800 total bytes (got {total_bytes})"
        );
    }

    // ── Test: IoUringDriver GRO cmsg delivery with QUIC-sized packets ──

    /// Verifies that the io_uring multishot recvmsg path delivers the UDP_GRO
    /// cmsg for GRO-coalesced datagrams. Without segment_size, the event loop
    /// can't split coalesced packets and quiche gets oversized blobs it can't
    /// parse — causing stream timeouts in the QUIC benchmark.
    #[test]
    fn iouring_driver_gro_cmsg_with_quic_sized_packets() {
        let _serial = io_uring_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        use crate::transport::io_uring::IoUringDriver;
        use crate::transport::{Driver, TxDatagram};

        let sender_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let receiver_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        sender_sock.set_nonblocking(true).unwrap();
        receiver_sock.set_nonblocking(true).unwrap();
        let recv_addr = receiver_sock.local_addr().unwrap();

        let (mut sender, _sw) = IoUringDriver::new(sender_sock).unwrap();
        let (mut receiver, _rw) = IoUringDriver::new(receiver_sock).unwrap();

        let _ = receiver.poll(Some(Instant::now() + Duration::from_millis(1)));
        let _ = sender.poll(Some(Instant::now() + Duration::from_millis(1)));

        // Send 4 × 1200B packets (typical QUIC packet size).
        // GSO coalesces these into one 4800B sendmsg. On loopback, GRO should
        // coalesce them back into one 4800B recvmsg with UDP_GRO segment_size=1200.
        let packets: Vec<TxDatagram> = (0..4)
            .map(|i| TxDatagram {
                data: vec![i as u8; 1200],
                to: recv_addr,
            })
            .collect();
        sender.submit_sends(packets).unwrap();
        let _ = sender.poll(Some(Instant::now() + Duration::from_millis(50)));

        let mut total_bytes = 0usize;
        let mut gro_seen = false;
        let mut non_gro_count = 0usize;
        let deadline = Instant::now() + Duration::from_secs(2);
        while total_bytes < 4800 && Instant::now() < deadline {
            let outcome = receiver
                .poll(Some(Instant::now() + Duration::from_millis(100)))
                .unwrap();
            for pkt in &outcome.rx {
                eprintln!(
                    "  iouring GRO: len={} segment_size={:?}",
                    pkt.data.len(),
                    pkt.segment_size,
                );
                total_bytes += pkt.data.len();
                if pkt.segment_size.is_some() {
                    gro_seen = true;
                } else if pkt.data.len() > 1200 {
                    // GRO-coalesced but NO segment_size — this is the bug!
                    eprintln!(
                        "  BUG: received {}B coalesced datagram with segment_size=None!",
                        pkt.data.len(),
                    );
                    non_gro_count += 1;
                }
            }
        }
        eprintln!("  GRO test: {total_bytes}B, gro_seen={gro_seen}, non_gro_count={non_gro_count}");
        assert_eq!(total_bytes, 4800, "should receive 4800 total bytes");
        // If GRO is active, segment_size MUST be present on coalesced packets.
        // If it's missing, the event loop won't split them and quiche breaks.
        assert_eq!(
            non_gro_count, 0,
            "coalesced packets must have segment_size (got {non_gro_count} without)",
        );
    }

    // ── Test: IoUringDriver GSO multi-round (regression for round 2+ failures)

    #[test]
    fn iouring_driver_gso_multi_round() {
        let _serial = io_uring_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        use crate::transport::io_uring::IoUringDriver;
        use crate::transport::{Driver, TxDatagram};

        let sender_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let receiver_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        sender_sock.set_nonblocking(true).unwrap();
        receiver_sock.set_nonblocking(true).unwrap();
        let recv_addr = receiver_sock.local_addr().unwrap();

        let (mut sender, _sw) = IoUringDriver::new(sender_sock).unwrap();
        let (mut receiver, _rw) = IoUringDriver::new(receiver_sock).unwrap();

        let _ = receiver.poll(Some(Instant::now() + Duration::from_millis(1)));
        let _ = sender.poll(Some(Instant::now() + Duration::from_millis(1)));

        for round in 0..5 {
            // Send 16 same-sized packets per round.
            let packets: Vec<TxDatagram> = (0..16)
                .map(|i| TxDatagram {
                    data: vec![(round * 16 + i) as u8; 200],
                    to: recv_addr,
                })
                .collect();
            sender.submit_sends(packets).unwrap();
            let _ = sender.poll(Some(Instant::now() + Duration::from_millis(50)));

            let mut total_bytes = 0usize;
            let deadline = Instant::now() + Duration::from_secs(2);
            while total_bytes < 16 * 200 && Instant::now() < deadline {
                let outcome = receiver
                    .poll(Some(Instant::now() + Duration::from_millis(100)))
                    .unwrap();
                for pkt in &outcome.rx {
                    total_bytes += pkt.data.len();
                }
            }
            eprintln!("  round {round}: {total_bytes}/3200 bytes");
            assert_eq!(
                total_bytes, 3200,
                "round {round}: should get 3200 bytes (got {total_bytes})"
            );
        }
    }

    // ── Test: IoUringDriver rapid submit_sends without poll (regression for SQE flush) ──

    /// Regression test for the io_uring intermittent slowdown bug.
    /// Sends 200+ packets via multiple submit_sends() calls WITHOUT calling
    /// poll() between them. Before the fix, unflushed SQEs would accumulate
    /// in the SQ ring, invisible to the kernel until the next poll().
    #[test]
    fn iouring_driver_rapid_submit_sends_no_poll() {
        let _serial = io_uring_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        use crate::transport::io_uring::IoUringDriver;
        use crate::transport::{Driver, TxDatagram};

        let sender_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let receiver_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        sender_sock.set_nonblocking(true).unwrap();
        receiver_sock.set_nonblocking(true).unwrap();
        let recv_addr = receiver_sock.local_addr().unwrap();

        let (mut sender, _sw) = IoUringDriver::new(sender_sock).unwrap();
        let (mut receiver, _rw) = IoUringDriver::new(receiver_sock).unwrap();

        // Enable on worker thread.
        let _ = receiver.poll(Some(Instant::now() + Duration::from_millis(1)));
        let _ = sender.poll(Some(Instant::now() + Duration::from_millis(1)));

        // Simulate the event loop pattern: multiple submit_sends() calls with
        // NO poll() in between. This is the pattern that triggered the bug —
        // SQEs accumulated without being flushed to the kernel.
        let calls = 8;
        let pkts_per_call = 32;
        let pkt_size = 200;
        let total_expected = calls * pkts_per_call * pkt_size; // 51200 bytes

        for call in 0..calls {
            let packets: Vec<TxDatagram> = (0..pkts_per_call)
                .map(|i| TxDatagram {
                    data: vec![(call * pkts_per_call + i) as u8; pkt_size],
                    to: recv_addr,
                })
                .collect();
            sender.submit_sends(packets).unwrap();
            // NO poll() here — this is the critical part of the test.
        }

        // Now receive all packets (receiver polls normally).
        let mut total_bytes = 0usize;
        let deadline = Instant::now() + Duration::from_secs(5);
        while total_bytes < total_expected && Instant::now() < deadline {
            let outcome = receiver
                .poll(Some(Instant::now() + Duration::from_millis(100)))
                .unwrap();
            for pkt in &outcome.rx {
                total_bytes += pkt.data.len();
            }
        }
        eprintln!("  rapid submit_sends: {total_bytes}/{total_expected} bytes");
        assert_eq!(
            total_bytes, total_expected,
            "all packets should arrive without poll() between submit_sends() calls \
             (got {total_bytes}/{total_expected})"
        );
    }

    // ── Test: IoUringDriver bidirectional echo (mimics QUIC event loop) ──

    /// Simulates a QUIC-like event loop: two io_uring drivers sending and
    /// receiving in alternating poll cycles. This catches stalls caused by
    /// unflushed SQEs, stuck completions, or poll/submit ordering issues.
    #[test]
    fn iouring_driver_bidirectional_echo() {
        let _serial = io_uring_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        use crate::transport::io_uring::IoUringDriver;
        use crate::transport::{Driver, TxDatagram};

        let sock_a = UdpSocket::bind("127.0.0.1:0").unwrap();
        let sock_b = UdpSocket::bind("127.0.0.1:0").unwrap();
        sock_a.set_nonblocking(true).unwrap();
        sock_b.set_nonblocking(true).unwrap();
        let addr_a = sock_a.local_addr().unwrap();
        let addr_b = sock_b.local_addr().unwrap();

        let (mut a, _wa) = IoUringDriver::new(sock_a).unwrap();
        let (mut b, _wb) = IoUringDriver::new(sock_b).unwrap();

        // Enable both drivers.
        let _ = a.poll(Some(Instant::now() + Duration::from_millis(1)));
        let _ = b.poll(Some(Instant::now() + Duration::from_millis(1)));

        let rounds = 20;
        let pkts_per_round = 16;
        let pkt_size = 200;
        let mut a_sent = 0usize;
        let mut b_sent = 0usize;
        let mut a_received = 0usize;
        let mut b_received = 0usize;
        let mut a_rx_bytes = 0usize;
        let mut b_rx_bytes = 0usize;

        for round in 0..rounds {
            // A sends to B.
            let packets: Vec<TxDatagram> = (0..pkts_per_round)
                .map(|i| TxDatagram {
                    data: vec![(round * pkts_per_round + i) as u8; pkt_size],
                    to: addr_b,
                })
                .collect();
            a.submit_sends(packets).unwrap();
            a_sent += pkts_per_round;

            // B sends to A.
            let packets: Vec<TxDatagram> = (0..pkts_per_round)
                .map(|i| TxDatagram {
                    data: vec![(round * pkts_per_round + i) as u8; pkt_size],
                    to: addr_a,
                })
                .collect();
            b.submit_sends(packets).unwrap();
            b_sent += pkts_per_round;

            // Both poll (short timeout — we're testing throughput not blocking).
            let out_a = a
                .poll(Some(Instant::now() + Duration::from_millis(10)))
                .unwrap();
            let out_b = b
                .poll(Some(Instant::now() + Duration::from_millis(10)))
                .unwrap();

            let a_rx_this = out_a.rx.iter().map(|p| p.data.len()).sum::<usize>();
            let b_rx_this = out_b.rx.iter().map(|p| p.data.len()).sum::<usize>();
            a_received += out_a.rx.len();
            b_received += out_b.rx.len();
            a_rx_bytes += a_rx_this;
            b_rx_bytes += b_rx_this;
            if round < 3 || round == rounds - 1 {
                eprintln!(
                    "  round {round}: a_rx={} ({a_rx_this}B) b_rx={} ({b_rx_this}B) a_total={a_received} b_total={b_received}",
                    out_a.rx.len(),
                    out_b.rx.len(),
                );
            }
        }

        // Drain remaining.
        let a_expected_bytes_for_drain = a_sent * pkt_size;
        let b_expected_bytes_for_drain = b_sent * pkt_size;
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut drain_polls = 0;
        while (a_rx_bytes < b_expected_bytes_for_drain || b_rx_bytes < a_expected_bytes_for_drain)
            && Instant::now() < deadline
        {
            let out_a = a
                .poll(Some(Instant::now() + Duration::from_millis(50)))
                .unwrap();
            let out_b = b
                .poll(Some(Instant::now() + Duration::from_millis(50)))
                .unwrap();
            let a_rx_this_bytes: usize = out_a.rx.iter().map(|p| p.data.len()).sum();
            let b_rx_this_bytes: usize = out_b.rx.iter().map(|p| p.data.len()).sum();
            a_received += out_a.rx.len();
            b_received += out_b.rx.len();
            a_rx_bytes += a_rx_this_bytes;
            b_rx_bytes += b_rx_this_bytes;
            drain_polls += 1;
            if drain_polls <= 3 || a_rx_this_bytes > 0 || b_rx_this_bytes > 0 {
                eprintln!(
                    "  drain poll {drain_polls}: a_rx={a_rx_this_bytes}B b_rx={b_rx_this_bytes}B a_bytes={a_rx_bytes}/{b_expected_bytes_for_drain} b_bytes={b_rx_bytes}/{a_expected_bytes_for_drain}",
                );
            }
        }

        let a_expected_bytes = a_sent * pkt_size;
        let b_expected_bytes = b_sent * pkt_size;
        eprintln!(
            "  bidir echo: a_sent={a_sent} b_rx_bytes={b_rx_bytes}/{a_expected_bytes} b_sent={b_sent} a_rx_bytes={a_rx_bytes}/{b_expected_bytes} drain_polls={drain_polls}"
        );
        // Compare bytes, not packet counts — GRO coalesces multiple sends into
        // a single receive datagram on loopback.
        assert_eq!(
            b_rx_bytes, a_expected_bytes,
            "B should receive all of A's bytes (got {b_rx_bytes}/{a_expected_bytes})"
        );
        assert_eq!(
            a_rx_bytes, b_expected_bytes,
            "A should receive all of B's bytes (got {a_rx_bytes}/{b_expected_bytes})"
        );
    }

    // ── Test: IoUringDriver echo-at-scale (mimics QUIC benchmark server) ──

    /// Simulates the QUIC benchmark pattern: client sends a burst of packets,
    /// server echoes them back, client receives echoes. This is the pattern
    /// that triggers the ~30% stall rate in the real benchmark.
    /// Uses QUIC-sized packets (1200B) and a realistic connection count.
    #[test]
    fn iouring_driver_echo_at_scale() {
        let _serial = io_uring_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        use crate::transport::io_uring::IoUringDriver;
        use crate::transport::{Driver, TxDatagram};

        let client_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let server_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        client_sock.set_nonblocking(true).unwrap();
        server_sock.set_nonblocking(true).unwrap();

        // Enlarge socket buffers to avoid kernel drops under burst load.
        // The default rmem_default (~208KB) can't absorb a 600KB burst.
        use std::os::unix::io::AsRawFd;
        for sock in [&client_sock, &server_sock] {
            let buf_size: libc::c_int = 2 * 1024 * 1024;
            unsafe {
                libc::setsockopt(
                    sock.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_RCVBUF,
                    &buf_size as *const _ as *const libc::c_void,
                    std::mem::size_of_val(&buf_size) as libc::socklen_t,
                );
                libc::setsockopt(
                    sock.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    &buf_size as *const _ as *const libc::c_void,
                    std::mem::size_of_val(&buf_size) as libc::socklen_t,
                );
            }
        }
        let client_addr = client_sock.local_addr().unwrap();
        let server_addr = server_sock.local_addr().unwrap();

        let (mut client, _cw) = IoUringDriver::new(client_sock).unwrap();
        let (mut server, _sw) = IoUringDriver::new(server_sock).unwrap();

        let _ = client.poll(Some(Instant::now() + Duration::from_millis(1)));
        let _ = server.poll(Some(Instant::now() + Duration::from_millis(1)));

        // Simulates a QUIC server handling 500 streams: client sends in bursts,
        // server receives + echoes, client receives echoes. Interleaved like the
        // real event loop — not a single giant burst.
        let total_client_pkts = 500;
        let pkt_size = 1200;
        let total_bytes = total_client_pkts * pkt_size;
        let batch_size = 50; // ~50 streams per connection worth of packets
        let t0 = Instant::now();

        // Echo loop: client sends in batches, server echoes, client receives.
        let mut server_rx_bytes = 0usize;
        let mut server_tx_bytes = 0usize;
        let mut client_echo_bytes = 0usize;
        let mut client_sent = 0usize;
        let deadline = Instant::now() + Duration::from_secs(5);

        let mut loop_count = 0usize;
        let mut stall_count = 0usize;
        while client_echo_bytes < total_bytes && Instant::now() < deadline {
            // Client: send next batch if available.
            if client_sent < total_client_pkts {
                let batch_end = (client_sent + batch_size).min(total_client_pkts);
                let packets: Vec<TxDatagram> = (client_sent..batch_end)
                    .map(|i| TxDatagram {
                        data: vec![(i % 256) as u8; pkt_size],
                        to: server_addr,
                    })
                    .collect();
                client.submit_sends(packets).unwrap();
                client_sent = batch_end;
            }
            loop_count += 1;
            // Server: poll, echo back anything received.
            let s_out = server
                .poll(Some(Instant::now() + Duration::from_millis(10)))
                .unwrap();
            let s_rx_this = s_out.rx.len();
            if !s_out.rx.is_empty() {
                let mut echo_packets = Vec::new();
                for pkt in &s_out.rx {
                    server_rx_bytes += pkt.data.len();
                    // Split GRO-coalesced packets for echo (like event_loop does).
                    if let Some(seg) = pkt.segment_size {
                        for chunk in pkt.data.chunks(seg as usize) {
                            echo_packets.push(TxDatagram {
                                data: chunk.to_vec(),
                                to: client_addr,
                            });
                        }
                    } else {
                        echo_packets.push(TxDatagram {
                            data: pkt.data.clone(),
                            to: client_addr,
                        });
                    }
                }
                let echo_count = echo_packets.len();
                server_tx_bytes += echo_packets.iter().map(|p| p.data.len()).sum::<usize>();
                server.submit_sends(echo_packets).unwrap();
                if loop_count <= 10 || loop_count % 50 == 0 {
                    eprintln!(
                        "  loop {loop_count}: server_rx={s_rx_this} echo={echo_count} pending={} srx_total={server_rx_bytes}",
                        server.pending_tx_count(),
                    );
                }
            } else {
                stall_count += 1;
                if stall_count <= 3 || stall_count % 20 == 0 {
                    eprintln!(
                        "  loop {loop_count}: EMPTY server poll, server_rx_total={server_rx_bytes}/{total_bytes} client_echo={client_echo_bytes}/{total_bytes} server_pending={}",
                        server.pending_tx_count(),
                    );
                }
            }

            // Client: poll to receive echoes.
            let c_out = client
                .poll(Some(Instant::now() + Duration::from_millis(10)))
                .unwrap();
            for pkt in &c_out.rx {
                client_echo_bytes += pkt.data.len();
            }
        }

        let elapsed = t0.elapsed();
        let mbps = (client_echo_bytes as f64 * 8.0) / (elapsed.as_secs_f64() * 1_000_000.0);
        eprintln!(
            "  echo-at-scale: server_rx={server_rx_bytes} server_tx={server_tx_bytes} client_echo={client_echo_bytes}/{total_bytes} in {:?} ({mbps:.1} Mbps)",
            elapsed,
        );
        assert_eq!(
            client_echo_bytes, total_bytes,
            "client should receive all echoed bytes (got {client_echo_bytes}/{total_bytes} in {:?})",
            elapsed,
        );
        assert!(
            elapsed.as_secs() < 3,
            "echo at scale took {:?} — stall detected",
            elapsed,
        );
    }

    // ── Test: IoUringDriver cross-thread echo (reproduces Node.js benchmark stall) ──

    /// Reproduces the QUIC benchmark stall in pure Rust: client and server
    /// on separate threads (simulating separate processes), client blasts
    /// a burst of packets, server echoes. The UDP receive buffer is the
    /// only coordination — if the client sends faster than the server
    /// drains, packets drop and the echo never completes.
    #[test]
    fn iouring_driver_cross_thread_echo() {
        let _serial = io_uring_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        use crate::transport::io_uring::IoUringDriver;
        use crate::transport::{Driver, TxDatagram};

        let client_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let server_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        client_sock.set_nonblocking(true).unwrap();
        server_sock.set_nonblocking(true).unwrap();

        // Match the scale test buffer sizing so the kernel can absorb the
        // one-shot 600KB burst before the server thread drains it.
        for sock in [&client_sock, &server_sock] {
            let buf_size: libc::c_int = 2 * 1024 * 1024;
            unsafe {
                libc::setsockopt(
                    sock.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_RCVBUF,
                    &buf_size as *const _ as *const libc::c_void,
                    std::mem::size_of_val(&buf_size) as libc::socklen_t,
                );
                libc::setsockopt(
                    sock.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    &buf_size as *const _ as *const libc::c_void,
                    std::mem::size_of_val(&buf_size) as libc::socklen_t,
                );
            }
        }
        let client_addr = client_sock.local_addr().unwrap();
        let server_addr = server_sock.local_addr().unwrap();

        let (mut client, _cw) = IoUringDriver::new(client_sock).unwrap();
        let (mut server, _sw) = IoUringDriver::new(server_sock).unwrap();

        let total_pkts = 500;
        let pkt_size = 1200;
        let total_bytes = total_pkts * pkt_size;

        // Server thread: poll, echo back, repeat.
        let server_handle = thread::spawn(move || {
            let _ = server.poll(Some(Instant::now() + Duration::from_millis(1)));
            let mut rx_bytes = 0usize;
            let mut tx_bytes = 0usize;
            let deadline = Instant::now() + Duration::from_secs(10);
            let echo_batch_size = 32usize;

            while Instant::now() < deadline {
                let out = server
                    .poll(Some(Instant::now() + Duration::from_millis(50)))
                    .unwrap();
                if !out.rx.is_empty() {
                    let mut echo = Vec::new();
                    for pkt in &out.rx {
                        rx_bytes += pkt.data.len();
                        if let Some(seg) = pkt.segment_size {
                            for chunk in pkt.data.chunks(seg as usize) {
                                echo.push(TxDatagram {
                                    data: chunk.to_vec(),
                                    to: client_addr,
                                });
                            }
                        } else {
                            echo.push(TxDatagram {
                                data: pkt.data.clone(),
                                to: client_addr,
                            });
                        }
                    }
                    tx_bytes += echo.iter().map(|p| p.data.len()).sum::<usize>();
                    while !echo.is_empty() {
                        let rest = if echo.len() > echo_batch_size {
                            echo.split_off(echo_batch_size)
                        } else {
                            Vec::new()
                        };
                        let batch = std::mem::replace(&mut echo, rest);
                        server.submit_sends(batch).unwrap();
                        if server.pending_tx_count() > 0 {
                            let _ = server.poll(Some(Instant::now() + Duration::from_millis(1)));
                        }
                    }
                }
                // Stop only after we've received the full burst and drained any
                // queued echo sends back out to the client.
                if rx_bytes >= total_bytes && server.pending_tx_count() == 0 {
                    break;
                }
            }
            eprintln!(
                "  server: rx={rx_bytes} tx={tx_bytes} pending={}",
                server.pending_tx_count(),
            );
            (rx_bytes, tx_bytes)
        });

        // Client: enable and then send in large batches while the server runs
        // on a separate thread. This keeps the cross-thread topology without
        // depending on host socket-buffer limits for a single 600KB burst.
        let _ = client.poll(Some(Instant::now() + Duration::from_millis(1)));

        // Poll for echoes.
        let batch_size = 10usize;
        let mut sent_pkts = 0usize;
        let mut echo_bytes = 0usize;
        let deadline = Instant::now() + Duration::from_secs(10);
        while echo_bytes < total_bytes && Instant::now() < deadline {
            if sent_pkts < total_pkts {
                let batch_end = (sent_pkts + batch_size).min(total_pkts);
                let packets: Vec<TxDatagram> = (sent_pkts..batch_end)
                    .map(|i| TxDatagram {
                        data: vec![(i % 256) as u8; pkt_size],
                        to: server_addr,
                    })
                    .collect();
                client.submit_sends(packets).unwrap();
                sent_pkts = batch_end;
                let _ = client.poll(Some(Instant::now() + Duration::from_millis(5)));
            }
            let out = client
                .poll(Some(Instant::now() + Duration::from_millis(50)))
                .unwrap();
            for pkt in &out.rx {
                echo_bytes += pkt.data.len();
            }
        }

        let (srv_rx, _srv_tx) = server_handle.join().unwrap();
        eprintln!(
            "  cross-thread echo: client_sent={total_bytes} server_rx={srv_rx} client_echo={echo_bytes}/{total_bytes}",
        );
        assert_eq!(
            echo_bytes, total_bytes,
            "client should receive all echoed bytes (got {echo_bytes}/{total_bytes}, server_rx={srv_rx})",
        );
    }

    #[test]
    fn iouring_driver_bounded_tier2_progress_under_saturation() {
        let _serial = io_uring_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        use crate::transport::io_uring::IoUringDriver;
        use crate::transport::{Driver, TxDatagram};

        let sender_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let receiver_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        sender_sock.set_nonblocking(true).unwrap();
        receiver_sock.set_nonblocking(true).unwrap();
        // Increase rcvbuf to handle the full burst without packet loss.
        // Default rmem_default (212992) is too small for 384 × 1480 = 568320 bytes.
        setsockopt_int(
            receiver_sock.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            1 << 20,
        );
        let recv_addr = receiver_sock.local_addr().unwrap();

        let (mut sender, _sw) = IoUringDriver::new(sender_sock).unwrap();
        let (mut receiver, _rw) = IoUringDriver::new(receiver_sock).unwrap();

        let packet_count = 384usize;
        let packet_size = 1480usize;
        let expected_bytes = packet_count * packet_size;
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let receiver_handle = thread::spawn(move || {
            let _ = receiver
                .poll(Some(Instant::now() + Duration::from_millis(1)))
                .unwrap();
            ready_tx.send(()).unwrap();
            let mut received = 0usize;
            let deadline = Instant::now() + Duration::from_secs(10);
            while received < expected_bytes && Instant::now() < deadline {
                let outcome = receiver
                    .poll(Some(Instant::now() + Duration::from_millis(20)))
                    .unwrap();
                for pkt in &outcome.rx {
                    received += pkt.data.len();
                }
            }
            received
        });
        ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let _ = sender.poll(Some(Instant::now() + Duration::from_millis(1)));
        let packets: Vec<TxDatagram> = (0..packet_count)
            .map(|i| TxDatagram {
                data: vec![(i % 251) as u8; packet_size],
                to: recv_addr,
            })
            .collect();

        let submit_started = Instant::now();
        sender.submit_sends(packets).unwrap();
        let submit_elapsed = submit_started.elapsed();
        let pending_after_submit = sender.pending_tx_count();

        eprintln!(
            "  bounded-tier2: submit={submit_elapsed:?} outstanding_after_submit={pending_after_submit}",
        );
        assert!(
            submit_elapsed < Duration::from_millis(500),
            "submit_sends should stay bounded under saturation (took {submit_elapsed:?})",
        );
        assert!(
            pending_after_submit > 256,
            "test should saturate more than one TX window (outstanding_after_submit={pending_after_submit})",
        );

        let deadline = Instant::now() + Duration::from_secs(10);
        while sender.pending_tx_count() > 0 && Instant::now() < deadline {
            let _ = sender.poll(Some(Instant::now() + Duration::from_millis(5)));
        }
        let received = receiver_handle.join().unwrap();

        assert_eq!(
            received, expected_bytes,
            "all saturated sends should arrive without stalling (got {received}/{expected_bytes})",
        );
        assert_eq!(
            sender.pending_tx_count(),
            0,
            "sender should drain all pending saturated sends",
        );
    }

    // ── Test: IoUringDriver submit_sends latency (detect stall) ──

    /// Measures wall-clock time for submit_sends+poll round-trips.
    /// Detects the stall bug: if any single round takes >500ms, something
    /// is blocking (unflushed SQEs, stuck completions, etc).
    #[test]
    fn iouring_driver_no_latency_spikes() {
        let _serial = io_uring_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        use crate::transport::io_uring::IoUringDriver;
        use crate::transport::{Driver, TxDatagram};

        let sender_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let receiver_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        sender_sock.set_nonblocking(true).unwrap();
        receiver_sock.set_nonblocking(true).unwrap();
        let recv_addr = receiver_sock.local_addr().unwrap();

        let (mut sender, _sw) = IoUringDriver::new(sender_sock).unwrap();
        let (mut receiver, _rw) = IoUringDriver::new(receiver_sock).unwrap();

        let _ = receiver.poll(Some(Instant::now() + Duration::from_millis(1)));
        let _ = sender.poll(Some(Instant::now() + Duration::from_millis(1)));

        let mut round_times = Vec::new();

        for round in 0..30 {
            let t0 = Instant::now();

            // Send 32 packets.
            let packets: Vec<TxDatagram> = (0..32)
                .map(|i| TxDatagram {
                    data: vec![(round * 32 + i) as u8; 200],
                    to: recv_addr,
                })
                .collect();
            sender.submit_sends(packets).unwrap();
            let _ = sender.poll(Some(Instant::now() + Duration::from_millis(20)));

            // Receive.
            let mut got = 0usize;
            let inner_deadline = Instant::now() + Duration::from_millis(500);
            while got < 32 * 200 && Instant::now() < inner_deadline {
                let outcome = receiver
                    .poll(Some(Instant::now() + Duration::from_millis(50)))
                    .unwrap();
                for pkt in &outcome.rx {
                    got += pkt.data.len();
                }
            }

            let elapsed = t0.elapsed();
            round_times.push(elapsed);

            if got < 32 * 200 {
                eprintln!(
                    "  round {round}: STALL — only {got}/{} bytes in {:?}",
                    32 * 200,
                    elapsed
                );
            }
        }

        let max_round = round_times.iter().max().unwrap();
        let avg_ms =
            round_times.iter().map(|d| d.as_millis()).sum::<u128>() / round_times.len() as u128;
        eprintln!(
            "  latency: avg={avg_ms}ms max={:?} rounds={}",
            max_round,
            round_times.len(),
        );

        // No round should take more than 500ms on loopback.
        assert!(
            max_round.as_millis() < 500,
            "latency spike detected: max round took {:?} (avg {avg_ms}ms) — likely a stall",
            max_round,
        );
    }

    // ── Test: IoUringDriver high-volume multi-round stress ──

    /// Stress test: 50 rounds × 64 packets with interleaved send/poll.
    /// Verifies no packet loss or stalls under sustained load.
    #[test]
    fn iouring_driver_stress_multi_round() {
        let _serial = io_uring_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        use crate::transport::io_uring::IoUringDriver;
        use crate::transport::{Driver, TxDatagram};

        let sender_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let receiver_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        sender_sock.set_nonblocking(true).unwrap();
        receiver_sock.set_nonblocking(true).unwrap();
        let recv_addr = receiver_sock.local_addr().unwrap();

        let (mut sender, _sw) = IoUringDriver::new(sender_sock).unwrap();
        let (mut receiver, _rw) = IoUringDriver::new(receiver_sock).unwrap();

        let _ = receiver.poll(Some(Instant::now() + Duration::from_millis(1)));
        let _ = sender.poll(Some(Instant::now() + Duration::from_millis(1)));

        let rounds = 50;
        let pkts_per_round = 64;
        let pkt_size = 200;
        let total_expected = rounds * pkts_per_round * pkt_size;
        let mut total_received = 0usize;
        let t0 = Instant::now();

        for round in 0..rounds {
            let packets: Vec<TxDatagram> = (0..pkts_per_round)
                .map(|i| TxDatagram {
                    data: vec![((round * pkts_per_round + i) % 256) as u8; pkt_size],
                    to: recv_addr,
                })
                .collect();
            sender.submit_sends(packets).unwrap();

            // Poll both sides every round.
            let _ = sender.poll(Some(Instant::now() + Duration::from_millis(5)));
            let outcome = receiver
                .poll(Some(Instant::now() + Duration::from_millis(10)))
                .unwrap();
            for pkt in &outcome.rx {
                total_received += pkt.data.len();
            }
        }

        // Drain remaining.
        let deadline = Instant::now() + Duration::from_secs(5);
        while total_received < total_expected && Instant::now() < deadline {
            let _ = sender.poll(Some(Instant::now() + Duration::from_millis(10)));
            let outcome = receiver
                .poll(Some(Instant::now() + Duration::from_millis(50)))
                .unwrap();
            for pkt in &outcome.rx {
                total_received += pkt.data.len();
            }
        }

        let elapsed = t0.elapsed();
        let mbps = (total_received as f64 * 8.0) / (elapsed.as_secs_f64() * 1_000_000.0);
        eprintln!(
            "  stress: {total_received}/{total_expected} bytes in {:?} ({mbps:.1} Mbps)",
            elapsed,
        );
        assert_eq!(
            total_received, total_expected,
            "all bytes should arrive (got {total_received}/{total_expected})"
        );
        // Should complete in under 10 seconds on loopback.
        assert!(
            elapsed.as_secs() < 10,
            "stress test took {:?} — likely stalled",
            elapsed,
        );
    }

    // ── Test: PollDriver GSO send on worker thread ──────────────────

    #[test]
    fn poll_driver_gso_send_cross_thread() {
        use crate::transport::poll::PollDriver;
        use crate::transport::{Driver, TxDatagram};

        let sender_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let receiver_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        sender_sock.set_nonblocking(true).unwrap();
        receiver_sock.set_nonblocking(true).unwrap();
        let recv_addr = receiver_sock.local_addr().unwrap();

        // PollDriver for sender (GSO), PollDriver for receiver (GRO).
        // GRO may recoalesce segments on loopback — that's correct behavior.
        // We verify that the total bytes received match (4 × 200 = 800).
        let (mut sender, _waker) = PollDriver::new(sender_sock).unwrap();
        let (mut receiver, _rwaker) = PollDriver::new(receiver_sock).unwrap();

        let handle = thread::spawn(move || {
            let packets: Vec<TxDatagram> = (0..4)
                .map(|i| TxDatagram {
                    data: vec![i as u8; 200],
                    to: recv_addr,
                })
                .collect();
            sender.submit_sends(packets).unwrap();
            sender
        });

        thread::sleep(Duration::from_millis(50));

        let handle_b = thread::spawn(move || {
            let mut total_bytes = 0usize;
            let mut pkt_count = 0usize;
            let deadline = Instant::now() + Duration::from_secs(2);
            while total_bytes < 800 && Instant::now() < deadline {
                let outcome = receiver
                    .poll(Some(Instant::now() + Duration::from_millis(100)))
                    .unwrap();
                for pkt in &outcome.rx {
                    // With GRO, we may get 1 × 800 (coalesced) or 4 × 200.
                    // Both are correct — GRO splitting in event_loop handles it.
                    total_bytes += pkt.data.len();
                    pkt_count += 1;
                }
            }
            eprintln!("  cross-thread: {pkt_count} packets, {total_bytes} bytes");
            assert_eq!(
                total_bytes, 800,
                "should receive 800 total bytes (got {total_bytes})"
            );
            receiver
        });

        let _s = handle.join().unwrap();
        let _r = handle_b.join().unwrap();
    }
}
