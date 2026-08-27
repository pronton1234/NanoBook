// Implementations for the header-declared decode and book paths.
#include "ll/book.hpp"
#include "ll/capture.hpp"
#include "ll/deep.hpp"
#include "ll/price.hpp"

#include <algorithm>
#include <cstdio>

namespace ll {

// ---------------------------------------------------------------- price -----

std::string Price::to_string() const {
    const bool neg = units_ < 0;
    const std::uint64_t abs = neg ? static_cast<std::uint64_t>(-(units_ + 1)) + 1
                                  : static_cast<std::uint64_t>(units_);
    const std::uint64_t whole = abs / kUnitsPerDollar;
    const std::uint64_t frac = abs % kUnitsPerDollar;
    char buf[40];
    // Integer division only. A price never passes through a double, not even to
    // be printed, because a formatting helper is exactly where such a
    // conversion sneaks back into arithmetic later.
    if (frac % 100 == 0) {
        std::snprintf(buf, sizeof buf, "%s%llu.%02llu", neg ? "-" : "",
                      static_cast<unsigned long long>(whole),
                      static_cast<unsigned long long>(frac / 100));
    } else {
        std::snprintf(buf, sizeof buf, "%s%llu.%04llu", neg ? "-" : "",
                      static_cast<unsigned long long>(whole),
                      static_cast<unsigned long long>(frac));
    }
    return std::string(buf);
}

// ----------------------------------------------------------------- pcap -----

std::uint32_t PcapReader::u32_at(std::size_t off) const noexcept {
    const std::uint32_t v = detail::load<std::uint32_t>(buf_.data() + off);
    if (little_) return v;
    return __builtin_bswap32(v);
}

std::optional<PcapReader> PcapReader::open(Bytes buf) noexcept {
    if (buf.size() < 24) return std::nullopt;
    const std::uint32_t be = static_cast<std::uint32_t>(buf[0]) << 24 |
                             static_cast<std::uint32_t>(buf[1]) << 16 |
                             static_cast<std::uint32_t>(buf[2]) << 8 |
                             static_cast<std::uint32_t>(buf[3]);
    PcapReader r;
    switch (be) {
        case 0xa1b2c3d4: r.little_ = false; r.nanos_ = false; break;
        case 0xa1b23c4d: r.little_ = false; r.nanos_ = true;  break;
        case 0xd4c3b2a1: r.little_ = true;  r.nanos_ = false; break;
        case 0x4d3cb2a1: r.little_ = true;  r.nanos_ = true;  break;
        default: return std::nullopt;
    }
    r.buf_ = buf;
    r.pos_ = 24;
    r.linktype_ = r.u32_at(20);
    return r;
}

std::optional<Packet> PcapReader::next() noexcept {
    if (pos_ + 16 > buf_.size()) return std::nullopt;
    const std::uint64_t sec = u32_at(pos_);
    const std::uint64_t frac = u32_at(pos_ + 4);
    const std::size_t incl = u32_at(pos_ + 8);
    const std::size_t start = pos_ + 16;
    // A record claiming more bytes than remain means the file was truncated
    // mid-write, which is normal for an interrupted capture. Stop cleanly.
    if (start + incl > buf_.size()) return std::nullopt;
    pos_ = start + incl;
    return Packet{sec * 1'000'000'000ull + (nanos_ ? frac : frac * 1'000),
                  buf_.subspan(start, incl)};
}

// ---------------------------------------------------------------- frame -----

std::optional<Datagram> parse_frame(Bytes frame) noexcept {
    constexpr std::uint16_t kVlan = 0x8100;
    constexpr std::uint16_t kIpv4 = 0x0800;
    constexpr std::uint8_t kUdp = 17;

    if (frame.size() < 14) return std::nullopt;
    std::size_t off = 14;
    std::uint16_t ethertype = detail::be16(frame.data() + 12);
    // QinQ is legal, so this loops. The cost over an `if` is nothing and it
    // removes a class of "works on my capture" bug.
    while (ethertype == kVlan) {
        if (frame.size() < off + 4) return std::nullopt;
        ethertype = detail::be16(frame.data() + off + 2);
        off += 4;
    }
    if (ethertype != kIpv4 || frame.size() < off + 20) return std::nullopt;

    const std::uint8_t* ip = frame.data() + off;
    const std::size_t ihl = static_cast<std::size_t>(ip[0] & 0x0f) * 4;
    if (ihl < 20 || frame.size() < off + ihl) return std::nullopt;
    if (ip[9] != kUdp) return std::nullopt;
    // A fragment cannot be parsed from this frame alone; treating fragment zero
    // as a whole datagram would corrupt the stream.
    if ((ip[6] & 0x20) != 0 || (detail::be16(ip + 6) & 0x1fff) != 0) return std::nullopt;

    const std::uint8_t* udp = ip + ihl;
    const std::size_t avail = frame.size() - off - ihl;
    if (avail < 8) return std::nullopt;
    // Trust the UDP length over the captured length: Ethernet pads frames to 60
    // bytes and that padding is not payload.
    const std::size_t udp_len = detail::be16(udp + 4);
    if (udp_len < 8 || udp_len > avail) return std::nullopt;

    const std::size_t payload_off = off + ihl + 8;
    return Datagram{detail::be16(udp + 2), frame.subspan(payload_off, udp_len - 8)};
}

// -------------------------------------------------------------- segment -----

