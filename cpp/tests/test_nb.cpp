// Tests for the C++ decode and book paths.
//
// A hand-rolled harness rather than GoogleTest, for the same reason the Rust
// side has one external dependency: a few dozen assertions do not justify a
// framework, and the assertions are the interesting part.
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <vector>

#include "nb/book.hpp"
#include "nb/capture.hpp"
#include "nb/deep.hpp"

namespace {

int g_failures = 0;
int g_checks = 0;

void check(bool cond, const char* what, int line) {
    ++g_checks;
    if (!cond) {
        ++g_failures;
        std::printf("  FAIL (line %d): %s\n", line, what);
    }
}

#define CHECK(cond) check((cond), #cond, __LINE__)

// ------------------------------------------------------------ price ---------

void test_price_is_exact() {
    using nb::Price;
    CHECK(Price::from_parts(99, 9900).to_string() == "99.99");
    CHECK(Price::from_parts(0, 1).to_string() == "0.0001");
    CHECK(Price{1'000'000}.to_string() == "100.00");
    CHECK(Price{-25'000}.to_string() == "-2.50");

    // The whole point of the type: the classic binary-float error must not be
    // representable. 0.1 + 0.2 == 0.3 exactly, in integers.
    const auto a = Price::from_parts(0, 1'000);
    const auto b = Price::from_parts(0, 2'000);
    CHECK(Price{a.raw() + b.raw()} == Price::from_parts(0, 3'000));

    // Off-grid prices must say so rather than rounding into the wrong bucket.
    const std::int64_t penny = Price::kUnitsPerDollar / 100;
    CHECK(Price::from_parts(1, 0).tick_index(penny) == std::optional<std::size_t>{100});
    CHECK(!Price::from_parts(0, 1).tick_index(penny).has_value());
    CHECK(!Price{-100}.tick_index(penny).has_value());
}

void test_symbol_padding_and_hash() {
    const auto s = nb::Symbol::from_ticker("AAPL");
    CHECK(s.trimmed() == "AAPL");
    CHECK(s == nb::Symbol::from_ticker("AAPL"));
    CHECK(!(s == nb::Symbol::from_ticker("AAPY")));
    CHECK(nb::Symbol::from_ticker("ABCDEFGH").trimmed() == "ABCDEFGH");

    // Similar tickers must not collide, or the symbol map degenerates.
    nb::SymbolHash h;
    std::vector<std::size_t> hs;
    for (const char* t : {"AAPL", "AAPM", "AAPN", "BAPL", "SPY", "SPZ", "QQQ"})
        hs.push_back(h(nb::Symbol::from_ticker(t)));
    for (std::size_t i = 0; i < hs.size(); ++i)
        for (std::size_t j = i + 1; j < hs.size(); ++j) CHECK(hs[i] != hs[j]);
}

// ------------------------------------------------------------- deep ---------

std::vector<std::uint8_t> make_plu(std::uint8_t kind, std::uint8_t flags, const char* sym,
                                   std::uint32_t size, std::int64_t price) {
    std::vector<std::uint8_t> v;
    v.push_back(kind);
    v.push_back(flags);
    const std::int64_t ts = 1'500'000'000'000'000'000;
    v.insert(v.end(), reinterpret_cast<const std::uint8_t*>(&ts),
             reinterpret_cast<const std::uint8_t*>(&ts) + 8);
    const auto s = nb::Symbol::from_ticker(sym);
    const std::uint64_t su = s.as_u64();
    v.insert(v.end(), reinterpret_cast<const std::uint8_t*>(&su),
             reinterpret_cast<const std::uint8_t*>(&su) + 8);
    v.insert(v.end(), reinterpret_cast<const std::uint8_t*>(&size),
             reinterpret_cast<const std::uint8_t*>(&size) + 4);
    v.insert(v.end(), reinterpret_cast<const std::uint8_t*>(&price),
             reinterpret_cast<const std::uint8_t*>(&price) + 8);
    return v;
}

void test_deep_decode() {
    const auto raw = make_plu(nb::kPriceLevelBuy, 0x01, "AAPL", 100, 1'865'500);
    CHECK(raw.size() == 30);
    const auto m = nb::parse_message(nb::Bytes(raw));
    CHECK(m.has_value());
    const auto* u = std::get_if<nb::PriceLevelUpdate>(&*m);
    CHECK(u != nullptr);
    if (u) {
        CHECK(u->side == nb::Side::Buy);
        CHECK(u->symbol.trimmed() == "AAPL");
        CHECK(u->size == 100);
        CHECK(u->price.to_string() == "186.55");
        CHECK(u->event_complete());
        CHECK(!u->is_delete());
    }

    // Bit 7 is NOT the completion flag. The Rust implementation used 0x80 and
    // no synthetic test caught it; only real data did.
    const auto wrongbit = make_plu(nb::kPriceLevelSell, 0x80, "MSFT", 1, 100);
    const auto m2 = nb::parse_message(nb::Bytes(wrongbit));
    const auto* u2 = std::get_if<nb::PriceLevelUpdate>(&*m2);
    CHECK(u2 && !u2->event_complete());

    // A known type at the wrong length means the framing above is wrong.
    auto bad = make_plu(nb::kPriceLevelBuy, 0x01, "AAPL", 1, 100);
    bad.push_back(0);
    CHECK(!nb::parse_message(nb::Bytes(bad)).has_value());

    // An unknown type is reported, not rejected.
    const std::vector<std::uint8_t> unk{0xEE, 1, 2};
    const auto m3 = nb::parse_message(nb::Bytes(unk));
    CHECK(m3 && std::get_if<nb::Unknown>(&*m3) != nullptr);

    // Truncation must never read out of bounds.
    for (std::size_t n = 0; n < raw.size(); ++n)
        (void)nb::parse_message(nb::Bytes(raw.data(), n));
}

// ------------------------------------------------------------- book ---------

nb::PriceLevelUpdate upd(nb::Side side, const char* sym, nb::Price p, std::uint32_t size,
                         bool complete) {
    nb::PriceLevelUpdate u;
    u.side = side;
    u.flags = complete ? 0x01 : 0x00;
    u.symbol = nb::Symbol::from_ticker(sym);
    u.size = size;
    u.price = p;
    return u;
}

void test_book() {
    using nb::Price;
    using nb::Side;
    nb::Book b(16);

    b.apply(upd(Side::Buy, "AAPL", Price::from_parts(186, 5500), 100, true));
    b.apply(upd(Side::Sell, "AAPL", Price::from_parts(186, 5700), 200, true));
    const auto* sb = b.get(nb::Symbol::from_ticker("AAPL"));
    CHECK(sb != nullptr);
    if (sb) {
        CHECK(sb->bids.max()->first == Price::from_parts(186, 5500));
        CHECK(sb->asks.min()->first == Price::from_parts(186, 5700));
    }
    CHECK(b.stats.crossed_when_stable == 0);

    // Best bid is the max, best ask the min, whatever the arrival order.
    nb::Book c(16);
    for (auto cents : {0, 2, -2}) {
        const auto p = Price::from_parts(10, cents * 100);
        c.apply(upd(Side::Buy, "X", p, 1, true));
        c.apply(upd(Side::Sell, "X", p, 1, true));
    }
    const auto* x = c.get(nb::Symbol::from_ticker("X"));
    CHECK(x && x->bids.max()->first == Price::from_parts(10, 200));
    CHECK(x && x->asks.min()->first == Price::from_parts(10, -200));

    // Deleting the touch must uncover the next level, not empty the side.
    nb::Book d(16);
    d.apply(upd(Side::Buy, "X", Price::from_parts(10, 0), 100, true));
    d.apply(upd(Side::Buy, "X", Price::from_parts(10, 100), 200, true));
    d.apply(upd(Side::Buy, "X", Price::from_parts(10, 100), 0, true));
    const auto* dx = d.get(nb::Symbol::from_ticker("X"));
    CHECK(dx && dx->bids.size() == 1);
    CHECK(dx && dx->bids.max()->first == Price::from_parts(10, 0));

    // Crossing counts only at an event boundary. Mid-event the feed is
    // legitimately crossed and an assert there fires on a healthy feed.
    nb::Book e(16);
    e.apply(upd(Side::Buy, "Y", Price::from_parts(10, 500), 100, false));
    e.apply(upd(Side::Sell, "Y", Price::from_parts(10, 0), 100, false));
    CHECK(e.stats.crossed_when_stable == 0);

    nb::Book f(16);
    f.apply(upd(Side::Buy, "Z", Price::from_parts(10, 500), 100, true));
    f.apply(upd(Side::Sell, "Z", Price::from_parts(10, 0), 100, true));
    CHECK(f.stats.crossed_when_stable == 1);

    // Locked is not crossed. Folding them together lets a real crossing hide.
    nb::Book g(16);
    g.apply(upd(Side::Buy, "W", Price::from_parts(10, 0), 100, true));
    g.apply(upd(Side::Sell, "W", Price::from_parts(10, 0), 100, true));
    CHECK(g.stats.locked_when_stable == 1);
    CHECK(g.stats.crossed_when_stable == 0);

    // A delete for a level never held is normal mid-session, not a crash.
    nb::Book h(16);
    h.apply(upd(Side::Buy, "Q", Price::from_parts(42, 0), 0, true));
    const auto* hq = h.get(nb::Symbol::from_ticker("Q"));
    CHECK(hq && hq->bids.empty());
}

/// Levels must stay sorted and the parallel arrays must never disagree.
void test_levels_stay_sorted() {
    using nb::Price;
    nb::Levels l;
    const int order[] = {5, 1, 9, 3, 7, 0, 8, 2};
    for (int k : order) l.set(Price::from_parts(100, k * 100), static_cast<std::uint32_t>(k + 1));
    CHECK(l.size() == 8);
    CHECK(l.min()->first == Price::from_parts(100, 0));
    CHECK(l.max()->first == Price::from_parts(100, 900));

    // Resizing must not duplicate.
    l.set(Price::from_parts(100, 300), 999);
    CHECK(l.size() == 8);

    // Deleting every level empties cleanly.
    for (int k : order) l.set(Price::from_parts(100, k * 100), 0);
    CHECK(l.empty());
    CHECK(!l.max().has_value());
}

}  // namespace

int main() {
    std::printf("running C++ tests\n");
    test_price_is_exact();
    test_symbol_padding_and_hash();
    test_deep_decode();
    test_book();
    test_levels_stay_sorted();
    std::printf("%d checks, %d failures\n", g_checks, g_failures);
    return g_failures == 0 ? 0 : 1;
}
