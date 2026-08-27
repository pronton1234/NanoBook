//! pcapng reading, for the captures classic pcap cannot express.
//!
//! IEX changed capture format somewhere between 2017 and 2018. The 2017 fixture
//! is classic pcap (`d4c3b2a1`); `20180607_IEXTP1_DEEP1.0.pcap` opens with
//! `0a0d0d0a`, a pcapng Section Header Block. A reader that assumes classic pcap
//! does not fail on such a file — it walks a 24-byte header and 16-byte record
//! headers over pcapng blocks and emits plausible-looking garbage. That is
//! precisely what an independent Python decoder did here, inventing dozens of
//! nonexistent message types, while the Rust reader rejected the file outright.
//! Refusing an unrecognised magic is the whole reason the difference was visible.
//!
//! Two things in this format bite, and both are silent:
//!
//! * **Timestamp resolution is per-interface**, declared by the `if_tsresol`
//!   option in each Interface Description Block, and it defaults to microseconds
//!   when absent. Reading a microsecond capture as nanoseconds — or the reverse —
//!   scales every timestamp by 1000 without any error, which for a latency
//!   project is a wrong answer rather than a crash.
//! * **Blocks are padded to a 4-byte boundary** and carry their length both
//!   before and after the body. Walking by the leading length alone works until
//!   an option list is unpadded, at which point the reader slides off by bytes.
//!
//! The SHB of the IEX file also records `File created by merging:`, so these
//! captures are assembled from several capture points rather than taken at one.

use crate::Error;

const BT_SECTION_HEADER: u32 = 0x0a0d_0d0a;
const BT_INTERFACE_DESC: u32 = 0x0000_0001;
const BT_ENHANCED_PACKET: u32 = 0x0000_0006;
const BT_SIMPLE_PACKET: u32 = 0x0000_0003;

/// Byte-order magic inside the Section Header Block.
const BYTE_ORDER_MAGIC: u32 = 0x1a2b_3c4d;
/// `if_tsresol`.
const OPT_IF_TSRESOL: u16 = 9;

/// Per-interface state. `if_tsresol` is declared per interface, so a capture
/// merged from several sources may legitimately mix resolutions.
#[derive(Debug, Clone, Copy)]
struct Interface {
    linktype: u32,
    /// Nanoseconds per timestamp unit.
    nanos_per_unit: u64,
}

impl Default for Interface {
    fn default() -> Self {
        // pcapng's default when if_tsresol is absent is 10^-6.
        Self {
            linktype: 0,
            nanos_per_unit: 1_000,
        }
    }
}

/// A borrowed pcapng reader.
pub struct PcapNgReader<'a> {
    buf: &'a [u8],
    pos: usize,
    little: bool,
    interfaces: Vec<Interface>,
    /// Linktype of the first interface, for reporting.
    pub linktype: u32,
    pub snaplen: u32,
}

#[inline(always)]
fn u16_at(b: &[u8], o: usize, little: bool) -> u16 {
    let v = [b[o], b[o + 1]];
    if little {
        u16::from_le_bytes(v)
    } else {
        u16::from_be_bytes(v)
    }
}

#[inline(always)]
fn u32_at(b: &[u8], o: usize, little: bool) -> u32 {
    let v = [b[o], b[o + 1], b[o + 2], b[o + 3]];
    if little {
        u32::from_le_bytes(v)
    } else {
        u32::from_be_bytes(v)
    }
}

/// Round a length up to the next 4-byte boundary.
#[inline(always)]
const fn pad4(n: usize) -> usize {
    (n + 3) & !3
}

