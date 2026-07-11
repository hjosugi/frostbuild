#![no_main]
use libfuzzer_sys::fuzz_target;
fuzz_target!(|data: &[u8]| {
    let _ = frostbuild_core::graph_store::GraphStore::validate_bytes(data);
});
