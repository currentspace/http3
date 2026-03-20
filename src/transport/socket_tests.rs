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
    use std::thread;
    use std::time::{Duration, Instant};

    // ── Constants matching socket.rs ─────────────────────────────────

    const SOL_UDP: libc::c_int = 17;
    const UDP_SEGMENT: libc::c_int = 103;
    const UDP_GRO: libc::c_int = 104;

    // ── Helpers ──────────────────────────────────────────────────────

    fn setsockopt_int(fd: libc::c_int, level: libc::c_int, opt: libc::c_int, val: libc::c_int) -> libc::c_int {
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

    fn getsockopt_int(fd: libc::c_int, level: libc::c_int, opt: libc::c_int) -> Result<libc::c_int, std::io::Error> {
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
        if rc == 0 { Ok(val) } else { Err(std::io::Error::last_os_error()) }
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
                    sin_addr: libc::in_addr { s_addr: u32::from_ne_bytes(a.ip().octets()) },
                    sin_zero: [0; 8],
                };
                unsafe { std::ptr::write(&mut storage as *mut _ as *mut _, sin); }
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
        let mut iov = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() };
        let mut addr: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
        let mut hdr: libc::mmsghdr = unsafe { std::mem::zeroed() };
        hdr.msg_hdr.msg_iov = &mut iov;
        hdr.msg_hdr.msg_iovlen = 1;
        hdr.msg_hdr.msg_name = (&mut addr as *mut libc::sockaddr_storage).cast();
        hdr.msg_hdr.msg_namelen = std::mem::size_of::<libc::sockaddr_storage>() as _;

        let rc = unsafe { libc::recvmmsg(fd, &mut hdr, 1, libc::MSG_DONTWAIT, std::ptr::null_mut()) };
        if rc > 0 { hdr.msg_len as usize } else { 0 }
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
        let ok = thread::spawn(move || {
            sendmmsg_one(sender_fd, &recv_clone)
        }).join().unwrap();

        assert!(ok, "sendmmsg should succeed even with UDP_SEGMENT=0 on socket");

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

        assert_eq!(n, 200, "recvmmsg on worker thread should get the packet (GRO enabled: {gro_enabled})");
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

        assert_eq!(n, 150, "recvmmsg should deliver payload even with IP_PKTINFO and no cmsg buffer");
    }

    // ── Test: PollDriver created on main thread, roundtrip on worker ─

    #[test]
    fn poll_driver_cross_thread_roundtrip() {
        use crate::transport::{Driver, TxDatagram};
        use crate::transport::poll::PollDriver;

        let sock_a = UdpSocket::bind("127.0.0.1:0").unwrap();
        let sock_b = UdpSocket::bind("127.0.0.1:0").unwrap();
        sock_a.set_nonblocking(true).unwrap();
        sock_b.set_nonblocking(true).unwrap();
        let b_addr = sock_b.local_addr().unwrap();
        let a_addr = sock_a.local_addr().unwrap();

        // Create PollDriver on THIS thread (simulating NodeJS main thread).
        let (mut driver_a, _waker_a) = PollDriver::new(sock_a).unwrap();
        let (mut driver_b, _waker_b) = PollDriver::new(sock_b).unwrap();

        eprintln!("poll_driver_cross_thread_roundtrip: created on {:?}", thread::current().id());

        // Move driver_a to a worker thread, send from there to b_addr.
        let handle = thread::spawn(move || {
            eprintln!("  worker A on {:?}", thread::current().id());
            let pkt = TxDatagram { data: vec![0xAB; 100], to: b_addr };
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
                let outcome = driver_b.poll(Some(Instant::now() + Duration::from_millis(100))).unwrap();
                total_rx += outcome.rx.len();
                if total_rx > 0 {
                    let pkt = &outcome.rx[0];
                    eprintln!("  rx: len={} peer={} local={} gro={:?}",
                        pkt.data.len(), pkt.peer, pkt.local, pkt.segment_size);
                    assert_eq!(pkt.data.len(), 100);
                    assert_eq!(pkt.peer, a_addr);
                }
            }
            assert!(total_rx > 0, "should receive at least one packet on worker thread");
            driver_b
        });

        let _a = handle.join().unwrap();
        let _b = handle_b.join().unwrap();
    }

    // ── Test: raw GSO sendmsg with our build_gso_cmsg ─────────────

    #[test]
    fn build_gso_cmsg_produces_valid_segmentation() {
        use crate::transport::socket::{build_gso_cmsg, SOL_UDP, UDP_SEGMENT};

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
            libc::getsockopt(sender_fd, SOL_UDP, UDP_SEGMENT,
                &mut gso_val as *mut _ as *mut libc::c_void, &mut gso_len)
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

        eprintln!("cmsg_len={cmsg_len} sizeof(cmsghdr)={} cmsg_data_offset={}",
            std::mem::size_of::<libc::cmsghdr>(),
            crate::transport::socket::cmsg_data_offset_for_test());

        // Build sendmsg manually.
        let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
        let addrlen = match recv_addr {
            std::net::SocketAddr::V4(a) => {
                let sin = libc::sockaddr_in {
                    sin_family: libc::AF_INET as _,
                    sin_port: a.port().to_be(),
                    sin_addr: libc::in_addr { s_addr: u32::from_ne_bytes(a.ip().octets()) },
                    sin_zero: [0; 8],
                };
                unsafe { std::ptr::write(&mut storage as *mut _ as *mut _, sin); }
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
        eprintln!("sendmsg rc={rc} errno={}", if rc < 0 { std::io::Error::last_os_error().to_string() } else { "ok".into() });
        assert!(rc > 0, "sendmsg with GSO cmsg should succeed (rc={rc})");

        // Now receive — should get 4 separate 100-byte datagrams, not 1 × 400.
        thread::sleep(Duration::from_millis(50));
        let recv_fd = receiver.as_raw_fd();
        let mut packets = Vec::new();
        for _ in 0..10 {
            let n = recvmmsg_one(recv_fd);
            if n == 0 { break; }
            packets.push(n);
        }

        eprintln!("received packets: {packets:?}");
        assert_eq!(packets.len(), 4, "GSO should segment into 4 packets, got {packets:?}");
        assert!(packets.iter().all(|&n| n == 100), "each segment should be 100 bytes, got {packets:?}");
    }

    // ── Test: GSO after set_pktinfo + enable_gro ──────────────────

    #[test]
    fn gso_sendmsg_after_pktinfo_and_gro() {
        use crate::transport::socket::{build_gso_cmsg, set_pktinfo, enable_gro, SOL_UDP, UDP_SEGMENT};

        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        sender.set_nonblocking(true).unwrap();
        receiver.set_nonblocking(true).unwrap();
        let sender_fd = sender.as_raw_fd();
        let recv_addr = receiver.local_addr().unwrap();

        // Check GSO support.
        let mut gso_val: libc::c_int = 0;
        let mut gso_len = std::mem::size_of_val(&gso_val) as libc::socklen_t;
        let gso_ok = unsafe { libc::getsockopt(sender_fd, SOL_UDP, UDP_SEGMENT, &mut gso_val as *mut _ as *mut _, &mut gso_len) };
        if gso_ok != 0 { eprintln!("GSO not supported, skipping"); return; }

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
                let sin = libc::sockaddr_in { sin_family: libc::AF_INET as _, sin_port: a.port().to_be(), sin_addr: libc::in_addr { s_addr: u32::from_ne_bytes(a.ip().octets()) }, sin_zero: [0; 8] };
                unsafe { std::ptr::write(&mut storage as *mut _ as *mut _, sin); }
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t
            }
            _ => panic!("v4 only"),
        };

        let mut iov = libc::iovec { iov_base: payload.as_ptr() as *mut _, iov_len: payload.len() };
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
            if n == 0 { break; }
            packets.push(n);
        }

        eprintln!("GSO after pktinfo+gro: received packets: {packets:?}");
        assert_eq!(packets.len(), 4, "GSO should still segment after pktinfo+gro, got {packets:?}");
    }

    // ── Test: Vec-based mmsghdr GSO (mimics send_batch_gso layout) ─

    #[test]
    fn gso_via_vec_mmsghdr_layout() {
        use crate::transport::socket::{build_gso_cmsg, SOL_UDP, UDP_SEGMENT};

        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        sender.set_nonblocking(true).unwrap();
        receiver.set_nonblocking(true).unwrap();
        let sender_fd = sender.as_raw_fd();
        let recv_addr = receiver.local_addr().unwrap();

        let mut gso_val: libc::c_int = 0;
        let mut gso_len = std::mem::size_of_val(&gso_val) as libc::socklen_t;
        if unsafe { libc::getsockopt(sender_fd, SOL_UDP, UDP_SEGMENT, &mut gso_val as *mut _ as *mut _, &mut gso_len) } != 0 {
            eprintln!("GSO not supported, skipping"); return;
        }

        // Mimic send_batch_gso: Vec<mmsghdr>, Vec<iovec>, Vec<sockaddr_storage>, Vec<[u8;32]>
        let count = 1usize;
        let mut tx_hdrs: Vec<libc::mmsghdr> = vec![unsafe { std::mem::zeroed() }; count];
        let mut tx_iovs: Vec<libc::iovec> = vec![libc::iovec { iov_base: std::ptr::null_mut(), iov_len: 0 }; count];
        let mut tx_addrs: Vec<libc::sockaddr_storage> = vec![unsafe { std::mem::zeroed() }; count];
        let mut tx_cmsg_bufs: Vec<[u8; 32]> = vec![[0u8; 32]; count];

        let payload = vec![0xABu8; 400]; // 4 × 100 coalesced

        // Fill address
        match recv_addr {
            std::net::SocketAddr::V4(a) => {
                let sin = libc::sockaddr_in { sin_family: libc::AF_INET as _, sin_port: a.port().to_be(), sin_addr: libc::in_addr { s_addr: u32::from_ne_bytes(a.ip().octets()) }, sin_zero: [0; 8] };
                unsafe { std::ptr::write(&mut tx_addrs[0] as *mut _ as *mut _, sin); }
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

        eprintln!("Vec layout: controllen={cmsg_len} cmsg_bytes={:02x?}", &tx_cmsg_bufs[0][..cmsg_len]);

        let rc = unsafe { libc::sendmsg(sender_fd, &tx_hdrs[0].msg_hdr, 0) };
        eprintln!("sendmsg rc={rc}");
        assert!(rc > 0);

        thread::sleep(Duration::from_millis(50));
        let recv_fd = receiver.as_raw_fd();
        let mut packets = Vec::new();
        for _ in 0..10 {
            let n = recvmmsg_one(recv_fd);
            if n == 0 { break; }
            packets.push(n);
        }

        eprintln!("Vec layout GSO: received {packets:?}");
        assert_eq!(packets.len(), 4, "Vec-based GSO should segment into 4, got {packets:?}");
    }

    // ── Test: Vec layout + pktinfo + gro (exact PollDriver conditions) ─

    #[test]
    fn gso_vec_layout_with_pktinfo_and_gro() {
        use crate::transport::socket::{build_gso_cmsg, set_pktinfo, enable_gro, SOL_UDP, UDP_SEGMENT};

        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        sender.set_nonblocking(true).unwrap();
        receiver.set_nonblocking(true).unwrap();
        let sender_fd = sender.as_raw_fd();
        let recv_addr = receiver.local_addr().unwrap();

        let mut gso_val: libc::c_int = 0;
        let mut gso_len = std::mem::size_of_val(&gso_val) as libc::socklen_t;
        if unsafe { libc::getsockopt(sender_fd, SOL_UDP, UDP_SEGMENT, &mut gso_val as *mut _ as *mut _, &mut gso_len) } != 0 {
            eprintln!("GSO not supported, skipping"); return;
        }

        // Apply SAME socket options as PollDriver::new
        set_pktinfo(&sender);
        enable_gro(&sender);

        // Vec-based layout (exactly like send_batch_gso)
        let count = 1;
        let mut tx_hdrs: Vec<libc::mmsghdr> = vec![unsafe { std::mem::zeroed() }; count];
        let mut tx_iovs: Vec<libc::iovec> = vec![libc::iovec { iov_base: std::ptr::null_mut(), iov_len: 0 }; count];
        let mut tx_addrs: Vec<libc::sockaddr_storage> = vec![unsafe { std::mem::zeroed() }; count];
        let mut tx_cmsg_bufs: Vec<[u8; 32]> = vec![[0u8; 32]; count];

        let payload = vec![0xABu8; 800]; // 4 × 200

        match recv_addr {
            std::net::SocketAddr::V4(a) => {
                let sin = libc::sockaddr_in { sin_family: libc::AF_INET as _, sin_port: a.port().to_be(), sin_addr: libc::in_addr { s_addr: u32::from_ne_bytes(a.ip().octets()) }, sin_zero: [0; 8] };
                unsafe { std::ptr::write(&mut tx_addrs[0] as *mut _ as *mut _, sin); }
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
        let rc_mmsg = unsafe {
            libc::sendmmsg(sender_fd, tx_hdrs.as_mut_ptr(), 1, libc::MSG_DONTWAIT)
        };
        eprintln!("gso_vec_layout_with_pktinfo_and_gro: sendmmsg(MSG_DONTWAIT) rc={rc_mmsg} msg_len={}", tx_hdrs[0].msg_len);
        assert_eq!(rc_mmsg, 1, "sendmmsg should send 1 message");

        thread::sleep(Duration::from_millis(50));
        let recv_fd = receiver.as_raw_fd();
        let mut packets = Vec::new();
        for _ in 0..10 {
            let n = recvmmsg_one(recv_fd);
            if n == 0 { break; }
            packets.push(n);
        }

        eprintln!("received: {packets:?}");
        assert_eq!(packets.len(), 4, "should get 4 segments, got {packets:?}");
    }

    // ── Test: group_for_gso produces correct batch data ───────────

    #[test]
    fn group_for_gso_coalesced_send() {
        use crate::transport::{TxDatagram, group_for_gso};
        use crate::transport::socket::{build_gso_cmsg, set_pktinfo, enable_gro, SOL_UDP, UDP_SEGMENT};

        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        sender.set_nonblocking(true).unwrap();
        receiver.set_nonblocking(true).unwrap();
        let sender_fd = sender.as_raw_fd();
        let recv_addr = receiver.local_addr().unwrap();

        let mut gso_val: libc::c_int = 0;
        let mut gso_len = std::mem::size_of_val(&gso_val) as libc::socklen_t;
        if unsafe { libc::getsockopt(sender_fd, SOL_UDP, UDP_SEGMENT, &mut gso_val as *mut _ as *mut _, &mut gso_len) } != 0 {
            return;
        }

        set_pktinfo(&sender);
        enable_gro(&sender);

        // Build packets like the test does
        let packets: Vec<TxDatagram> = (0..4)
            .map(|i| TxDatagram { data: vec![i as u8; 200], to: recv_addr })
            .collect();
        let batches = group_for_gso(packets);
        assert_eq!(batches.len(), 1, "4 same-size should coalesce into 1 batch");
        let batch = &batches[0];
        eprintln!("batch: data.len()={} seg_size={}", batch.data.len(), batch.segment_size);
        assert_eq!(batch.data.len(), 800);
        assert_eq!(batch.segment_size, 200);

        // Now send with Vec layout
        let mut tx_hdrs: Vec<libc::mmsghdr> = vec![unsafe { std::mem::zeroed() }; 1];
        let mut tx_iovs: Vec<libc::iovec> = vec![libc::iovec { iov_base: std::ptr::null_mut(), iov_len: 0 }; 1];
        let mut tx_addrs: Vec<libc::sockaddr_storage> = vec![unsafe { std::mem::zeroed() }; 1];
        let mut tx_cmsg_bufs: Vec<[u8; 32]> = vec![[0u8; 32]; 1];

        match recv_addr {
            std::net::SocketAddr::V4(a) => {
                let sin = libc::sockaddr_in { sin_family: libc::AF_INET as _, sin_port: a.port().to_be(), sin_addr: libc::in_addr { s_addr: u32::from_ne_bytes(a.ip().octets()) }, sin_zero: [0; 8] };
                unsafe { std::ptr::write(&mut tx_addrs[0] as *mut _ as *mut _, sin); }
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
            if n == 0 { break; }
            packets_received.push(n);
        }

        eprintln!("received: {packets_received:?}");
        assert_eq!(packets_received.len(), 4, "should get 4 segments via group_for_gso, got {packets_received:?}");
    }

    // ── Test: PollDriver GSO send SAME thread ─────────────────────

    #[test]
    fn poll_driver_gso_send_same_thread() {
        use crate::transport::{Driver, TxDatagram};
        use crate::transport::poll::PollDriver;

        let sender_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let receiver_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        sender_sock.set_nonblocking(true).unwrap();
        receiver_sock.set_nonblocking(true).unwrap();
        let recv_addr = receiver_sock.local_addr().unwrap();

        // Use PollDriver for SENDER (this calls set_pktinfo + enable_gro + probe_gso)
        let (mut sender, _waker) = PollDriver::new(sender_sock).unwrap();
        // Use RAW receiver (no PollDriver, no GRO) to isolate sender-side GSO
        let recv_fd = receiver_sock.as_raw_fd();

        // Send 4 same-sized packets — ALL ON THE SAME THREAD.
        let packets: Vec<TxDatagram> = (0..4)
            .map(|i| TxDatagram { data: vec![i as u8; 200], to: recv_addr })
            .collect();
        sender.submit_sends(packets).unwrap();
        eprintln!("  submit_sends done, checking receiver...");

        thread::sleep(Duration::from_millis(50));

        // Receive with raw socket (no GRO) to verify GSO segmentation.
        thread::sleep(Duration::from_millis(50));
        let mut packets_rx = Vec::new();
        for _ in 0..10 {
            let n = recvmmsg_one(recv_fd);
            if n == 0 { break; }
            packets_rx.push(n);
        }
        eprintln!("  raw receiver: {packets_rx:?}");
        assert_eq!(packets_rx.len(), 4, "GSO should segment into 4 (got {packets_rx:?})");
    }

    // ── Test: IoUringDriver GSO send + receive (GRO) ──────────────

    #[test]
    fn iouring_driver_gso_roundtrip() {
        use crate::transport::{Driver, TxDatagram};
        use crate::transport::io_uring::IoUringDriver;

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
            .map(|i| TxDatagram { data: vec![i as u8; 200], to: recv_addr })
            .collect();
        sender.submit_sends(packets).unwrap();

        // Poll sender to flush SQEs.
        let _ = sender.poll(Some(Instant::now() + Duration::from_millis(50)));

        // Receive.
        let mut total_bytes = 0usize;
        let mut pkt_count = 0usize;
        let deadline = Instant::now() + Duration::from_secs(2);
        while total_bytes < 800 && Instant::now() < deadline {
            let outcome = receiver.poll(Some(Instant::now() + Duration::from_millis(100))).unwrap();
            for pkt in &outcome.rx {
                eprintln!("  iouring rx: len={} gro={:?}", pkt.data.len(), pkt.segment_size);
                total_bytes += pkt.data.len();
                pkt_count += 1;
            }
        }
        eprintln!("  iouring GSO roundtrip: {pkt_count} pkts, {total_bytes} bytes");
        assert_eq!(total_bytes, 800, "should receive 800 total bytes (got {total_bytes})");
    }

    // ── Test: IoUringDriver GSO multi-round (regression for round 2+ failures)

    #[test]
    fn iouring_driver_gso_multi_round() {
        use crate::transport::{Driver, TxDatagram};
        use crate::transport::io_uring::IoUringDriver;

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
                .map(|i| TxDatagram { data: vec![(round * 16 + i) as u8; 200], to: recv_addr })
                .collect();
            sender.submit_sends(packets).unwrap();
            let _ = sender.poll(Some(Instant::now() + Duration::from_millis(50)));

            let mut total_bytes = 0usize;
            let deadline = Instant::now() + Duration::from_secs(2);
            while total_bytes < 16 * 200 && Instant::now() < deadline {
                let outcome = receiver.poll(Some(Instant::now() + Duration::from_millis(100))).unwrap();
                for pkt in &outcome.rx {
                    total_bytes += pkt.data.len();
                }
            }
            eprintln!("  round {round}: {total_bytes}/3200 bytes");
            assert_eq!(total_bytes, 3200, "round {round}: should get 3200 bytes (got {total_bytes})");
        }
    }

    // ── Test: PollDriver GSO send on worker thread ──────────────────

    #[test]
    fn poll_driver_gso_send_cross_thread() {
        use crate::transport::{Driver, TxDatagram};
        use crate::transport::poll::PollDriver;

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
                .map(|i| TxDatagram { data: vec![i as u8; 200], to: recv_addr })
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
                let outcome = receiver.poll(Some(Instant::now() + Duration::from_millis(100))).unwrap();
                for pkt in &outcome.rx {
                    // With GRO, we may get 1 × 800 (coalesced) or 4 × 200.
                    // Both are correct — GRO splitting in event_loop handles it.
                    total_bytes += pkt.data.len();
                    pkt_count += 1;
                }
            }
            eprintln!("  cross-thread: {pkt_count} packets, {total_bytes} bytes");
            assert_eq!(total_bytes, 800, "should receive 800 total bytes (got {total_bytes})");
            receiver
        });

        let _s = handle.join().unwrap();
        let _r = handle_b.join().unwrap();
    }
}
