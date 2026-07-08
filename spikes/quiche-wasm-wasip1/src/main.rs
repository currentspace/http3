fn main() {
    let mut config = quiche::Config::new(quiche::PROTOCOL_VERSION).unwrap();
    config.set_application_protos(&[b"h3"]).unwrap();
    config.verify_peer(false);
    let scid = quiche::ConnectionId::from_ref(&[0xba; 16]);
    let local: std::net::SocketAddr = "127.0.0.1:4000".parse().unwrap();
    let peer: std::net::SocketAddr = "127.0.0.1:4433".parse().unwrap();
    let mut conn =
        quiche::connect(Some("example.com"), &scid, local, peer, &mut config).unwrap();
    let mut out = [0u8; 1500];
    let (n, info) = conn.send(&mut out).unwrap();
    println!("initial packet: {n} bytes, to {:?}", info.to);
    println!("timeout: {:?}", conn.timeout());
}
