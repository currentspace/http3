#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    http3::fuzz_exports::pending_write_state(data);
});
