//! Classic pcap (libpcap 2.4) reading, borrowing rather than copying.
//!
//! IEX HIST files are classic pcap, not pcapng. The header of
//! `20170826_IEXTP1_DEEP1.0.pcap` reads:
//!
//! ```text
//! d4c3 b2a1  0200 0400  ...  ffff 0000  0100 0000
//! magic      v2.4            snaplen    linktype 1 (Ethernet)
//! ```
//!
//! Four magics exist and they encode two independent choices — byte order, and
//! whether the sub-second timestamp field counts microseconds or nanoseconds.
//! Reading a nanosecond file as microseconds silently inflates every timestamp
//! by 1000x, which for a latency project would be a quietly wrong answer rather
//! than a crash, so all four are handled explicitly.
//!
//! The reader yields `&[u8]` into the caller's buffer. Nothing is allocated per
//! packet and nothing is copied.

use crate::Error;

const GLOBAL_HEADER_LEN: usize = 24;
const RECORD_HEADER_LEN: usize = 16;

/// Ethernet II. IEX HIST captures are all linktype 1.
pub const LINKTYPE_ETHERNET: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Endian {
    Little,
    Big,
}

/// One captured frame.
#[derive(Debug, Clone, Copy)]
pub struct Packet<'a> {
    /// Capture timestamp in nanoseconds since the Unix epoch.
    ///
    /// Normalised on the way out, so callers never need to know whether the file
    /// stored microseconds or nanoseconds.
    pub ts_nanos: u64,
    /// Bytes actually captured. May be shorter than the frame on the wire when
    /// the capture used a snaplen.
    pub data: &'a [u8],
    /// Frame length on the wire. `data.len() < orig_len` means truncation.
    pub orig_len: u32,
}

/// A borrowed reader over a whole pcap file.
pub struct PcapReader<'a> {
    buf: &'a [u8],
    pos: usize,
    endian: Endian,
    nanos: bool,
    pub linktype: u32,
    pub snaplen: u32,
}

impl<'a> PcapReader<'a> {
    /// Validate the global header and position at the first record.
    pub fn new(buf: &'a [u8]) -> Result<Self, Error> {
        if buf.len() < GLOBAL_HEADER_LEN {
            return Err(Error::Truncated {
                need: GLOBAL_HEADER_LEN,
                got: buf.len(),
            });
        }
        let raw = [buf[0], buf[1], buf[2], buf[3]];
        let (endian, nanos) = match u32::from_be_bytes(raw) {
            0xa1b2_c3d4 => (Endian::Big, false),
            0xa1b2_3c4d => (Endian::Big, true),
            0xd4c3_b2a1 => (Endian::Little, false),
            0x4d3c_b2a1 => (Endian::Little, true),
            other => return Err(Error::BadMagic(other)),
        };
        let mut r = Self {
            buf,
            pos: GLOBAL_HEADER_LEN,
            endian,
            nanos,
            linktype: 0,
            snaplen: 0,
        };
        r.snaplen = r.u32_at(16);
        r.linktype = r.u32_at(20);
        Ok(r)
    }

    #[inline(always)]
    fn u32_at(&self, off: usize) -> u32 {
        let b = [
            self.buf[off],
            self.buf[off + 1],
            self.buf[off + 2],
            self.buf[off + 3],
        ];
        match self.endian {
            Endian::Little => u32::from_le_bytes(b),
            Endian::Big => u32::from_be_bytes(b),
        }
    }

    /// Bytes consumed so far — used to report throughput in GB/s.
    #[inline]
    pub fn position(&self) -> usize {
        self.pos
    }
}

impl<'a> Iterator for PcapReader<'a> {
    type Item = Packet<'a>;

    #[inline]
    fn next(&mut self) -> Option<Packet<'a>> {
        if self.pos + RECORD_HEADER_LEN > self.buf.len() {
            return None;
        }
        let ts_sec = self.u32_at(self.pos) as u64;
        let ts_frac = self.u32_at(self.pos + 4) as u64;
        let incl = self.u32_at(self.pos + 8) as usize;
        let orig_len = self.u32_at(self.pos + 12);
        let start = self.pos + RECORD_HEADER_LEN;

        // A record claiming more bytes than remain means the file was truncated
        // mid-write, which is normal for an interrupted capture. Stop cleanly
        // rather than panicking on the slice.
        let end = start.checked_add(incl)?;
        if end > self.buf.len() {
            return None;
        }
        self.pos = end;

        let ts_nanos = ts_sec * 1_000_000_000 + if self.nanos { ts_frac } else { ts_frac * 1_000 };
        Some(Packet {
            ts_nanos,
            data: &self.buf[start..end],
            orig_len,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(magic: u32, packets: &[&[u8]]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&magic.to_be_bytes());
        v.extend_from_slice(&2u16.to_le_bytes());
        v.extend_from_slice(&4u16.to_le_bytes());
        v.extend_from_slice(&[0; 8]);
        v.extend_from_slice(&65535u32.to_le_bytes());
        v.extend_from_slice(&1u32.to_le_bytes());
        for p in packets {
            v.extend_from_slice(&7u32.to_le_bytes()); // ts_sec
            v.extend_from_slice(&500u32.to_le_bytes()); // ts_frac
            v.extend_from_slice(&(p.len() as u32).to_le_bytes());
            v.extend_from_slice(&(p.len() as u32).to_le_bytes());
            v.extend_from_slice(p);
        }
        v
    }

    #[test]
    fn reads_packets_and_linktype() {
        let f = build(0xd4c3_b2a1, &[b"aaa", b"bbbb"]);
        let r = PcapReader::new(&f).unwrap();
        assert_eq!(r.linktype, LINKTYPE_ETHERNET);
        let got: Vec<&[u8]> = r.map(|p| p.data).collect();
        assert_eq!(got, vec![b"aaa".as_slice(), b"bbbb".as_slice()]);
    }

    /// Misreading a nanosecond file as microseconds inflates timestamps 1000x.
    /// It would not crash, which is exactly why it is worth a test.
    #[test]
    fn microsecond_and_nanosecond_magics_agree_on_scale() {
        let us = build(0xd4c3_b2a1, &[b"x"]);
        let ns = build(0x4d3c_b2a1, &[b"x"]);
        let a = PcapReader::new(&us).unwrap().next().unwrap();
        let b = PcapReader::new(&ns).unwrap().next().unwrap();
        assert_eq!(a.ts_nanos, 7 * 1_000_000_000 + 500_000);
        assert_eq!(b.ts_nanos, 7 * 1_000_000_000 + 500);
    }

    #[test]
    fn bad_magic_is_an_error() {
        assert!(matches!(
            PcapReader::new(&[0u8; 32]),
            Err(Error::BadMagic(_))
        ));
    }

    /// An interrupted capture leaves a half-written record. Stop, do not panic.
    #[test]
    fn truncated_final_record_stops_cleanly() {
        let mut f = build(0xd4c3_b2a1, &[b"aaa", b"bbbb"]);
        f.truncate(f.len() - 2);
        assert_eq!(PcapReader::new(&f).unwrap().count(), 1);
    }

    #[test]
    fn every_prefix_is_safe() {
        let f = build(0xd4c3_b2a1, &[b"aaa", b"bbbb"]);
        for n in 0..f.len() {
            if let Ok(r) = PcapReader::new(&f[..n]) {
                let _ = r.count(); // must not panic
            }
        }
    }
}
