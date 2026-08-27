//! Stripping Ethernet/VLAN/IPv4/UDP down to the transport payload.
//!
//! Every byte offset here was read off a real IEX capture rather than a
//! specification, because the specification does not mention the thing that
//! breaks naive parsers: **IEX frames carry an 802.1Q VLAN tag.** In
//! `20170826_IEXTP1_DEEP1.0.pcap` all 20,145 frames are tagged, so a reader that
//! assumes the IPv4 header begins at the usual offset 14 lands four bytes early,
//! in the middle of the VLAN header, and misparses every packet in the file.
//!
//! The tag is detected rather than assumed, because a capture from a different
//! tap point may well not have it.
//!
//! No allocation and no copying happens here. Every function narrows a borrow of
//! the caller's buffer, so the whole path from mapped file to message body is a
//! series of subslices.

/// EtherType marking an 802.1Q VLAN tag, whose 4 bytes precede the real type.
const ETHERTYPE_VLAN: u16 = 0x8100;
/// EtherType for IPv4.
const ETHERTYPE_IPV4: u16 = 0x0800;
/// IANA protocol number for UDP.
const IPPROTO_UDP: u8 = 17;

const ETH_HEADER_LEN: usize = 14;
const VLAN_TAG_LEN: usize = 4;
const UDP_HEADER_LEN: usize = 8;

/// Why a frame carried no transport payload.
///
/// These are *expected* outcomes on a real capture, not failures: a link carries
/// ARP, IPv6 and other traffic alongside the feed. The caller counts them and
/// moves on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameSkip {
    TooShort,
    NotIpv4,
    NotUdp,
    Fragmented,
}

/// A UDP datagram located inside an Ethernet frame.
#[derive(Debug, Clone, Copy)]
pub struct Datagram<'a> {
    pub src_port: u16,
    pub dst_port: u16,
    /// Destination IPv4 address, big-endian as it appears on the wire.
    ///
    /// The feed is multicast, so this identifies *which* multicast group — which
    /// is how the two sides of an A/B feed pair are told apart.
    pub dst_ip: [u8; 4],
    pub payload: &'a [u8],
}

/// Read a big-endian `u16` at `off`.
///
/// `from_be_bytes` over a fixed-size array is the safe way to do an unaligned
/// load: the compiler lowers it to a single unaligned read plus a byte swap, so
/// nothing is gained by reaching for `read_unaligned` and the UB risk vanishes.
#[inline(always)]
fn be16(buf: &[u8], off: usize) -> u16 {
    u16::from_be_bytes([buf[off], buf[off + 1]])
}

