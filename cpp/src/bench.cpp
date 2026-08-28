// The C++ side of the cross-language comparison.
//
// Reports the same figures as the Rust `budget` example, over the same capture,
// with the same discipline: warm before timing, round-robin repetitions, and a
// run-to-run spread printed beside every number so a difference smaller than the
// noise is not read as a result.
//
//     ./ll_bench <capture.pcap> [reps]
#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <fstream>
#include <string>
#include <vector>

#include "nb/book.hpp"
#include "nb/capture.hpp"
#include "nb/deep.hpp"

namespace {

std::vector<std::uint8_t> read_file(const char* path) {
    std::ifstream f(path, std::ios::binary | std::ios::ate);
    if (!f) return {};
    const auto n = static_cast<std::size_t>(f.tellg());
    f.seekg(0);
    std::vector<std::uint8_t> buf(n);
    f.read(reinterpret_cast<char*>(buf.data()), static_cast<std::streamsize>(n));
    return buf;
}

struct Counters {
    std::uint64_t packets = 0;
    std::uint64_t messages = 0;
    std::uint64_t book_updates = 0;
    std::uint64_t crossed = 0;
    std::size_t levels = 0;
    std::uint64_t total_size = 0;
};

/// One full pass: pcap record -> frame -> segment -> DEEP -> book.
Counters run_once(const std::vector<nb::Bytes>& frames) {
    nb::Book book(8192);
    Counters c;
    for (const auto& f : frames) {
        ++c.packets;
        const auto dg = nb::parse_frame(f);
        if (!dg) continue;
        const auto seg = nb::parse_segment(dg->payload);
        if (!seg) continue;
        nb::MessageIter it(*seg);
        while (const auto body = it.next()) {
            const auto msg = nb::parse_message(*body);
            if (!msg) continue;
            ++c.messages;
            if (const auto* u = std::get_if<nb::PriceLevelUpdate>(&*msg)) {
                book.apply(*u);
                ++c.book_updates;
            }
        }
    }
    c.crossed = book.stats.crossed_when_stable;
    c.levels = book.total_levels();
    c.total_size = book.total_size();
    return c;
}

}  // namespace

int main(int argc, char** argv) {
    if (argc < 2) {
        std::fprintf(stderr, "usage: ll_bench <capture.pcap> [reps]\n");
        return 2;
    }
    const int reps = argc > 2 ? std::atoi(argv[2]) : 7;

    const auto buf = read_file(argv[1]);
    if (buf.empty()) {
        std::fprintf(stderr, "%s: cannot read\n", argv[1]);
        return 1;
    }

    // Collect frames up front so the timed loop measures the pipeline, not the
    // capture reader.
    std::vector<nb::Bytes> frames;
    {
        auto r = nb::PcapReader::open(nb::Bytes(buf));
        if (!r) {
            std::fprintf(stderr, "%s: not a pcap file\n", argv[1]);
            return 1;
        }
        while (const auto p = r->next()) frames.push_back(p->data);
    }
    std::printf("%s\n  %zu packets\n\n", argv[1], frames.size());

    // Warm before timing. The first pass faults in every page of the symbol
    // table and level storage; timing it measures the allocator.
    const Counters warm = run_once(frames);

    double best = 1e18, worst = 0.0;
    for (int i = 0; i < reps; ++i) {
        const auto t0 = std::chrono::steady_clock::now();
        const Counters c = run_once(frames);
        const auto t1 = std::chrono::steady_clock::now();
        // Keep the result observable so the optimiser cannot delete the work.
        asm volatile("" : : "r"(&c) : "memory");
        const double ns =
            std::chrono::duration<double, std::nano>(t1 - t0).count() /
            static_cast<double>(frames.size());
        if (ns < best) best = ns;
        if (ns > worst) worst = ns;
    }

    std::printf("  %-26s %10.2f ns/packet\n", "tick-to-book (C++)", best);
    std::printf("  %-26s %10.2f ns   run-to-run\n", "spread", worst - best);
    std::printf("  %-26s %10.2f ns/message\n", "per message",
                best * static_cast<double>(frames.size()) /
                    static_cast<double>(warm.messages ? warm.messages : 1));
    std::printf("\n  messages       %12llu\n", (unsigned long long)warm.messages);
    std::printf("  book updates   %12llu\n", (unsigned long long)warm.book_updates);
    std::printf("  levels held    %12zu\n", warm.levels);
    std::printf("  total size     %12llu\n", (unsigned long long)warm.total_size);
    std::printf("  crossed@stable %12llu  %s\n", (unsigned long long)warm.crossed,
                warm.crossed == 0 ? "<- clean" : "<- BOOK CORRUPTION");
    return 0;
}
