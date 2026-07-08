fn main() {
    // `napi-build` is an optional build-dependency gated on the `node-api`
    // feature (see Cargo.toml: `node-api = ["dep:napi", "dep:napi-derive",
    // "dep:napi-build"]`), so `napi_build::setup()` only resolves to a
    // real crate when that feature is active — hence the `#[cfg]` here
    // rather than a plain runtime `if` (a runtime check alone can't save
    // us from a "cannot find crate" compile error when the optional dep
    // isn't pulled in). Inside that already-gated branch we additionally
    // check `CARGO_FEATURE_NODE_API` (the env var Cargo sets for build
    // scripts, screaming-snake-case per feature) as a second, defensive
    // signal, so a future non-node-api build of this crate (e.g.
    // wasm32-wasip1 with `--no-default-features --features wasm-abi`)
    // never emits N-API link args even under an unexpected feature
    // combination.
    #[cfg(feature = "node-api")]
    {
        if std::env::var_os("CARGO_FEATURE_NODE_API").is_some() {
            napi_build::setup();
        }
    }
}
