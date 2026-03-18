fn main() {
    if let Err(err) = http3::profile::quiche_direct::cli_main() {
        eprintln!("{err}");
        std::process::exit(1);
    }
}