/// Locate the UDP payload inside one Ethernet frame.
///
/// Returns `Err` when the frame simply is not feed traffic, which is routine.
#[inline]
pub fn parse(frame: &[u8]) -> Result<Datagram<'_>, FrameSkip> {
    if frame.len() < ETH_HEADER_LEN {
        return Err(FrameSkip::TooShort);
    }

    // Walk past however many VLAN tags are stacked here. QinQ (two tags) is
    // legal and cheap to support, so the loop costs nothing over an `if` and
    // removes a whole class of "works on my capture" bug.
    let mut off = ETH_HEADER_LEN;
    let mut ethertype = be16(frame, 12);
    while ethertype == ETHERTYPE_VLAN {
        if frame.len() < off + VLAN_TAG_LEN {
            return Err(FrameSkip::TooShort);
        }
        ethertype = be16(frame, off + 2);
        off += VLAN_TAG_LEN;
    }
    if ethertype != ETHERTYPE_IPV4 {
        return Err(FrameSkip::NotIpv4);
    }

    // IPv4. The header length is variable (options), so it must be read from the
    // low nibble of byte 0 rather than assumed to be 20.
    if frame.len() < off + 20 {
        return Err(FrameSkip::TooShort);
    }
    let ip = &frame[off..];
    let ihl = ((ip[0] & 0x0f) as usize) * 4;
    if ihl < 20 || ip.len() < ihl {
        return Err(FrameSkip::TooShort);
    }
    if ip[9] != IPPROTO_UDP {
        return Err(FrameSkip::NotUdp);
    }
    // A fragmented datagram cannot be parsed from this frame alone. Silently
    // treating fragment 0 as a whole message would corrupt the stream, so it is
    // reported instead. (More-fragments bit, or a non-zero fragment offset.)
    if ip[6] & 0x20 != 0 || (be16(ip, 6) & 0x1fff) != 0 {
        return Err(FrameSkip::Fragmented);
    }
    let dst_ip = [ip[16], ip[17], ip[18], ip[19]];

    let udp = &ip[ihl..];
    if udp.len() < UDP_HEADER_LEN {
        return Err(FrameSkip::TooShort);
    }
    // Trust the UDP length field over the captured length: Ethernet pads frames
    // up to 60 bytes, and treating that padding as payload appends garbage to
    // short datagrams.
    let udp_len = be16(udp, 4) as usize;
    if udp_len < UDP_HEADER_LEN || udp_len > udp.len() {
        return Err(FrameSkip::TooShort);
    }

    Ok(Datagram {
        src_port: be16(udp, 0),
        dst_port: be16(udp, 2),
        dst_ip,
        payload: &udp[UDP_HEADER_LEN..udp_len],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a frame: optional VLAN, IPv4, UDP, then `payload`.
    fn frame(vlan: bool, payload: &[u8]) -> Vec<u8> {
        let mut f = vec![0u8; 12];
        if vlan {
            f.extend_from_slice(&ETHERTYPE_VLAN.to_be_bytes());
            f.extend_from_slice(&[0x03, 0xf5]); // priority + VLAN id
        }
        f.extend_from_slice(&ETHERTYPE_IPV4.to_be_bytes());
        let ip_start = f.len();
        f.extend_from_slice(&[0x45, 0, 0, 0, 0, 0, 0, 0, 64, IPPROTO_UDP, 0, 0]);
        f.extend_from_slice(&[10, 0, 0, 1]); // src
        f.extend_from_slice(&[233, 215, 21, 4]); // dst (multicast)
        let udp_len = (UDP_HEADER_LEN + payload.len()) as u16;
        f.extend_from_slice(&[0x28, 0x8a]); // src port
        f.extend_from_slice(&10378u16.to_be_bytes()); // dst port
        f.extend_from_slice(&udp_len.to_be_bytes());
        f.extend_from_slice(&[0, 0]); // checksum
        f.extend_from_slice(payload);
        let total = (f.len() - ip_start) as u16;
        f[ip_start + 2..ip_start + 4].copy_from_slice(&total.to_be_bytes());
        f
    }

    #[test]
    fn untagged_frame_parses() {
        let f = frame(false, b"hello");
        let d = parse(&f).unwrap();
        assert_eq!(d.dst_port, 10378);
        assert_eq!(d.payload, b"hello");
    }

    /// The trap this module exists for: every real IEX frame is VLAN tagged, and
    /// an offset-14 assumption misparses all of them.
    #[test]
    fn vlan_tagged_frame_parses_identically() {
        let f = frame(true, b"hello");
        let d = parse(&f).unwrap();
        assert_eq!(d.dst_port, 10378);
        assert_eq!(d.payload, b"hello");
        assert_eq!(d.dst_ip, [233, 215, 21, 4]);
    }

    /// Ethernet pads to a 60-byte minimum. The UDP length field, not the frame
    /// length, decides where the payload ends.
    #[test]
    fn ethernet_padding_is_not_payload() {
        let mut f = frame(true, b"ab");
        f.resize(60, 0);
        assert_eq!(parse(&f).unwrap().payload, b"ab");
    }

    #[test]
    fn non_udp_and_fragments_are_skipped() {
        // Offset of the IPv4 header in a VLAN-tagged frame: 14 + 4.
        const IP: usize = 18;

        let mut tcp = frame(true, b"x");
        tcp[IP + 9] = 6; // protocol = TCP
        assert!(matches!(parse(&tcp), Err(FrameSkip::NotUdp)));

        let mut more_frags = frame(true, b"x");
        more_frags[IP + 6] |= 0x20;
        assert!(matches!(parse(&more_frags), Err(FrameSkip::Fragmented)));

        // A non-zero fragment offset is the tail of a datagram, equally unusable.
        let mut tail = frame(true, b"x");
        tail[IP + 7] = 0x10;
        assert!(matches!(parse(&tail), Err(FrameSkip::Fragmented)));
    }

    #[test]
    fn truncated_frames_do_not_panic() {
        let f = frame(true, b"payload");
        for n in 0..f.len() {
            let _ = parse(&f[..n]); // must not panic
        }
    }
}
