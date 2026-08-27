//! Run a real IEX HIST capture through the whole transport stack.
//!
//! This is the crate's actual correctness argument. Unit tests here are built on
//! synthetic frames, which prove the code does what I *think* the format is;
//! only a real capture proves I understood the format. The counts printed below
//! are cross-checked against an independent Python decoder written first, so a
//! disagreement between the two means one of them is wrong.
//!
//!     cargo run --release --example replay -- data/raw/<file>.pcap

use std::collections::BTreeMap;
use std::time::Instant;

use iextp::{capture::Capture, frame, segment::Segment, sequencer::Sequencer, FrameSkip};

fn main() {
    let path = match std::env::args().nth(1) {
        Some(p) => p,
        None => {
            eprintln!("usage: replay <capture.pcap>");
            std::process::exit(2);
        }
    };
    let buf = std::fs::read(&path).unwrap_or_else(|e| {
        eprintln!("{path}: {e}");
        std::process::exit(1);
    });

    let reader = match Capture::open(&buf) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{path}: {e}");
            std::process::exit(1);
        }
    };
    println!("{path}");
    println!(
        "  {} bytes, {:?}, linktype {}, snaplen {}",
        buf.len(),
        reader.format(),
        reader.linktype(),
        reader.snaplen()
    );

    let mut packets = 0u64;
    let mut vlan_or_other_skips: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut segments = 0u64;
    let mut messages = 0u64;
    let mut kinds: BTreeMap<u8, u64> = BTreeMap::new();
    let mut seq = Sequencer::default();

    let start = Instant::now();
    for pkt in reader {
        packets += 1;
        let dg = match frame::parse(pkt.data) {
            Ok(d) => d,
            Err(e) => {
                *vlan_or_other_skips
                    .entry(match e {
                        FrameSkip::TooShort => "too short",
                        FrameSkip::NotIpv4 => "not ipv4",
                        FrameSkip::NotUdp => "not udp",
                        FrameSkip::Fragmented => "fragmented",
                    })
                    .or_default() += 1;
                continue;
            }
        };
        let Ok(seg) = Segment::parse(dg.payload) else {
            continue;
        };
        segments += 1;
        // Sequence every segment, including the losing copy of an A/B pair.
        // Only delivered segments have their messages counted.
        match seq.observe(seg.session, seg.first_sequence, seg.message_count) {
            iextp::Action::Deliver | iextp::Action::SessionReset => {
                for m in seg.messages() {
                    messages += 1;
                    if let Some(&t) = m.first() {
                        *kinds.entry(t).or_default() += 1;
                    }
                }
            }
            _ => {}
        }
    }
    let dt = start.elapsed();

    println!("\n  packets      {packets:>12}");
    println!("  segments     {segments:>12}");
    println!("  messages     {messages:>12}");
    for (why, n) in &vlan_or_other_skips {
        println!("  skipped ({why}) {n}");
    }

    let s = seq.stats;
    println!("\n  transport");
    println!("    delivered      {:>10}", s.delivered);
    println!(
        "    duplicates     {:>10}  (B-feed copies and retransmits)",
        s.duplicates
    );
    println!("    heartbeats     {:>10}", s.heartbeats);
    println!("    gaps opened    {:>10}", s.gaps_opened);
    println!("    gaps healed    {:>10}", s.gaps_healed);
    println!("    session resets {:>10}", s.session_resets);
    println!("    still held     {:>10}", seq.held_len());

    println!("\n  message types");
    let mut by_count: Vec<_> = kinds.iter().map(|(&k, &v)| (v, k)).collect();
    by_count.sort_unstable_by(|a, b| b.cmp(a));
    for (n, t) in by_count {
        let pct = n as f64 / messages.max(1) as f64 * 100.0;
        println!("    0x{t:02x} {:<26} {n:>10}  {pct:>5.2}%", name(t));
    }

    let secs = dt.as_secs_f64();
    println!(
        "\n  {:.3}s   {:.1} M msg/s   {:.2} GB/s",
        secs,
        messages as f64 / secs / 1e6,
        buf.len() as f64 / secs / 1e9
    );
}

/// DEEP 1.0 message types, from the IEX DEEP specification.
fn name(t: u8) -> &'static str {
    match t {
        0x53 => "System Event",
        0x44 => "Security Directory",
        0x48 => "Trading Status",
        0x4f => "Operational Halt",
        0x50 => "Short Sale Price Test",
        0x45 => "Security Event",
        0x38 => "Price Level Update BUY",
        0x35 => "Price Level Update SELL",
        0x54 => "Trade Report",
        0x58 => "Official Price",
        0x42 => "Trade Break",
        0x41 => "Auction Information",
        _ => "unknown",
    }
}
