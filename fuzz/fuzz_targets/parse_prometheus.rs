#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../../src/llama/prometheus.rs"]
mod prometheus;

fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        let metrics = prometheus::parse_prometheus(text);
        std::hint::black_box(metrics.len());
    }
});