impl<'a> PcapNgReader<'a> {
    pub fn new(buf: &'a [u8]) -> Result<Self, Error> {
        if buf.len() < 12 {
            return Err(Error::Truncated {
                need: 12,
                got: buf.len(),
            });
        }
        // The block type is byte-order independent by construction; the byte
        // order magic that follows tells us how to read everything else.
        if u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) != BT_SECTION_HEADER {
            return Err(Error::BadMagic(u32::from_be_bytes([
                buf[0], buf[1], buf[2], buf[3],
            ])));
        }
        let little = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]) == BYTE_ORDER_MAGIC;
        if !little && u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]) != BYTE_ORDER_MAGIC {
            return Err(Error::BadMagic(u32::from_be_bytes([
                buf[8], buf[9], buf[10], buf[11],
            ])));
        }
        let mut r = Self {
            buf,
            pos: 0,
            little,
            interfaces: Vec::new(),
            linktype: 0,
            snaplen: 0,
        };
        r.prescan_first_idb();
        Ok(r)
    }

    /// Populate `linktype`/`snaplen` from the first Interface Description Block.
    ///
    /// Classic pcap carries both in its global header, so they are known the
    /// moment the file is opened. pcapng puts them in a block that iteration has
    /// to reach, which meant a caller printing `linktype` before consuming any
    /// packet saw `0` — and `0` is BSD loopback, a real link type, so the wrong
    /// answer looked like a plausible one. A short forward scan restores the
    /// classic-pcap contract without disturbing the iteration cursor.
    fn prescan_first_idb(&mut self) {
        let mut pos = 0usize;
        while pos + 12 <= self.buf.len() {
            let btype = u32_at(self.buf, pos, self.little);
            let blen = u32_at(self.buf, pos + 4, self.little) as usize;
            if blen < 12 || pos + blen > self.buf.len() {
                return;
            }
            if btype == BT_INTERFACE_DESC {
                let body = &self.buf[pos + 8..pos + blen - 4];
                if body.len() >= 8 {
                    self.linktype = u16_at(body, 0, self.little) as u32;
                    self.snaplen = u32_at(body, 4, self.little);
                }
                return;
            }
            // Only the section header may precede the first interface.
            if btype != BT_SECTION_HEADER {
                return;
            }
            pos += blen;
        }
    }

    /// Parse an Interface Description Block, honouring `if_tsresol`.
    fn read_idb(&mut self, body: &[u8]) {
        let mut iface = Interface {
            linktype: u16_at(body, 0, self.little) as u32,
            ..Interface::default()
        };
        let snaplen = if body.len() >= 8 {
            u32_at(body, 4, self.little)
        } else {
            0
        };

        // Options begin after linktype(2) + reserved(2) + snaplen(4).
        let mut p = 8;
        while p + 4 <= body.len() {
            let code = u16_at(body, p, self.little);
            let len = u16_at(body, p + 2, self.little) as usize;
            if code == 0 {
                break; // opt_endofopt
            }
            if p + 4 + len > body.len() {
                break;
            }
            if code == OPT_IF_TSRESOL && len >= 1 {
                let raw = body[p + 4];
                let exp = (raw & 0x7f) as u32;
                iface.nanos_per_unit = if raw & 0x80 != 0 {
                    // 2^-exp seconds per unit.
                    1_000_000_000u64 / 2u64.saturating_pow(exp).max(1)
                } else {
                    // 10^-exp seconds per unit.
                    1_000_000_000u64 / 10u64.saturating_pow(exp.min(9)).max(1)
                };
            }
            p += 4 + pad4(len);
        }

        if self.interfaces.is_empty() {
            self.linktype = iface.linktype;
            self.snaplen = snaplen;
        }
        self.interfaces.push(iface);
    }

    #[inline]
    pub fn position(&self) -> usize {
        self.pos
    }
}

impl<'a> Iterator for PcapNgReader<'a> {
    type Item = crate::pcap::Packet<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.pos + 8 > self.buf.len() {
                return None;
            }
            let btype = u32_at(self.buf, self.pos, self.little);
            let blen = u32_at(self.buf, self.pos + 4, self.little) as usize;
            // A block is at minimum type+len+len = 12 bytes; anything shorter
            // would not advance and would spin forever.
            if blen < 12 || self.pos + blen > self.buf.len() {
                return None;
            }
            let body = &self.buf[self.pos + 8..self.pos + blen - 4];
            let here = self.pos;
            self.pos += blen;

