//! Gap detection, duplicate suppression, and A/B feed arbitration.
//!
//! Exchanges publish market data twice, down physically separate paths, and a
//! receiver is expected to consume both and take whichever copy of each segment
//! arrives first. The two feeds carry *identical* sequence numbers, which leads
//! to the observation this module is built on:
//!
//! **A/B arbitration is not a separate algorithm. It is deduplication by
//! sequence number over the merged stream.**
//!
//! Feed the A and B datagrams into one [`Sequencer`] as they arrive and the
//! winner is whichever reached the socket first; the loser is rejected by the
//! same code path that rejects a retransmit. There is no A/B-specific branch and
//! no clock comparison, so there is no way for the two paths to disagree.
//!
//! Three states have to be told apart, and conflating any two is a real bug:
//!
//! * `got == expected` — deliver.
//! * `got < expected` — already seen. The B-feed copy, or a retransmit. Drop it.
//! * `got > expected` — a gap. Segments are held, in order, until the missing
//!   ones arrive; if they do not, the stream is unrecoverable without a
//!   retransmission request to the recovery service.
//!
//! UDP reorders, so a gap is *not* immediately a loss. Declaring loss on first
//! sight of an out-of-order segment would fire constantly on a healthy feed —
//! which is why held segments are buffered rather than discarded.

use std::collections::BTreeMap;

/// What the receiver should do with a segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// In order. Process its messages now.
    Deliver,
    /// Seen before: the other side of an A/B pair, or a retransmit.
    Duplicate,
    /// Ahead of the expected sequence. Held pending the missing segments.
    Buffered { expected: i64, got: i64 },
    /// The session id changed, so sequence numbers restarted.
    SessionReset,
    /// Carries no messages; does not advance the sequence.
    Heartbeat,
}

/// Counters worth reporting at the end of a run.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub delivered: u64,
    pub duplicates: u64,
    pub buffered: u64,
    pub gaps_opened: u64,
    pub gaps_healed: u64,
    pub messages_lost: u64,
    pub session_resets: u64,
    pub heartbeats: u64,
}

/// Per-channel sequencing state.
pub struct Sequencer {
    session: Option<u32>,
    expected: i64,
    /// Segments ahead of `expected`, keyed by their first sequence number.
    held: BTreeMap<i64, (i64, u16)>,
    max_held: usize,
    pub stats: Stats,
}

impl Default for Sequencer {
    fn default() -> Self {
        Self::new(1024)
    }
}

impl Sequencer {
    /// `max_held` bounds the reorder buffer. Without a bound, a permanently lost
    /// segment would make the buffer grow for the rest of the session.
    pub fn new(max_held: usize) -> Self {
        Self {
            session: None,
            expected: 0,
            held: BTreeMap::new(),
            max_held,
            stats: Stats::default(),
        }
    }

    /// Sequence number currently awaited.
    #[inline]
    pub fn expected(&self) -> i64 {
        self.expected
    }

    /// Offer a segment, identified by its session, first sequence and count.
    pub fn observe(&mut self, session: u32, first_sequence: i64, message_count: u16) -> Action {
        // A heartbeat proves liveness and nothing else. Treating its sequence as
        // authoritative would let a quiet channel silently reset `expected`.
        if message_count == 0 {
            self.stats.heartbeats += 1;
            if self.session.is_none() {
                self.session = Some(session);
                self.expected = first_sequence;
            }
            return Action::Heartbeat;
        }

        match self.session {
            None => {
                // Join mid-stream: whatever arrives first defines the origin.
                self.session = Some(session);
                self.expected = first_sequence;
            }
            Some(s) if s != session => {
                self.session = Some(session);
                self.expected = first_sequence;
                self.held.clear();
                self.stats.session_resets += 1;
                self.stats.delivered += 1;
                self.expected = first_sequence + message_count as i64;
                return Action::SessionReset;
            }
            Some(_) => {}
        }

        if first_sequence < self.expected {
            self.stats.duplicates += 1;
            return Action::Duplicate;
        }
        if first_sequence > self.expected {
            let expected = self.expected;
            if self.held.is_empty() {
                self.stats.gaps_opened += 1;
            }
            if self.held.len() < self.max_held {
                self.held
                    .insert(first_sequence, (first_sequence, message_count));
                self.stats.buffered += 1;
            }
            return Action::Buffered {
                expected,
                got: first_sequence,
            };
        }

        self.expected = first_sequence + message_count as i64;
        self.stats.delivered += 1;
        self.drain_held();
        Action::Deliver
    }

    /// Release any buffered segments that are now contiguous.
    fn drain_held(&mut self) {
        while let Some((&seq, &(_, count))) = self.held.iter().next() {
            if seq > self.expected {
                break;
            }
            self.held.remove(&seq);
            // A held segment can also be a duplicate once the gap closes.
            if seq == self.expected {
                self.expected = seq + count as i64;
                self.stats.delivered += 1;
            } else {
                self.stats.duplicates += 1;
            }
            if self.held.is_empty() {
                self.stats.gaps_healed += 1;
            }
        }
    }

