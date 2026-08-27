//! The IEX-TP v1 segment: a 40-byte header followed by length-prefixed messages.
//!
//! Field layout confirmed against `20170826_IEXTP1_DEEP1.0.pcap`, whose first
//! segment decodes as `version=1 message_protocol_id=0x8004 channel=1
//! session=1140588544`, on UDP port 10378.
//!
//! Everything is **little-endian**, which is worth stating because the layers
//! underneath it are not: Ethernet, IPv4 and UDP are all big-endian, so a parser
//! that carries one convention across the whole packet is wrong on one side of
//! the boundary.
//!
//! Two properties of the format do real work downstream:
//!
//! * `message_count` may be **zero**. That is a heartbeat, and it is how the
//!   session stays live when a symbol is quiet. It is not an error and not a
//!   gap; the very first segment in the fixture is one.
//! * `first_message_sequence` plus `message_count` gives the next expected
//!   sequence number, which is the entire basis of gap detection and of
//!   arbitrating two copies of the same feed.

use crate::Error;

/// Length of the fixed segment header.
pub const HEADER_LEN: usize = 40;

/// `message_protocol_id` for DEEP.
pub const PROTOCOL_DEEP: u16 = 0x8004;
/// `message_protocol_id` for TOPS.
pub const PROTOCOL_TOPS: u16 = 0x8003;

#[inline(always)]
fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
#[inline(always)]
fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
#[inline(always)]
fn le64(b: &[u8], o: usize) -> i64 {
    i64::from_le_bytes([
        b[o],
        b[o + 1],
        b[o + 2],
        b[o + 3],
        b[o + 4],
        b[o + 5],
        b[o + 6],
        b[o + 7],
    ])
}

/// A parsed IEX-TP segment, borrowing its message block.
#[derive(Debug, Clone, Copy)]
pub struct Segment<'a> {
    pub version: u8,
    pub protocol: u16,
    pub channel: u32,
    /// Changes when IEX restarts the feed; sequence numbers restart with it, so
    /// comparing sequences across a session change is meaningless.
    pub session: u32,
    pub message_count: u16,
    pub stream_offset: i64,
    pub first_sequence: i64,
    /// Feed send time, nanoseconds since the Unix epoch.
    pub send_time: i64,
    messages: &'a [u8],
}

impl<'a> Segment<'a> {
    /// Parse a segment from a UDP payload.
    pub fn parse(payload: &'a [u8]) -> Result<Self, Error> {
        if payload.len() < HEADER_LEN {
            return Err(Error::Truncated {
                need: HEADER_LEN,
                got: payload.len(),
            });
        }
        let payload_len = le16(payload, 12) as usize;
        let end = HEADER_LEN + payload_len;
        if end > payload.len() {
            return Err(Error::Truncated {
                need: end,
                got: payload.len(),
            });
        }
        Ok(Segment {
            version: payload[0],
            protocol: le16(payload, 2),
            channel: le32(payload, 4),
            session: le32(payload, 8),
            message_count: le16(payload, 14),
            stream_offset: le64(payload, 16),
            first_sequence: le64(payload, 24),
            send_time: le64(payload, 32),
            messages: &payload[HEADER_LEN..end],
        })
    }

    /// A segment carrying no messages, sent to keep the session alive.
    #[inline]
    pub fn is_heartbeat(&self) -> bool {
        self.message_count == 0
    }

    /// Sequence number the next segment on this channel should start at.
    #[inline]
    pub fn next_sequence(&self) -> i64 {
        self.first_sequence + self.message_count as i64
    }

    /// Iterate the length-prefixed message bodies.
    #[inline]
    pub fn messages(&self) -> Messages<'a> {
        Messages {
            buf: self.messages,
            pos: 0,
            remaining: self.message_count,
        }
    }
}

/// Iterator over `u16`-length-prefixed message bodies inside one segment.
pub struct Messages<'a> {
    buf: &'a [u8],
    pos: usize,
    remaining: u16,
}

impl<'a> Iterator for Messages<'a> {
    type Item = &'a [u8];

    #[inline]
    fn next(&mut self) -> Option<&'a [u8]> {
        if self.remaining == 0 || self.pos + 2 > self.buf.len() {
            return None;
        }
        let len = le16(self.buf, self.pos) as usize;
        let start = self.pos + 2;
        let end = start.checked_add(len)?;
        if end > self.buf.len() {
            return None;
        }
        self.pos = end;
        self.remaining -= 1;
        Some(&self.buf[start..end])
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, Some(self.remaining as usize))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(session: u32, first_seq: i64, msgs: &[&[u8]]) -> Vec<u8> {
        let mut body = Vec::new();
        for m in msgs {
            body.extend_from_slice(&(m.len() as u16).to_le_bytes());
            body.extend_from_slice(m);
        }
        let mut v = Vec::new();
        v.push(1); // version
        v.push(0); // reserved
        v.extend_from_slice(&PROTOCOL_DEEP.to_le_bytes());
        v.extend_from_slice(&1u32.to_le_bytes()); // channel
        v.extend_from_slice(&session.to_le_bytes());
        v.extend_from_slice(&(body.len() as u16).to_le_bytes());
        v.extend_from_slice(&(msgs.len() as u16).to_le_bytes());
        v.extend_from_slice(&0i64.to_le_bytes()); // stream offset
        v.extend_from_slice(&first_seq.to_le_bytes());
        v.extend_from_slice(&0i64.to_le_bytes()); // send time
        v.extend_from_slice(&body);
        v
    }

    #[test]
    fn parses_header_and_messages() {
        let raw = segment(1140588544, 42, &[b"abc", b"de"]);
        let s = Segment::parse(&raw).unwrap();
        assert_eq!(s.version, 1);
        assert_eq!(s.protocol, PROTOCOL_DEEP);
        assert_eq!(s.session, 1140588544);
        assert_eq!(s.first_sequence, 42);
        assert_eq!(s.message_count, 2);
        assert_eq!(s.next_sequence(), 44);
        let got: Vec<&[u8]> = s.messages().collect();
        assert_eq!(got, vec![b"abc".as_slice(), b"de".as_slice()]);
    }

    /// The first segment of the real fixture is a heartbeat. It must parse as a
    /// normal segment carrying nothing, not as an error and not as a gap.
    #[test]
    fn heartbeat_is_valid_and_advances_nothing() {
        let raw = segment(1, 1, &[]);
        let s = Segment::parse(&raw).unwrap();
        assert!(s.is_heartbeat());
        assert_eq!(s.next_sequence(), 1);
        assert_eq!(s.messages().count(), 0);
    }

    #[test]
    fn short_payloads_error_rather_than_panic() {
        let raw = segment(1, 1, &[b"abc"]);
        for n in 0..raw.len() {
            match Segment::parse(&raw[..n]) {
                Ok(s) => {
                    let _ = s.messages().count();
                }
                Err(Error::Truncated { .. }) => {}
                Err(e) => panic!("unexpected {e:?}"),
            }
        }
    }

    /// A count that overstates the body must not read past it.
    #[test]
    fn lying_message_count_is_bounded_by_the_buffer() {
        let mut raw = segment(1, 1, &[b"abc"]);
        raw[14..16].copy_from_slice(&9u16.to_le_bytes());
        assert_eq!(Segment::parse(&raw).unwrap().messages().count(), 1);
    }
}
