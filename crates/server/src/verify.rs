//! Decode a real exchange session and report whether it was understood.
//!
//! Everything else this server measures runs against generated data. That is
//! honest — the generator writes valid IEX-TP and DEEP, and the round trip is
//! tested — but a visitor has no reason to take it on faith, and "it decodes my
//! own output correctly" is a weaker claim than it sounds.
//!
//! So ten real sessions are compiled into the binary, one per year from 2017 to
//! 2026, each the opening couple of megabytes of that day's DEEP feed. A visitor
//! picks a year and watches the decoder run against bytes IEX actually
//! published, with the ticker symbols and quoted prices from that morning
//! visible in the output.
//!
//! ## What this test caught
//!
//! It was not decoration. Running it across a decade found that **from 2022
//! onward roughly 22% of every session was a message type the decoder did not
//! know** — sessions through 2021 were clean. IEX had added a per-symbol status
//! message (`0x49`) to DEEP and nothing had told us.
//!
//! It was visible only because unknown types are *counted and reported* rather
//! than skipped. A decoder that quietly dropped what it did not recognise would
//! have shown ten clean years while ignoring a fifth of five of them.
//!
//! ## Data
//!
//! Provided for free by IEX. By accessing or using IEX Historical Data you agree
//! to the IEX Historical Data Terms of Use. <https://iextrading.com>

use std::collections::BTreeMap;

use book::{Book, SortedLevels};
use deep::{Message, Symbol};
use iextp::{capture::Capture, frame, segment::Segment, sequencer::Sequencer, Action};

/// One real session, compiled in. Roughly 2 MB each: the first few tens of
/// thousands of messages of that morning, which is enough to prove the decoder
/// understands the feed and small enough to ship in the image.
pub struct Sample {
    pub date: &'static str,
    pub bytes: &'static [u8],
}

pub const SAMPLES: &[Sample] = &[
    Sample {
        date: "2017-11-16",
        bytes: include_bytes!("../samples/20171116.pcap"),
    },
    Sample {
        date: "2018-10-09",
        bytes: include_bytes!("../samples/20181009.pcap"),
    },
    Sample {
        date: "2019-02-27",
        bytes: include_bytes!("../samples/20190227.pcap"),
    },
    Sample {
        date: "2020-07-30",
        bytes: include_bytes!("../samples/20200730.pcap"),
    },
    Sample {
        date: "2021-06-28",
        bytes: include_bytes!("../samples/20210628.pcap"),
    },
    Sample {
        date: "2022-06-02",
        bytes: include_bytes!("../samples/20220602.pcap"),
    },
    Sample {
        date: "2023-01-03",
        bytes: include_bytes!("../samples/20230103.pcap"),
    },
    Sample {
        date: "2024-07-09",
        bytes: include_bytes!("../samples/20240709.pcap"),
    },
    Sample {
        date: "2025-09-10",
        bytes: include_bytes!("../samples/20250910.pcap"),
    },
    Sample {
        date: "2026-05-22",
        bytes: include_bytes!("../samples/20260522.pcap"),
    },
];

pub fn find(date: &str) -> Option<&'static Sample> {
    SAMPLES.iter().find(|s| s.date == date)
}

#[derive(Debug, Default)]
pub struct Quote {
    pub symbol: String,
    pub bid: String,
    pub bid_size: u32,
    pub ask: String,
    pub ask_size: u32,
}

#[derive(Debug, Default)]
pub struct Verdict {
    pub date: String,
    pub format: &'static str,
    pub bytes: usize,
    pub packets: u64,
    pub messages: u64,
    /// Messages whose type is known but whose length was wrong. Any non-zero
    /// value means the framing above the message layer is broken.
    pub errors: u64,
    /// Messages of a type this decoder does not model. Counted, never dropped.
    pub unknown: u64,
    pub symbols: usize,
    pub price_updates: u64,
    pub trades: u64,
    pub gaps: u64,
    pub duplicates: u64,
    pub crossed: u64,
    pub low: String,
    pub high: String,
    /// Message type byte to count, so a visitor can see the mix.
    pub types: Vec<(String, u64)>,
    /// Real quotes from that morning. The most convincing thing in the output:
    /// these are tickers and prices IEX published on that date.
    pub quotes: Vec<Quote>,
    pub nanos: u128,
}