std::optional<Segment> parse_segment(Bytes p) noexcept {
    if (p.size() < kSegmentHeaderLen) return std::nullopt;
    // Little-endian from here down, unlike every layer above.
    const std::size_t payload_len = detail::load<std::uint16_t>(p.data() + 12);
    const std::size_t end = kSegmentHeaderLen + payload_len;
    if (end > p.size()) return std::nullopt;

    Segment s;
    s.protocol = detail::load<std::uint16_t>(p.data() + 2);
    s.session = detail::load<std::uint32_t>(p.data() + 8);
    s.message_count = detail::load<std::uint16_t>(p.data() + 14);
    s.first_sequence = detail::load<std::int64_t>(p.data() + 24);
    s.messages = p.subspan(kSegmentHeaderLen, payload_len);
    return s;
}

std::optional<Bytes> MessageIter::next() noexcept {
    if (remaining_ == 0 || pos_ + 2 > buf_.size()) return std::nullopt;
    const std::size_t len = detail::load<std::uint16_t>(buf_.data() + pos_);
    const std::size_t start = pos_ + 2;
    if (start + len > buf_.size()) return std::nullopt;
    pos_ = start + len;
    --remaining_;
    return buf_.subspan(start, len);
}

// ----------------------------------------------------------------- deep -----

std::optional<Message> parse_message(Bytes body) noexcept {
    if (body.empty()) return std::nullopt;
    const std::uint8_t kind = body[0];
    const auto want = expected_len(kind);
    if (!want) return Message{Unknown{kind}};
    // A known type at the wrong length means the framing above is wrong, not
    // that the message is exotic. Parsing the prefix would yield a confident
    // wrong answer.
    if (body.size() != *want) return std::nullopt;

    const std::uint8_t* b = body.data();
    switch (kind) {
        // The hot path first: 97% of messages land here.
        case kPriceLevelBuy:
        case kPriceLevelSell: {
            PriceLevelUpdate u;
            u.side = (kind == kPriceLevelBuy) ? Side::Buy : Side::Sell;
            u.flags = b[1];
            u.timestamp = detail::load<std::int64_t>(b + 2);
            u.symbol = Symbol{b + 10};
            u.size = detail::load<std::uint32_t>(b + 18);
            u.price = Price{detail::load<std::int64_t>(b + 22)};
            return Message{u};
        }
        case kTradeReport:
        case kTradeBreak: {
            Trade t;
            t.is_break = (kind == kTradeBreak);
            t.timestamp = detail::load<std::int64_t>(b + 2);
            t.symbol = Symbol{b + 10};
            t.size = detail::load<std::uint32_t>(b + 18);
            t.price = Price{detail::load<std::int64_t>(b + 22)};
            return Message{t};
        }
        default: return Message{Unknown{kind}};
    }
}

// ----------------------------------------------------------------- book -----

std::size_t Levels::find(Price price, bool& found) const noexcept {
    const std::size_t n = prices_.size();
    for (std::size_t i = 0; i < n; ++i) {
        if (prices_[i] == price) { found = true; return i; }
        if (prices_[i] > price) { found = false; return i; }
    }
    found = false;
    return n;
}

void Levels::set(Price price, std::uint32_t size) noexcept {
    bool found = false;
    const std::size_t i = find(price, found);
    if (found) {
        if (size == 0) {
            prices_.erase(prices_.begin() + static_cast<std::ptrdiff_t>(i));
            sizes_.erase(sizes_.begin() + static_cast<std::ptrdiff_t>(i));
        } else {
            sizes_[i] = size;
        }
        return;
    }
    // A delete for a level we do not hold is normal when joining a live feed
    // mid-session, and must not insert a zero-size level.
    if (size == 0) return;
    prices_.insert(prices_.begin() + static_cast<std::ptrdiff_t>(i), price);
    sizes_.insert(sizes_.begin() + static_cast<std::ptrdiff_t>(i), size);
}

std::uint64_t Levels::total_size() const noexcept {
    std::uint64_t t = 0;
    for (const auto s : sizes_) t += s;
    return t;
}

Touch Book::apply(const PriceLevelUpdate& u) noexcept {
    SymbolBook& sb = books_[u.symbol];
    ++stats.updates;
    if (u.size == 0) ++stats.deletes;

    if (u.side == Side::Buy) {
        sb.bids.set(u.price, u.size);
    } else {
        sb.asks.set(u.price, u.size);
    }
    sb.complete = u.event_complete();

    // Read the touch ONCE. Asking is_crossed() and is_locked() separately would
    // read both sides twice, and every one of those is a dependent load into
    // level storage.
    Touch t{sb.bids.max(), sb.asks.min(), sb.complete};
    if (t.bid && t.ask) {
        // Gated on the event-complete flag. IEX splits one logical book event
        // across several messages and is legitimately crossed in between, so a
        // validator that asserts on every message fires constantly on a healthy
        // feed -- and the natural fix, relaxing the assert, discards the one
        // invariant that catches real corruption.
        if (t.bid->first > t.ask->first) {
            if (t.stable) ++stats.crossed_when_stable;
        } else if (t.bid->first == t.ask->first && t.stable) {
            ++stats.locked_when_stable;
        }
    }
    return t;
}

std::size_t Book::total_levels() const noexcept {
    std::size_t n = 0;
    books_.for_each([&](const Symbol&, const SymbolBook& sb) {
        n += sb.bids.size() + sb.asks.size();
    });
    return n;
}

std::uint64_t Book::total_size() const noexcept {
    std::uint64_t t = 0;
    books_.for_each([&](const Symbol&, const SymbolBook& sb) {
        t += sb.bids.total_size() + sb.asks.total_size();
    });
    return t;
}

}  // namespace ll
