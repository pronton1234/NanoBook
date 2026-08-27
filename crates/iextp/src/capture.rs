//! One reader over both capture formats.
//!
//! IEX changed format between 2017 and 2018 — the 2017 fixture is classic pcap,
//! the 2018 file is pcapng — and a downstream pipeline should not have to know
//! or care which it was handed. The format is decided by the first four bytes
//! and everything above this point sees one iterator of [`Packet`].
//!
//! Sniffing is by magic rather than by file extension, because both formats are
//! distributed as `.pcap` here: `20180607_IEXTP1_DEEP1.0.pcap` is pcapng despite
//! its name. Trusting the extension would reintroduce exactly the misparse this
//! module exists to prevent.

use crate::pcap::{Packet, PcapReader};
use crate::pcapng::PcapNgReader;
use crate::Error;

/// Which on-disk format a capture turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// libpcap 2.4, the classic format.
    Pcap,
    /// pcapng, block-structured.
    PcapNg,
}

/// A reader over a capture in either supported format.
pub enum Capture<'a> {
    Pcap(PcapReader<'a>),
    PcapNg(PcapNgReader<'a>),
}

impl<'a> Capture<'a> {
    /// Detect the format from the leading bytes and open the right reader.
    pub fn open(buf: &'a [u8]) -> Result<Self, Error> {
        if buf.len() >= 4 {
            // pcapng's Section Header Block type is byte-order independent.
            let le = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
            if le == 0x0a0d_0d0a {
                return Ok(Capture::PcapNg(PcapNgReader::new(buf)?));
            }
        }
        Ok(Capture::Pcap(PcapReader::new(buf)?))
    }

    pub fn format(&self) -> Format {
        match self {
            Capture::Pcap(_) => Format::Pcap,
            Capture::PcapNg(_) => Format::PcapNg,
        }
    }

    /// Link-layer type. IEX captures are Ethernet (1).
    pub fn linktype(&self) -> u32 {
        match self {
            Capture::Pcap(r) => r.linktype,
            Capture::PcapNg(r) => r.linktype,
        }
    }

    pub fn snaplen(&self) -> u32 {
        match self {
            Capture::Pcap(r) => r.snaplen,
            Capture::PcapNg(r) => r.snaplen,
        }
    }
}

impl<'a> Iterator for Capture<'a> {
    type Item = Packet<'a>;

    #[inline]
    fn next(&mut self) -> Option<Packet<'a>> {
        match self {
            Capture::Pcap(r) => r.next(),
            Capture::PcapNg(r) => r.next(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classic_pcap_is_detected() {
        let mut f = Vec::new();
        f.extend_from_slice(&0xd4c3_b2a1u32.to_be_bytes());
        f.extend_from_slice(&2u16.to_le_bytes());
        f.extend_from_slice(&4u16.to_le_bytes());
        f.extend_from_slice(&[0; 8]);
        f.extend_from_slice(&65535u32.to_le_bytes());
        f.extend_from_slice(&1u32.to_le_bytes());
        let c = Capture::open(&f).unwrap();
        assert_eq!(c.format(), Format::Pcap);
        assert_eq!(c.linktype(), 1);
    }

    #[test]
    fn pcapng_is_detected() {
        let mut body = Vec::new();
        body.extend_from_slice(&0x1a2b_3c4du32.to_le_bytes());
        body.extend_from_slice(&1u16.to_le_bytes());
        body.extend_from_slice(&0u16.to_le_bytes());
        body.extend_from_slice(&(-1i64).to_le_bytes());
        let total = 12 + body.len();
        let mut f = Vec::new();
        f.extend_from_slice(&0x0a0d_0d0au32.to_le_bytes());
        f.extend_from_slice(&(total as u32).to_le_bytes());
        f.extend_from_slice(&body);
        f.extend_from_slice(&(total as u32).to_le_bytes());
        assert_eq!(Capture::open(&f).unwrap().format(), Format::PcapNg);
    }

    #[test]
    fn unrecognised_magic_errors() {
        assert!(matches!(Capture::open(&[0u8; 32]), Err(Error::BadMagic(_))));
    }
}
