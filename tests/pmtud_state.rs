//! Exercise the vendored discovery state tests from the root workspace.
//! The registry-normalized vendor crate is not itself a workspace member.
#![allow(dead_code)]
#[macro_use]
extern crate log;
const MIN_CLIENT_INITIAL_LEN: usize = 1200;
#[path = "../vendor/quiche/src/pmtud.rs"]
mod pmtud;
