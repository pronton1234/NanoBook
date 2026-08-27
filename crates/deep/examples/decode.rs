//! Decode a real capture end to end and check the results look like a market.
//!
//! Unit tests here are built from bytes I wrote, so they can only confirm the
//! decoder is self-consistent with my belief about the layout. If a field offset
//! is off by four, a synthetic test written from the same wrong belief passes.
//!
//! Real data cannot be fooled that way. A wrong price offset produces prices in
//! the millions or the negatives; a wrong symbol offset produces bytes that are
//! not tickers; a wrong size offset produces share counts that are not round
//! lots. So this prints the distributions and lets them be judged.
//!
//!     cargo run --release --example decode -- data/raw/<file>.pcap

use std::collections::HashMap;
use std::time::Instant;

use deep::{Message, Price, Side, Symbol};
use iextp::{capture::Capture, frame, segment::Segment, sequencer::Sequencer};

#[derive(Default)]
struct SymbolStat {
    updates: u64,
    trades: u64,
    traded_shares: u64,
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: decode <capture.pcap>");
    let buf = std::fs::read(&path).expect("read capture");
    let reader = Capture::open(&buf).expect("open capture");
    println!("{path}\n");

    let mut seq = Sequencer::default();
    let mut per_symbol: HashMap<Symbol, SymbolStat> = HashMap::new();
    let mut decoded = 0u64;
    let mut errors = 0u64;
    let mut unknown = 0u64;
    let mut deletes = 0u64;
    let mut buys = 0u64;
    let mut sells = 0u64;
    let mut incomplete_events = 0u64;

    // Price and size sanity: a wrong field offset shows up here immediately.
    let mut min_px = Price::from_raw(i64::MAX);
    let mut max_px = Price::ZERO;
    let mut off_penny_grid = 0u64;
    let mut odd_lots = 0u64;
    let mut first_examples: Vec<String> = Vec::new();
    let mut ts_min = i64::MAX;
    let mut ts_max = i64::MIN;

    let start = Instant::now();
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
            let msg = match deep::parse(body) {
                Ok(m) => m,
                Err(_) => {
                    errors += 1;
                    continue;
                }
            };
            decoded += 1;
            if let Some(t) = msg.timestamp() {
                ts_min = ts_min.min(t);
                ts_max = ts_max.max(t);
            }
            match msg {
                Message::PriceLevel(m) => {
                    let e = per_symbol.entry(m.symbol).or_default();
                    e.updates += 1;
                    match m.side {
                        Side::Buy => buys += 1,
                        Side::Sell => sells += 1,
                    }
                    if m.is_delete() {
                        deletes += 1;
                    } else {
                        if m.price < min_px {
                            min_px = m.price;
                        }
                        if m.price > max_px {
                            max_px = m.price;
                        }
                        if m.price.tick_index(deep::UNITS_PER_DOLLAR / 100).is_none() {
                            off_penny_grid += 1;
                        }
                        if m.size % 100 != 0 {
                            odd_lots += 1;
                        }
                        if first_examples.len() < 6 {
                            first_examples.push(format!(
                                "    {:<8} {:<4} {:>8} @ {:>10}   flags 0x{:02x}",
                                m.symbol,
                                if m.side == Side::Buy { "BID" } else { "ASK" },
                                m.size,
                                m.price,
                                m.flags
                            ));
                        }
                    }
                    if !m.event_complete() {
                        incomplete_events += 1;
                    }
                }
                Message::Trade(t) => {
                    let e = per_symbol.entry(t.symbol).or_default();
                    e.trades += 1;
                    e.traded_shares += t.size as u64;
                }
                Message::Unknown { .. } => unknown += 1,
                _ => {}
            }
        }
    }
    let dt = start.elapsed();

    println!("  decoded {decoded:>12}   errors {errors}   unknown types {unknown}");
    println!("  symbols {:>12}", per_symbol.len());
    println!("\n  price level updates");
    println!("    buy  {buys:>12}");
    println!("    sell {sells:>12}");
    println!(
        "    deletes (size 0) {deletes:>10}  {:.1}%",
        deletes as f64 / (buys + sells).max(1) as f64 * 100.0
    );
    println!(
        "    mid-event (flag clear) {incomplete_events:>4}  {:.1}%",
        incomplete_events as f64 / (buys + sells).max(1) as f64 * 100.0
    );

    println!("\n  sanity — these must look like a market, not like noise");
    if max_px > Price::ZERO {
        println!("    price range      {min_px} .. {max_px}");
    }
    println!(
        "    off penny grid   {off_penny_grid}  ({:.2}% — sub-dollar names quote finer)",
        off_penny_grid as f64 / (buys + sells - deletes).max(1) as f64 * 100.0
    );
    println!(
        "    odd lots         {odd_lots}  ({:.1}%)",
        odd_lots as f64 / (buys + sells - deletes).max(1) as f64 * 100.0
    );
    if ts_min != i64::MAX {
        println!(
            "    timestamps       {} .. {} (epoch seconds {} .. {})",
            ts_min,
            ts_max,
            ts_min / 1_000_000_000,
            ts_max / 1_000_000_000
        );
    }

    println!("\n  first price level updates seen");
    for line in &first_examples {
        println!("{line}");
    }

    let mut top: Vec<_> = per_symbol.iter().collect();
    top.sort_unstable_by_key(|(_, s)| std::cmp::Reverse(s.updates));
    println!("\n  busiest symbols");
    println!(
        "    {:<8} {:>12} {:>10} {:>14}",
        "symbol", "updates", "trades", "shares"
    );
    for (sym, s) in top.iter().take(10) {
        println!(
            "    {:<8} {:>12} {:>10} {:>14}",
            sym, s.updates, s.trades, s.traded_shares
        );
    }

    let secs = dt.as_secs_f64();
    println!(
        "\n  {:.3}s   {:.1} M msg/s   {:.2} GB/s",
        secs,
        decoded as f64 / secs / 1e6,
        buf.len() as f64 / secs / 1e9
    );
}
