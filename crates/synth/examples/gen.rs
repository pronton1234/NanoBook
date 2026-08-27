//! Write a generated capture to disk.
//!
//! Exists so the Rust and C++ benchmarks read byte-identical input. A
//! cross-language comparison over two different corpora measures the corpora.
//!
//!     cargo run --release -p synth --example gen -- out.pcap [segments]

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: gen <out.pcap> [segments]");
    let segments: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(200_000);
    let cap = synth::generate(&synth::Config {
        segments,
        ..Default::default()
    });
    std::fs::write(&path, &cap.bytes).expect("write capture");
    println!(
        "{path}: {} bytes, {} segments emitted, {} messages",
        cap.bytes.len(),
        cap.segments_emitted,
        cap.messages_published
    );
}
