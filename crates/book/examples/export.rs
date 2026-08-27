//! Export top-of-book to CSV for the Python analysis layer.
//!
//!     cargo run --release -p book --example export -- <capture.pcap> <out.csv>
//!
//! The division of labour is deliberate. Rust owns the wire: framing,
//! sequencing, decoding, and maintaining the book, because that is the part
//! where nanoseconds and correctness matter. Python owns the statistics, because
//! that is where iteration speed and libraries matter and where a hand-rolled
//! regression would be a liability rather than a demonstration.
//!
//! Re-implementing the DEEP decoder in Python to do the analysis would mean two
//! decoders that can silently disagree — the exact hazard this project keeps
//! finding. One decoder, one export, one source of truth.
//!
//! A row is written on every change to the touch, not on every message: an
//! update deep in the book moves no quote and would only pad the file.

use std::io::{BufWriter, Write};

use book::{Book, SortedLevels};
use deep::{Message, Price, Symbol};
use iextp::{capture::Capture, frame, segment::Segment, sequencer::Sequencer, Action};

fn main() {
    let cap_path = std::env::args()
        .nth(1)
        .expect("usage: export <capture.pcap> <out.csv>");
    let out_path = std::env::args().nth(2).expect("missing output path");

    let buf = std::fs::read(&cap_path).expect("read capture");
    let reader = Capture::open(&buf).expect("not a capture");
    let mut seq = Sequencer::default();
    let mut b: Book<SortedLevels> = Book::with_capacity(8192);

    let f = std::fs::File::create(&out_path).expect("create output");
    let mut w = BufWriter::with_capacity(1 << 20, f);
    writeln!(w, "ts_nanos,symbol,bid,bid_size,ask,ask_size").unwrap();

    // Last published touch per symbol, so only genuine changes are written.
    let mut last: std::collections::HashMap<Symbol, (Price, u32, Price, u32)> =
        std::collections::HashMap::new();
    let mut rows = 0u64;
    let mut messages = 0u64;

    for pkt in reader {
        let Ok(dg) = frame::parse(pkt.data) else {
            continue;
        };
        let Ok(seg) = Segment::parse(dg.payload) else {
            continue;
        };
        if !matches!(
            seq.observe(seg.session, seg.first_sequence, seg.message_count),
            Action::Deliver | Action::SessionReset
        ) {
            continue;
        }
        for body in seg.messages() {
            let Ok(msg) = deep::parse(body) else { continue };
            messages += 1;
            let Message::PriceLevel(u) = msg else {
                continue;
            };
            let touch = b.apply_price_level(&u);

            // Only a two-sided, settled book is a quote. Writing a half-updated
            // one would put states into the regression that the feed never
            // published, and the event-complete flag exists precisely to say so.
            if !touch.stable {
                continue;
            }
            let (Some((bid, bsz)), Some((ask, asz))) = (touch.bid, touch.ask) else {
                continue;
            };
            let now = (bid, bsz, ask, asz);
            if last.get(&u.symbol) == Some(&now) {
                continue;
            }
            last.insert(u.symbol, now);

            // Prices as raw integer units. Formatting to a decimal string here
            // and parsing it back in Python would round-trip every price through
            // a float for no reason; the analysis divides by the scale once.
            writeln!(
                w,
                "{},{},{},{},{},{}",
                u.timestamp,
                u.symbol,
                bid.raw(),
                bsz,
                ask.raw(),
                asz
            )
            .unwrap();
            rows += 1;
        }
    }
    w.flush().unwrap();
    println!("{out_path}: {rows} quote changes from {messages} messages");
}