    /// Abandon an unfillable gap and resume from the earliest held segment.
    ///
    /// On a live feed this is what follows an unanswered retransmission request.
    /// The messages skipped are counted rather than hidden, because a receiver
    /// that quietly resynchronises is indistinguishable from one that never
    /// dropped anything.
    pub fn force_resync(&mut self) -> Option<i64> {
        let (&seq, &(_, count)) = self.held.iter().next()?;
        self.stats.messages_lost += (seq - self.expected).max(0) as u64;
        self.held.remove(&seq);
        self.expected = seq + count as i64;
        self.stats.delivered += 1;
        self.drain_held();
        Some(seq)
    }

    /// Number of segments currently held pending a gap.
    #[inline]
    pub fn held_len(&self) -> usize {
        self.held.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u32 = 1140588544;

    #[test]
    fn in_order_segments_deliver() {
        let mut q = Sequencer::default();
        assert_eq!(q.observe(S, 1, 3), Action::Deliver);
        assert_eq!(q.observe(S, 4, 2), Action::Deliver);
        assert_eq!(q.expected(), 6);
        assert_eq!(q.stats.delivered, 2);
    }

    /// The property the whole module exists for: replaying an identical stream
    /// through the same sequencer — which is exactly what the B feed is —
    /// delivers every segment exactly once.
    #[test]
    fn ab_arbitration_delivers_each_segment_once() {
        let segments = [(1i64, 3u16), (4, 2), (6, 1), (7, 4)];

        // Every way two feeds can be skewed against each other. All must give
        // the same answer, because arbitration depends only on sequence numbers
        // -- never on arrival order, and never on which feed a copy came from.
        let interleaved: Vec<_> = segments.iter().flat_map(|&s| [s, s]).collect();
        let a_fully_ahead: Vec<_> = segments.iter().chain(segments.iter()).copied().collect();
        let mut b_lags_one = vec![segments[0]];
        for (i, &s) in segments.iter().enumerate().skip(1) {
            b_lags_one.push(s);
            b_lags_one.push(segments[i - 1]);
        }
        b_lags_one.push(segments[segments.len() - 1]);

        for (name, order) in [
            ("interleaved", interleaved),
            ("A fully ahead of B", a_fully_ahead),
            ("B lagging one segment", b_lags_one),
        ] {
            let mut q = Sequencer::default();
            for (seq, n) in order {
                q.observe(S, seq, n);
            }
            assert_eq!(q.stats.delivered, 4, "{name}: each segment delivered once");
            assert_eq!(q.stats.duplicates, 4, "{name}: the losing copy is dropped");
            assert_eq!(q.expected(), 11, "{name}");
            assert_eq!(q.held_len(), 0, "{name}: nothing left stranded");
        }
    }

    /// UDP reorders. An out-of-order segment is held and released when the gap
    /// closes — declaring loss on first sight would fire on a healthy feed.
    #[test]
    fn reordered_segments_are_held_then_released() {
        let mut q = Sequencer::default();
        assert_eq!(q.observe(S, 1, 2), Action::Deliver);
        assert!(matches!(
            q.observe(S, 5, 1),
            Action::Buffered {
                expected: 3,
                got: 5
            }
        ));
        assert_eq!(q.held_len(), 1);
        assert_eq!(q.observe(S, 3, 2), Action::Deliver);
        assert_eq!(q.held_len(), 0, "the held segment drained once contiguous");
        assert_eq!(q.expected(), 6);
        assert_eq!(q.stats.gaps_opened, 1);
    }

    #[test]
    fn heartbeats_do_not_advance_the_sequence() {
        let mut q = Sequencer::default();
        q.observe(S, 1, 2);
        assert_eq!(q.observe(S, 99, 0), Action::Heartbeat);
        assert_eq!(q.expected(), 3, "a heartbeat must not move the stream");
    }

    #[test]
    fn session_change_restarts_sequencing() {
        let mut q = Sequencer::default();
        q.observe(S, 100, 2);
        assert_eq!(q.observe(S + 1, 1, 1), Action::SessionReset);
        assert_eq!(q.expected(), 2);
        assert_eq!(q.stats.session_resets, 1);
    }

    /// An unfillable gap must be counted, not silently skipped.
    #[test]
    fn force_resync_reports_what_was_lost() {
        let mut q = Sequencer::default();
        q.observe(S, 1, 1);
        q.observe(S, 10, 1);
        assert_eq!(q.force_resync(), Some(10));
        assert_eq!(q.stats.messages_lost, 8, "sequences 2..=9 were never seen");
        assert_eq!(q.expected(), 11);
    }

    #[test]
    fn reorder_buffer_is_bounded() {
        let mut q = Sequencer::new(4);
        q.observe(S, 1, 1);
        for seq in (10..100).step_by(2) {
            q.observe(S, seq, 1);
        }
        assert_eq!(q.held_len(), 4, "buffer must not grow without bound");
    }
}
