//! Histogram of (message type, byte length) pairs across a capture.
//!
//! Written before the DEEP parser, to derive each message's wire length from
//! real data rather than transcribing it from a specification and hoping. If a
//! type shows more than one length here, either the format is genuinely variable
//! or the framing is being misread — and it is worth knowing which before
//! building a parser on top.
//!
//!     cargo run --release --example msglens -- data/raw/<file>.pcap

use std::collections::BTreeMap;

use iextp::{capture::Capture, frame, segment::Segment, sequencer::Sequencer};

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: msglens <capture.pcap>");
    let buf = std::fs::read(&path).expect("read capture");
    let reader = Capture::open(&buf).expect("open capture");

    // (type, len) -> count
    let mut hist: BTreeMap<(u8, usize), u64> = BTreeMap::new();
    let mut seq = Sequencer::default();

    for pkt in reader {
        let Ok(dg) = frame::parse(pkt.data) else {
            continue;
        };
        let Ok(seg) = Segment::parse(dg.payload) else {
            continue;
        };
        if !matches!(
            seq.observe(seg.session, seg.first_sequence, seg.message_count),
            iextp::Action::Deliver | iextp::Action::SessionReset
        ) {
            continue;
        }
        for m in seg.messages() {
            if let Some(&t) = m.first() {
                *hist.entry((t, m.len())).or_default() += 1;
            }
        }
    }

    println!("{path}\n");
    println!("  {:>4} {:>6} {:>14}   name", "type", "len", "count");
    let mut lens_by_type: BTreeMap<u8, Vec<(usize, u64)>> = BTreeMap::new();
    for ((t, len), n) in hist {
        lens_by_type.entry(t).or_default().push((len, n));
    }
    for (t, mut lens) in lens_by_type {
        lens.sort_unstable_by_key(|&(_, n)| std::cmp::Reverse(n));
        for (i, (len, n)) in lens.iter().enumerate() {
            let flag = if i > 0 {
                "  <-- SECOND LENGTH FOR THIS TYPE"
            } else {
                ""
            };
            println!("  0x{t:02x} {len:>6} {n:>14}   {}{flag}", name(t));
        }
    }
}

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
