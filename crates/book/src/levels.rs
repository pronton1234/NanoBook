//! The one interface the bake-off swaps out.
//!
//! DEEP is an aggregated-depth feed, so a side of the book is a map from price
//! to displayed size — not a graph of orders. That makes the container choice
//! unusually stark: there is exactly one data structure here, it handles 97% of
//! all feed traffic, and every implementation of this trait is a different
//! answer to the same question.
//!
//! Three operations matter, in this order of frequency:
//!
//! 1. **`set`** — write a size at a price. Every price level update.
//! 2. **`max`/`min`** — find the touch. Every update, if a consumer wants a
//!    top-of-book after each one.
//! 3. everything else — depth walks, validation, diagnostics. Rare.
//!
//! `set` with a size of zero is a *deletion*, and deletion is where the
//! implementations genuinely diverge: removing the best level means the next
//! best has to be found, which is free for an ordered map and is the entire
//! problem for a flat array.
//!
//! Dispatch is static — the trait is a generic bound, never a `dyn` — because a
//! vtable indirection on the hot path would be measuring the vtable.

use deep::Price;

/// A single side of one symbol's book: price to displayed size.
pub trait Levels: Default {
    /// Set the displayed size at `price`. A `size` of zero removes the level.
    fn set(&mut self, price: Price, size: u32);

    /// Highest price holding size. The best bid.
    fn max(&self) -> Option<(Price, u32)>;

    /// Lowest price holding size. The best ask.
    fn min(&self) -> Option<(Price, u32)>;

    /// Number of populated levels.
    fn len(&self) -> usize;

    #[inline]
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Total displayed size across every level. Used by the audit, not the hot
    /// path, and deliberately recomputed rather than cached so it can catch a
    /// cached total that has drifted.
    fn total_size(&self) -> u64;

    /// Visit every populated level. Order is unspecified.
    fn for_each(&self, f: impl FnMut(Price, u32));

    /// Drop every level. Used on a session reset, where sequence numbers restart
    /// and any retained state is from a stream that no longer exists.
    fn clear(&mut self);
}
