#![allow(clippy::missing_errors_doc)]

use quiche::h3::NameValue;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const MAX_DATAGRAM_SIZE: usize = 1350;

#[derive(Clone)]
struct LoadConfig {
    addr: String,
    host: String,
    path: String,
    clients: usize,
    requests_per_client: usize,
    timeout_ms: u64,
    verify_peer: bool,
}

#[derive(Default, Clone, Copy)]
struct RequestOutcome {
    ok: bool,
    latency_ms: f64,
}

fn parse_args() -> Result<LoadConfig, String> {
    let mut cfg = LoadConfig {
        addr: "127.0.0.1:8443".to_string(),
        host: "localhost".to_string(),
        path: "/".to_string(),
        clients: 10,
        requests_per_client: 20,
        timeout_ms: 5000,
        verify_peer: false,
    };

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let value = args
            .next()
            .ok_or_else(|| format!("missing value for argument {arg}"))?;
        match arg.as_str() {
            "--addr" => cfg.addr = value,
            "--host" => cfg.host = value,
            "--path" => cfg.path = value,
            "--clients" => {
                cfg.clients = value
                    .parse::<usize>()
                    .map_err(|e| format!("invalid --clients value: {e}"))?;
            }
            "--requests-per-client" => {
                cfg.requests_per_client = value
                    .parse::<usize>()
                    .map_err(|e| format!("invalid --requests-per-client value: {e}"))?;
            }
            "--timeout-ms" => {
                cfg.timeout_ms = value
                    .parse::<u64>()
                    .map_err(|e| format!("invalid --timeout-ms value: {e}"))?;
            }
            "--verify-peer" => {
                cfg.verify_peer = matches!(value.as_str(), "1" | "true" | "yes");
            }
            _ => {
                return Err(format!(
                    "unknown argument {arg}. supported: --addr --host --path --clients --requests-per-client --timeout-ms --verify-peer",
                ));
            }
        }
    }

    Ok(cfg)
}

fn percentile(sorted: &[f64], pct: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let idx = ((pct / 100.0) * ((sorted.len() - 1) as f64)).round() as usize;
    sorted[idx]
}

fn build_client_config(verify_peer: bool) -> Result<quiche::Config, String> {
    let mut config =
        quiche::Config::new(quiche::PROTOCOL_VERSION).map_err(|e| format!("config error: {e}"))?;
    config
        .set_application_protos(quiche::h3::APPLICATION_PROTOCOL)
        .map_err(|e| format!("application proto error: {e}"))?;
    config.set_max_idle_timeout(5000);
    config.set_max_recv_udp_payload_size(MAX_DATAGRAM_SIZE);
    config.set_max_send_udp_payload_size(MAX_DATAGRAM_SIZE);
    config.set_initial_max_data(10_000_000);
    config.set_initial_max_stream_data_bidi_local(1_000_000);
    config.set_initial_max_stream_data_bidi_remote(1_000_000);
    config.set_initial_max_stream_data_uni(1_000_000);
    config.set_initial_max_streams_bidi(1000);
    config.set_initial_max_streams_uni(1000);
    config.set_disable_active_migration(true);
    config.verify_peer(verify_peer);
    Ok(config)
}

