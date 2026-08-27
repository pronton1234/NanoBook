//! Dump every crossed-at-a-boundary event with enough context to judge it.
//!
//! The audit reported 8 of these on the 2017 fixture. A crossed book is either a
//! real bug in the book, or a real thing the feed published — and the only way
//! to tell is to look at the messages that produced it rather than to relax the
//! check until it stops complaining.
//!
//!     cargo run --release --example crossed -- data/raw/<file>.pcap

use book::{BTreeLevels, Book, Levels};
use deep::{Message, Price, Side};
use iextp::{capture::Capture, frame, segment::Segment, sequencer::Sequencer};

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: crossed <capture.pcap>");
    let limit: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);
    let buf = std::fs::read(&path).expect("read capture");
    let reader = Capture::open(&buf).expect("open capture");

    let mut seq = Sequencer::default();
    let mut b: Book<BTreeLevels> = Book::with_capacity(8192);
    let mut shown = 0usize;
    let mut locked = 0u64;
    let mut truly_crossed = 0u64;

    println!("{path}\n");
    println!("  events where best_bid >= best_ask with the event-complete flag set\n");

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
        for body in seg.messages() {
            let Ok(msg) = deep::parse(body) else { continue };
            let before = b.stats.crossed_when_stable;
            b.apply(&msg);
            if b.stats.crossed_when_stable == before {
                continue;
            }
            let Message::PriceLevel(u) = msg else {
                continue;
            };
            let sb = b.get(u.symbol).unwrap();
            let (bid, bsz) = sb.best_bid().unwrap();
            let (ask, asz) = sb.best_ask().unwrap();
            if bid == ask {
                locked += 1;
            } else {
                truly_crossed += 1;
            }
            if shown < limit {
                shown += 1;
                println!(
                    "  {:<8} {} {:>7} @ {:<10} flags 0x{:02x}   =>  bid {} x{}  ask {} x{}   {}",
                    u.symbol,
                    if u.side == Side::Buy { "BID" } else { "ASK" },
                    u.size,
                    u.price.to_string(),
                    u.flags,
                    bid,
                    bsz,
                    ask,
                    asz,
                    if bid == ask { "LOCKED" } else { "CROSSED" }
                );
                println!("           bids: {}", dump(&sb.bids));
                println!("           asks: {}", dump(&sb.asks));
            }
        }
    }

    println!("\n  locked  (bid == ask)  {locked}");
    println!("  crossed (bid >  ask)  {truly_crossed}");
}

fn dump<L: Levels>(l: &L) -> String {
    let mut v: Vec<(Price, u32)> = Vec::new();
    l.for_each(|p, s| v.push((p, s)));
    v.sort_unstable();
    v.iter()
        .map(|(p, s)| format!("{p}x{s}"))
        .collect::<Vec<_>>()
        .join("  ")
}