            match btype {
                BT_SECTION_HEADER => {
                    // A new section may switch byte order and resets interfaces.
                    if body.len() >= 4 {
                        self.little = u32::from_le_bytes([body[0], body[1], body[2], body[3]])
                            == BYTE_ORDER_MAGIC;
                    }
                    self.interfaces.clear();
                }
                BT_INTERFACE_DESC => self.read_idb(body),
                BT_ENHANCED_PACKET => {
                    if body.len() < 20 {
                        continue;
                    }
                    let iface_id = u32_at(body, 0, self.little) as usize;
                    let ts_hi = u32_at(body, 4, self.little) as u64;
                    let ts_lo = u32_at(body, 8, self.little) as u64;
                    let cap_len = u32_at(body, 12, self.little) as usize;
                    let orig_len = u32_at(body, 16, self.little);
                    if 20 + cap_len > body.len() {
                        continue;
                    }
                    let units = (ts_hi << 32) | ts_lo;
                    let nanos_per_unit = self
                        .interfaces
                        .get(iface_id)
                        .copied()
                        .unwrap_or_default()
                        .nanos_per_unit;
                    // Absolute offset of the packet bytes, so the returned slice
                    // borrows `self.buf` rather than the local `body`.
                    let start = here + 8 + 20;
                    return Some(crate::pcap::Packet {
                        ts_nanos: units.saturating_mul(nanos_per_unit),
                        data: &self.buf[start..start + cap_len],
                        orig_len,
                    });
                }
                BT_SIMPLE_PACKET => {
                    if body.len() < 4 {
                        continue;
                    }
                    let orig_len = u32_at(body, 0, self.little);
                    let start = here + 8 + 4;
                    let cap_len = (blen - 16).min(orig_len as usize);
                    return Some(crate::pcap::Packet {
                        ts_nanos: 0, // simple packet blocks carry no timestamp
                        data: &self.buf[start..start + cap_len],
                        orig_len,
                    });
                }
                _ => {} // name resolution, statistics, custom blocks: skip
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(btype: u32, body: &[u8]) -> Vec<u8> {
        let total = 12 + pad4(body.len());
        let mut v = Vec::new();
        v.extend_from_slice(&btype.to_le_bytes());
        v.extend_from_slice(&(total as u32).to_le_bytes());
        v.extend_from_slice(body);
        v.resize(8 + pad4(body.len()), 0);
        v.extend_from_slice(&(total as u32).to_le_bytes());
        v
    }

    fn shb() -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&BYTE_ORDER_MAGIC.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&0u16.to_le_bytes());
        b.extend_from_slice(&(-1i64).to_le_bytes());
        block(BT_SECTION_HEADER, &b)
    }

    fn idb(tsresol: Option<u8>) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&1u16.to_le_bytes()); // linktype Ethernet
        b.extend_from_slice(&0u16.to_le_bytes());
        b.extend_from_slice(&65535u32.to_le_bytes());
        if let Some(r) = tsresol {
            b.extend_from_slice(&OPT_IF_TSRESOL.to_le_bytes());
            b.extend_from_slice(&1u16.to_le_bytes());
            b.push(r);
            b.extend_from_slice(&[0, 0, 0]); // pad to 4
            b.extend_from_slice(&0u16.to_le_bytes()); // opt_endofopt
            b.extend_from_slice(&0u16.to_le_bytes());
        }
        block(BT_INTERFACE_DESC, &b)
    }

    fn epb(units: u64, payload: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&0u32.to_le_bytes()); // interface id
        b.extend_from_slice(&((units >> 32) as u32).to_le_bytes());
        b.extend_from_slice(&(units as u32).to_le_bytes());
        b.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        b.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        b.extend_from_slice(payload);
        block(BT_ENHANCED_PACKET, &b)
    }

    #[test]
    fn reads_packets_and_linktype() {
        let mut f = shb();
        f.extend(idb(Some(6)));
        f.extend(epb(1, b"aaa"));
        f.extend(epb(2, b"bbbb"));
        let mut r = PcapNgReader::new(&f).unwrap();
        let got: Vec<&[u8]> = r.by_ref().map(|p| p.data).collect();
        assert_eq!(got, vec![b"aaa".as_slice(), b"bbbb".as_slice()]);
        assert_eq!(r.linktype, 1);
    }

    /// linktype must be known before iteration, as it is for classic pcap.
    /// Returning 0 here is not a harmless default -- 0 is BSD loopback, so the
    /// wrong answer is indistinguishable from a real one.
    #[test]
    fn linktype_is_known_before_iterating() {
        let mut f = shb();
        f.extend(idb(Some(6)));
        f.extend(epb(1, b"aaa"));
        let r = PcapNgReader::new(&f).unwrap();
        assert_eq!(
            r.linktype, 1,
            "linktype available without consuming a packet"
        );
        assert_eq!(r.snaplen, 65535);
    }

    /// The silent 1000x bug: `if_tsresol` decides the scale, and its absence
    /// means microseconds, not nanoseconds.
    #[test]
    fn timestamp_resolution_is_honoured() {
        for (res, want) in [(Some(6), 1_000_000u64), (Some(9), 1_000), (None, 1_000_000)] {
            let mut f = shb();
            f.extend(idb(res));
            f.extend(epb(1_000, b"x"));
            let p = PcapNgReader::new(&f).unwrap().next().unwrap();
            assert_eq!(p.ts_nanos, want, "if_tsresol {res:?}");
        }
    }

    /// Blocks pcapng defines but we do not consume must be stepped over, not
    /// misread as packets.
    #[test]
    fn unknown_blocks_are_skipped() {
        let mut f = shb();
        f.extend(idb(Some(6)));
        f.extend(block(0x0000_0004, &[0u8; 16])); // interface statistics
        f.extend(epb(1, b"payload"));
        assert_eq!(PcapNgReader::new(&f).unwrap().count(), 1);
    }

    #[test]
    fn classic_pcap_magic_is_rejected() {
        let f = [0xd4u8, 0xc3, 0xb2, 0xa1, 0, 0, 0, 0, 0, 0, 0, 0];
        assert!(matches!(PcapNgReader::new(&f), Err(Error::BadMagic(_))));
    }

    #[test]
    fn every_prefix_is_safe() {
        let mut f = shb();
        f.extend(idb(Some(6)));
        f.extend(epb(1, b"aaa"));
        for n in 0..f.len() {
            if let Ok(r) = PcapNgReader::new(&f[..n]) {
                let _ = r.count(); // must terminate and must not panic
            }
        }
    }
}