fn run_single_request(
    server_addr: SocketAddr,
    host: &str,
    path: &str,
    timeout: Duration,
    verify_peer: bool,
    scid_counter: &AtomicU64,
) -> Result<RequestOutcome, String> {
    let bind_addr = if server_addr.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    let socket = UdpSocket::bind(bind_addr).map_err(|e| format!("bind error: {e}"))?;
    socket
        .set_nonblocking(true)
        .map_err(|e| format!("set_nonblocking error: {e}"))?;
    let local_addr = socket
        .local_addr()
        .map_err(|e| format!("local_addr error: {e}"))?;

    let mut config = build_client_config(verify_peer)?;
    let h3_config = quiche::h3::Config::new().map_err(|e| format!("h3 config error: {e}"))?;

    let mut scid = [0_u8; quiche::MAX_CONN_ID_LEN];
    let id = scid_counter.fetch_add(1, Ordering::Relaxed).to_le_bytes();
    let id_len = id.len().min(scid.len());
    scid[..id_len].copy_from_slice(&id[..id_len]);
    let scid_ref = quiche::ConnectionId::from_ref(&scid);

    let mut conn = quiche::connect(Some(host), &scid_ref, local_addr, server_addr, &mut config)
        .map_err(|e| format!("connect error: {e}"))?;

    let started = Instant::now();
    let mut h3_conn: Option<quiche::h3::Connection> = None;
    let mut request_stream_id: Option<u64> = None;
    let mut status_code: u16 = 0;
    let mut out = vec![0_u8; MAX_DATAGRAM_SIZE];
    let mut recv = vec![0_u8; 65535];

    loop {
        if started.elapsed() > timeout {
            return Err("request timed out".to_string());
        }

        if conn.timeout() == Some(Duration::ZERO) {
            conn.on_timeout();
        }

        loop {
            match conn.send(&mut out) {
                Ok((len, send_info)) => {
                    socket
                        .send_to(&out[..len], send_info.to)
                        .map_err(|e| format!("send_to error: {e}"))?;
                }
                Err(quiche::Error::Done) => break,
                Err(e) => return Err(format!("conn.send error: {e}")),
            }
        }

        loop {
            match socket.recv_from(&mut recv) {
                Ok((len, from)) => {
                    let recv_info = quiche::RecvInfo {
                        from,
                        to: local_addr,
                    };
                    match conn.recv(&mut recv[..len], recv_info) {
                        Ok(_) | Err(quiche::Error::Done) => {}
                        Err(e) => return Err(format!("conn.recv error: {e}")),
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(format!("socket recv error: {e}")),
            }
        }

        if conn.is_established() && h3_conn.is_none() {
            let created = quiche::h3::Connection::with_transport(&mut conn, &h3_config)
                .map_err(|e| format!("h3 with_transport error: {e}"))?;
            h3_conn = Some(created);
        }

        if let Some(h3) = h3_conn.as_mut() {
            if request_stream_id.is_none() {
                let req_headers = vec![
                    quiche::h3::Header::new(b":method", b"GET"),
                    quiche::h3::Header::new(b":scheme", b"https"),
                    quiche::h3::Header::new(b":authority", host.as_bytes()),
                    quiche::h3::Header::new(b":path", path.as_bytes()),
                ];
                let stream_id = h3
                    .send_request(&mut conn, &req_headers, true)
                    .map_err(|e| format!("send_request error: {e}"))?;
                request_stream_id = Some(stream_id);
            }

            loop {
                match h3.poll(&mut conn) {
                    Ok((stream_id, quiche::h3::Event::Headers { list, .. })) => {
                        if Some(stream_id) != request_stream_id {
                            continue;
                        }
                        for header in &list {
                            if header.name() == b":status" {
                                let status_text = String::from_utf8_lossy(header.value());
                                status_code = status_text.parse::<u16>().unwrap_or(0);
                            }
                        }
                    }
                    Ok((stream_id, quiche::h3::Event::Data)) => {
                        if Some(stream_id) != request_stream_id {
                            continue;
                        }
                        let mut body_buf = [0_u8; 4096];
                        loop {
                            match h3.recv_body(&mut conn, stream_id, &mut body_buf) {
                                Ok(_) => {}
                                Err(quiche::h3::Error::Done) => break,
                                Err(e) => return Err(format!("recv_body error: {e}")),
                            }
                        }
                    }
                    Ok((stream_id, quiche::h3::Event::Finished)) => {
                        if Some(stream_id) == request_stream_id {
                            let code = if status_code == 0 { 200 } else { status_code };
                            return Ok(RequestOutcome {
                                ok: (200..300).contains(&code),
                                latency_ms: started.elapsed().as_secs_f64() * 1000.0,
                            });
                        }
                    }
                    Ok((_stream_id, _event)) => {}
                    Err(quiche::h3::Error::Done) => break,
                    Err(e) => return Err(format!("h3.poll error: {e}")),
                }
            }
        }

        if conn.is_closed() {
            return Err("connection closed before response finished".to_string());
        }

        thread::sleep(Duration::from_millis(1));
    }
}

fn resolve_addr(input: &str) -> Result<SocketAddr, String> {
    input
        .to_socket_addrs()
        .map_err(|e| format!("address parse error: {e}"))?
        .next()
        .ok_or_else(|| "address resolution returned no results".to_string())
}

fn main() {
    let cfg = match parse_args() {
        Ok(c) => c,
        Err(err) => {
            eprintln!("{err}");
            std::process::exit(2);
        }
    };

    let server_addr = match resolve_addr(&cfg.addr) {
        Ok(addr) => addr,
        Err(err) => {
            eprintln!("{err}");
            std::process::exit(2);
        }
    };

    let total_requests = cfg.clients * cfg.requests_per_client;
    println!(
        "rust-h3-load target={} host={} path={} clients={} requests_per_client={} total={}",
        cfg.addr, cfg.host, cfg.path, cfg.clients, cfg.requests_per_client, total_requests
    );

    let started = Instant::now();
    let (tx, rx) = mpsc::channel::<RequestOutcome>();
    let scid_counter = AtomicU64::new(1);

    thread::scope(|scope| {
        for _ in 0..cfg.clients {
            let tx = tx.clone();
            let host = cfg.host.clone();
            let path = cfg.path.clone();
            let timeout = Duration::from_millis(cfg.timeout_ms);
            let verify_peer = cfg.verify_peer;
            let requests = cfg.requests_per_client;
            let server = server_addr;
            let counter_ref = &scid_counter;

            scope.spawn(move || {
                for _ in 0..requests {
                    let outcome = match run_single_request(
                        server,
                        &host,
                        &path,
                        timeout,
                        verify_peer,
                        counter_ref,
                    ) {
                        Ok(result) => result,
                        Err(err) => {
                            eprintln!("request failed: {err}");
                            RequestOutcome {
                                ok: false,
                                latency_ms: timeout.as_secs_f64() * 1000.0,
                            }
                        }
                    };
                    if tx.send(outcome).is_err() {
                        return;
                    }
                }
            });
        }
    });
    drop(tx);

    let mut total_ok = 0usize;
    let mut total_fail = 0usize;
    let mut latencies_ms: Vec<f64> = Vec::with_capacity(total_requests);

    for outcome in rx.iter().take(total_requests) {
        if outcome.ok {
            total_ok += 1;
        } else {
            total_fail += 1;
        }
        latencies_ms.push(outcome.latency_ms);
    }

    latencies_ms.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let elapsed = started.elapsed().as_secs_f64();
    let rps = (total_ok + total_fail) as f64 / elapsed.max(1e-9);
    let avg = if latencies_ms.is_empty() {
        f64::NAN
    } else {
        latencies_ms.iter().sum::<f64>() / (latencies_ms.len() as f64)
    };

    println!("elapsed_sec={:.3}", elapsed);
    println!("rps={:.2}", rps);
    println!("success={}", total_ok);
    println!("failures={}", total_fail);
    println!("avg_ms={:.3}", avg);
    println!("p50_ms={:.3}", percentile(&latencies_ms, 50.0));
    println!("p95_ms={:.3}", percentile(&latencies_ms, 95.0));
    println!("p99_ms={:.3}", percentile(&latencies_ms, 99.0));

    if total_fail > 0 {
        std::process::exit(1);
    }
}
