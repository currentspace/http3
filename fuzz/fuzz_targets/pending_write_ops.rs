#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;

#[derive(Arbitrary, Debug)]
struct PendingWriteProgram {
    ops: Vec<PendingWriteOp>,
}

#[derive(Arbitrary, Debug)]
enum PendingWriteOp {
    Append { len: u8, byte: u8, fin: bool },
    Flush { max_accept: u8 },
    Observe,
    Close,
    Reopen,
}

fuzz_target!(|program: PendingWriteProgram| {
    let ops: Vec<(u8, u8, u8)> = program
        .ops
        .into_iter()
        .take(256)
        .map(|op| match op {
            PendingWriteOp::Append { len, byte, fin } => (0, len, (byte & 0xfe) | u8::from(fin)),
            PendingWriteOp::Flush { max_accept } => (1, max_accept, 0),
            PendingWriteOp::Observe => (2, 0, 0),
            PendingWriteOp::Close => (3, 0, 0),
            PendingWriteOp::Reopen => (4, 0, 0),
        })
        .collect();

    http3::fuzz_exports::pending_write_state_ops(&ops);
});
