//! A lock-free single-producer, single-consumer ring buffer.
//!
//! Built for the threaded pipeline comparison, where the question is whether
//! splitting a ~110 ns path across two cores beats running it on one. That
//! question is only meaningful with a *good* queue: a mutex-guarded `VecDeque`
//! would lose so heavily that it would prove nothing except that mutexes are
//! slow.
//!
//! ## The two things that make it fast
//!
//! **Cache-line padding.** `head` and `tail` are written by different threads.
//! On the same cache line, every producer write invalidates the consumer's copy
//! and vice versa — false sharing, and it can cost more than the work being
//! distributed. They are padded to separate lines.
//!
//! **Cached opposite indices.** A naive implementation reads the other side's
//! atomic on every operation, so every push touches a line the consumer owns.
//! Each side instead caches its last-seen view of the other and only re-reads
//! when the cache says the queue is full (producer) or empty (consumer). In
//! steady state the hot path touches only lines that side already owns.
//!
//! ## Memory ordering, and why ARM is the useful place to write this
//!
//! The producer writes the slot, then publishes `tail` with `Release`. The
//! consumer reads `tail` with `Acquire`, which guarantees the slot write is
//! visible before the index that advertises it.
//!
//! On x86 this is nearly free and — more to the point — **using `Relaxed` here
//! would usually still work**, because x86's TSO already forbids the store-store
//! reordering that would break it. Code with missing barriers passes on x86 and
//! fails on ARM. This crate is developed on an M2, where the weak memory model
//! actually permits the reordering, so an ordering bug has somewhere to show up
//! rather than lying dormant until it reaches different silicon.
//!
//! Exhaustive interleaving validation under `loom` is B7 and not done here; what
//! this file claims is a correct-by-construction Release/Acquire pairing and
//! tests that pass on a weakly-ordered machine, which is a weaker claim than a
//! model checker and is stated as such.

use std::cell::{Cell, UnsafeCell};
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Padding to a typical cache line so two fields cannot share one.
#[repr(align(128))]
struct Padded<T>(T);

struct Inner<T> {
    /// Capacity is a power of two so wrapping is a mask, not a modulo.
    buf: Box<[UnsafeCell<MaybeUninit<T>>]>,
    mask: usize,
    /// Next slot to write. Producer-owned.
    tail: Padded<AtomicUsize>,
    /// Next slot to read. Consumer-owned.
    head: Padded<AtomicUsize>,
}

// SAFETY: the split into `Producer` and `Consumer` means exactly one thread
// writes `tail` and touches slots in [head, tail), and exactly one reads `head`
// and touches slots in [head, tail). Neither ever accesses the other's range.
unsafe impl<T: Send> Send for Inner<T> {}
unsafe impl<T: Send> Sync for Inner<T> {}

/// The writing half. Not `Clone` — a second producer would break the invariant.
pub struct Producer<T> {
    inner: Arc<Inner<T>>,
    /// Last seen `head`, to avoid touching the consumer's cache line.
    cached_head: Cell<usize>,
}

/// The reading half.
pub struct Consumer<T> {
    inner: Arc<Inner<T>>,
    cached_tail: Cell<usize>,
}

/// Create a queue holding up to `capacity - 1` items.
///
/// One slot is always left empty so a full queue is distinguishable from an
/// empty one without a separate counter, which would be a third contended field.
#[must_use]
pub fn channel<T>(capacity: usize) -> (Producer<T>, Consumer<T>) {
    let cap = capacity.next_power_of_two();
    let mut buf = Vec::with_capacity(cap);
    for _ in 0..cap {
        buf.push(UnsafeCell::new(MaybeUninit::uninit()));
    }
    let inner = Arc::new(Inner {
        buf: buf.into_boxed_slice(),
        mask: cap - 1,
        tail: Padded(AtomicUsize::new(0)),
        head: Padded(AtomicUsize::new(0)),
    });
    (
        Producer {
            inner: Arc::clone(&inner),
            cached_head: Cell::new(0),
        },
        Consumer {
            inner,
            cached_tail: Cell::new(0),
        },
    )
}

impl<T> Producer<T> {
    /// Enqueue, or hand the value back if the queue is full.
    #[inline]
    pub fn push(&self, value: T) -> Result<(), T> {
        let tail = self.inner.tail.0.load(Ordering::Relaxed);
        let next = tail.wrapping_add(1);

        // Only consult the consumer's index when our cached view says full.
        // In steady state this branch is not taken and the consumer's cache
        // line is never touched.
        if next.wrapping_sub(self.cached_head.get()) > self.inner.mask {
            self.cached_head
                .set(self.inner.head.0.load(Ordering::Acquire));
            if next.wrapping_sub(self.cached_head.get()) > self.inner.mask {
                return Err(value);
            }
        }

        // SAFETY: slot `tail & mask` is outside [head, tail), so the consumer
        // will not read it until `tail` is published below.
        unsafe {
            (*self.inner.buf[tail & self.inner.mask].get()).write(value);
        }
        // Release: the slot write above must be visible to any thread that
        // observes this new tail. Relaxed here would still pass on x86 and is
        // genuinely broken on ARM.
        self.inner.tail.0.store(next, Ordering::Release);
        Ok(())
    }
}

