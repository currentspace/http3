fn main() {
    if let Err(err) = http3::profile::mock_sustained::cli_main() {
        eprintln!("{err}");
        std::process::exit(1);
    }
}
