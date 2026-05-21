#![no_main]
//! Fuzz the loose-object body parser. Goal: feed arbitrary bytes through
//! `RawObject::parse_loose` and assert no panic / no UB. Errors are fine.

use libfuzzer_sys::fuzz_target;
use rustygit::hash::HashKind;
use rustygit::object::RawObject;

fuzz_target!(|data: &[u8]| {
    let _ = RawObject::parse_loose(data, HashKind::Sha1);
    let _ = RawObject::parse_loose(data, HashKind::Sha256);
});