fn type_name(t: u8) -> &'static str {
    match t {
        0x38 => "Price Level Update (buy)",
        0x35 => "Price Level Update (sell)",
        0x54 => "Trade Report",
        0x42 => "Trade Break",
        0x53 => "System Event",
        0x44 => "Security Directory",
        0x48 => "Trading Status",
        0x4f => "Operational Halt",
        0x50 => "Short Sale Price Test",
        0x45 => "Security Event",
        0x49 => "Retail Liquidity (added ~2022)",
        0x58 => "Official Price",
        0x41 => "Auction Information",
        _ => "unknown",
    }
}

pub fn verify(s: &Sample) -> Verdict {
    let start = std::time::Instant::now();
    let mut v = Verdict {
        date: s.date.to_string(),
        bytes: s.bytes.len(),
        ..Default::default()
    };

    let Some(reader) = Capture::open(s.bytes).ok() else {
        v.format = "unreadable";
        return v;
    };
    v.format = match reader.format() {
        iextp::capture::Format::Pcap => "classic pcap",
        iextp::capture::Format::PcapNg => "pcapng",
    };

    let mut seq = Sequencer::default();
    let mut book: Book<SortedLevels> = Book::with_capacity(8192);
    let mut counts: BTreeMap<u8, u64> = BTreeMap::new();
    let mut symbols: std::collections::HashSet<Symbol> = std::collections::HashSet::new();
    let mut low = i64::MAX;
    let mut high = i64::MIN;

    for pkt in reader {
        v.packets += 1;
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
            let Some(&kind) = body.first() else { continue };
            match deep::parse(body) {
                Err(_) => {
                    v.errors += 1;
                    continue;
                }
                Ok(msg) => {
                    v.messages += 1;
                    *counts.entry(kind).or_default() += 1;
                    if let Some(sym) = msg.symbol() {
                        symbols.insert(sym);
                    }
                    match msg {
                        Message::Unknown { .. } => v.unknown += 1,
                        Message::Trade(_) => v.trades += 1,
                        Message::PriceLevel(u) => {
                            v.price_updates += 1;
                            book.apply_price_level(&u);
                            if !u.is_delete() {
                                low = low.min(u.price.raw());
                                high = high.max(u.price.raw());
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    v.symbols = symbols.len();
    v.gaps = seq.stats.gaps_opened;
    v.duplicates = seq.stats.duplicates;
    v.crossed = book.stats.crossed_when_stable;
    if high >= low {
        v.low = deep::Price::from_raw(low).to_string();
        v.high = deep::Price::from_raw(high).to_string();
    }
    v.types = counts
        .iter()
        .map(|(&t, &n)| (format!("0x{t:02x} {}", type_name(t)), n))
        .collect();
    v.types.sort_by_key(|(_, n)| std::cmp::Reverse(*n));

    // Real two-sided quotes, so the output shows a market rather than counters.
    let mut quotes: Vec<Quote> = book
        .iter()
        .filter_map(|(sym, sb)| {
            let (bid, bsz) = sb.best_bid()?;
            let (ask, asz) = sb.best_ask()?;
            Some(Quote {
                symbol: sym.to_string(),
                bid: bid.to_string(),
                bid_size: bsz,
                ask: ask.to_string(),
                ask_size: asz,
            })
        })
        .collect();
    quotes.sort_by(|a, b| a.symbol.cmp(&b.symbol));
    quotes.truncate(8);
    v.quotes = quotes;

    v.nanos = start.elapsed().as_nanos();
    v
}
