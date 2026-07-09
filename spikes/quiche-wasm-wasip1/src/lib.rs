pub fn make_config() -> quiche::Config {
    let mut c = quiche::Config::new(quiche::PROTOCOL_VERSION).unwrap();
    c.set_application_protos(&[b"h3"]).unwrap();
    c
}
