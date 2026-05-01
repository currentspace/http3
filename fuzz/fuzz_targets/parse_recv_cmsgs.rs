#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|control: &[u8]| {
    http3::fuzz_exports::parse_recv_cmsgs(control);
});
