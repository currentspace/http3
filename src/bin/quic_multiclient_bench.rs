fn main() {
    if let Err(err) = http3::profile::multiclient::cli_main() {
        eprintln!("{err}");
        std::process::exit(1);
    }
}
