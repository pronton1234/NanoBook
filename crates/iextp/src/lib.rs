//! IEX-TP v1 transport: pcap framing, segment sequencing, A/B feed arbitration.
//!
//! This is the layer most order-book projects skip. They open a file of already
//! de-framed messages and start parsing, which misses everything the transport
//! actually does: market data arrives as UDP multicast, it is published twice
//! down separate paths, packets are reordered and dropped, and a receiver has to
//! reconcile all of that before a single book update is correct.
//!
//! The stack, outermost first:
//!
//! ```text
//!   pcap record          16-byte header, timestamps in us or ns   [pcap]
//!     Ethernet II        + 802.1Q VLAN tag on every IEX frame     [frame]
//!       IPv4             variable header length, multicast dst    [frame]
//!         UDP            length field, not the frame length       [frame]
//!           IEX-TP       40-byte header, LITTLE-endian            [segment]
//!             messages   u16 length prefix each                   [segment]
//! ```
//!
//! Byte order flips at the IEX-TP boundary: everything above it is big-endian,
//! everything from it down is little-endian. Carrying one convention through the
//! whole packet is wrong on one side.
//!
//! Nothing here allocates per packet. Every layer narrows a borrow of the mapped
//! file, so a message body is a subslice of the original bytes.

pub mod frame;
pub mod pcap;
pub mod segment;
pub mod sequencer;

pub use frame::{Datagram, FrameSkip};
pub use pcap::{Packet, PcapReader, LINKTYPE_ETHERNET};
pub use segment::{Segment, HEADER_LEN, PROTOCOL_DEEP, PROTOCOL_TOPS};
pub use sequencer::{Action, Sequencer, Stats};

/// Errors that mean the input is malformed, as opposed to merely uninteresting.
///
/// Frames that are not feed traffic are reported through [`FrameSkip`] instead:
/// a capture legitimately contains ARP and IPv6 alongside the feed, and treating
/// that as an error would drown the real ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("not a pcap file: magic 0x{0:08x}")]
    BadMagic(u32),
    #[error("truncated: needed {need} bytes, have {got}")]
    Truncated { need: usize, got: usize },
}