impl<T> Consumer<T> {
    /// Dequeue, or `None` if empty.
    #[inline]
    pub fn pop(&self) -> Option<T> {
        let head = self.inner.head.0.load(Ordering::Relaxed);

        if head == self.cached_tail.get() {
            self.cached_tail
                .set(self.inner.tail.0.load(Ordering::Acquire));
            if head == self.cached_tail.get() {
                return None;
            }
        }

        // SAFETY: `head < tail` was established above, so this slot was written
        // and published by the producer and has not been read yet.
        let value = unsafe { (*self.inner.buf[head & self.inner.mask].get()).assume_init_read() };
        self.inner
            .head
            .0
            .store(head.wrapping_add(1), Ordering::Release);
        Some(value)
    }

    /// Items currently queued.
    #[inline]
    pub fn len(&self) -> usize {
        let tail = self.inner.tail.0.load(Ordering::Acquire);
        let head = self.inner.head.0.load(Ordering::Relaxed);
        tail.wrapping_sub(head)
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<T> Drop for Inner<T> {
    fn drop(&mut self) {
        // Items still queued were written but never read, so their destructors
        // have not run. Leaking them would be a real leak for any `T` that owns
        // memory, even though the pipeline only ever queues `Copy` types.
        let head = self.head.0.load(Ordering::Relaxed);
        let tail = self.tail.0.load(Ordering::Relaxed);
        let mut i = head;
        while i != tail {
            // SAFETY: [head, tail) is exactly the initialised range.
            unsafe {
                (*self.buf[i & self.mask].get()).assume_init_drop();
            }
            i = i.wrapping_add(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    #[test]
    fn round_trips_in_order() {
        let (p, c) = channel::<u64>(8);
        for i in 0..7 {
            p.push(i).unwrap();
        }
        for i in 0..7 {
            assert_eq!(c.pop(), Some(i));
        }
        assert_eq!(c.pop(), None);
    }

    /// One slot is deliberately sacrificed to tell full from empty.
    #[test]
    fn capacity_is_one_less_than_the_buffer() {
        let (p, c) = channel::<u32>(4);
        for i in 0..3 {
            p.push(i).unwrap();
        }
        assert_eq!(p.push(99), Err(99), "the fourth slot stays empty");
        assert_eq!(c.pop(), Some(0));
        p.push(99).unwrap();
    }

    #[test]
    fn wraps_around_many_times() {
        let (p, c) = channel::<u64>(4);
        for i in 0..10_000u64 {
            p.push(i).unwrap();
            assert_eq!(c.pop(), Some(i));
        }
        assert!(c.is_empty());
    }

    /// The real test: two threads, every item delivered exactly once and in
    /// order, on a weakly-ordered machine where a missing barrier can actually
    /// reorder rather than being hidden by x86's TSO.
    #[test]
    fn concurrent_transfer_preserves_every_item_and_its_order() {
        const N: u64 = 500_000;
        let (p, c) = channel::<u64>(1024);
        let sum = Arc::new(AtomicU64::new(0));
        let sink = Arc::clone(&sum);

        let consumer = std::thread::spawn(move || {
            let mut seen = 0u64;
            let mut expect = 0u64;
            while seen < N {
                if let Some(v) = c.pop() {
                    assert_eq!(v, expect, "items must arrive in order");
                    expect += 1;
                    seen += 1;
                    sink.fetch_add(v, Ordering::Relaxed);
                } else {
                    std::hint::spin_loop();
                }
            }
        });

        for i in 0..N {
            while p.push(i).is_err() {
                std::hint::spin_loop();
            }
        }
        consumer.join().unwrap();
        assert_eq!(sum.load(Ordering::Relaxed), (0..N).sum::<u64>());
    }

    /// A queued item's destructor must still run when the queue is dropped.
    #[test]
    fn dropping_a_non_empty_queue_drops_its_items() {
        use std::sync::atomic::AtomicUsize;
        static DROPS: AtomicUsize = AtomicUsize::new(0);
        #[derive(Debug)]
        struct Counted(#[allow(dead_code)] u32);
        impl Drop for Counted {
            fn drop(&mut self) {
                DROPS.fetch_add(1, Ordering::Relaxed);
            }
        }

        DROPS.store(0, Ordering::Relaxed);
        {
            let (p, c) = channel::<Counted>(8);
            for i in 0..5 {
                p.push(Counted(i)).unwrap();
            }
            drop(c.pop()); // one drained and dropped here
        }
        assert_eq!(
            DROPS.load(Ordering::Relaxed),
            5,
            "the four still queued must be dropped with the queue"
        );
    }
}
