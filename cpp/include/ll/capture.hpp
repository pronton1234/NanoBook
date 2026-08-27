// Reading a capture: pcap record, Ethernet/VLAN/IPv4/UDP, IEX-TP segment.
//
// Every offset here was checked against real IEX captures, and one of them is
// not in any specification: **every IEX frame carries an 802.1Q VLAN tag**. A
// parser assuming IPv4 begins at the usual offset 14 lands four bytes early,
// inside the VLAN header, and misparses every packet in the file.
//
// Byte order flips mid-packet. Ethernet, IPv4 and UDP are big-endian; IEX-TP
// and everything below it is little-endian. Carrying one convention through the
// whole packet is wrong on one side of that boundary.
//
// Nothing here allocates or copies: every layer returns a std::span narrowing
// the caller's buffer.
#pragma once

#include <cstdint>
#include <cstring>
#include <optional>
#include <span>

namespace ll {

using Bytes = std::span<const std::uint8_t>;

namespace detail {

/// Unaligned loads, done by memcpy so the compiler emits a single load rather
/// than the caller inventing undefined behaviour with a reinterpret_cast.
template <typename T>
[[nodiscard]] inline T load(const std::uint8_t* p) noexcept {
    T v;
    std::memcpy(&v, p, sizeof(T));
    return v;
}

[[nodiscard]] inline std::uint16_t be16(const std::uint8_t* p) noexcept {
    return static_cast<std::uint16_t>((p[0] << 8) | p[1]);
}

}  // namespace detail

// ---------------------------------------------------------------- pcap ------

/// One captured frame.
struct Packet {
    std::uint64_t ts_nanos = 0;
    Bytes data;
};

/// Classic pcap reader. Borrows; does not own.
///
/// Four magics exist, encoding two independent choices: byte order, and whether
/// the sub-second field counts microseconds or nanoseconds. Reading a nanosecond
/// file as microseconds inflates every timestamp by 1000 without any error --
/// for a latency project, a quietly wrong answer rather than a crash.
class PcapReader {
public:
    static std::optional<PcapReader> open(Bytes buf) noexcept;

    [[nodiscard]] std::optional<Packet> next() noexcept;
    [[nodiscard]] std::uint32_t linktype() const noexcept { return linktype_; }

private:
    PcapReader() = default;
    [[nodiscard]] std::uint32_t u32_at(std::size_t off) const noexcept;

    Bytes buf_;
    std::size_t pos_ = 0;
    bool little_ = true;
    bool nanos_ = false;
    std::uint32_t linktype_ = 0;
};

// --------------------------------------------------------------- frame ------

/// A UDP datagram located inside an Ethernet frame.
struct Datagram {
    std::uint16_t dst_port = 0;
    Bytes payload;
};

/// Strip Ethernet, any stacked VLAN tags, IPv4 and UDP.
///
/// Returns nullopt for frames that are simply not feed traffic, which is routine
/// on a real capture: a link carries ARP and IPv6 alongside the feed.
[[nodiscard]] std::optional<Datagram> parse_frame(Bytes frame) noexcept;

// ------------------------------------------------------------- segment ------

/// IEX-TP message protocol id for DEEP.
inline constexpr std::uint16_t kProtocolDeep = 0x8004;
inline constexpr std::size_t kSegmentHeaderLen = 40;

/// An IEX-TP segment: a 40-byte little-endian header, then length-prefixed
/// messages.
struct Segment {
    std::uint16_t protocol = 0;
    std::uint32_t session = 0;
    std::uint16_t message_count = 0;
    std::int64_t first_sequence = 0;
    Bytes messages;

    /// message_count may be zero. That is a heartbeat, not an error and not a
    /// gap -- it is how the session stays live when a symbol is quiet.
    [[nodiscard]] bool is_heartbeat() const noexcept { return message_count == 0; }
    [[nodiscard]] std::int64_t next_sequence() const noexcept {
        return first_sequence + message_count;
    }
};

[[nodiscard]] std::optional<Segment> parse_segment(Bytes payload) noexcept;

/// Iterate the u16-length-prefixed message bodies inside one segment.
class MessageIter {
public:
    explicit MessageIter(const Segment& s) noexcept
        : buf_(s.messages), remaining_(s.message_count) {}

    [[nodiscard]] std::optional<Bytes> next() noexcept;

private:
    Bytes buf_;
    std::size_t pos_ = 0;
    std::uint16_t remaining_ = 0;
};

}  // namespace ll
