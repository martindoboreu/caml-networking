//! Test-only global allocator that counts heap allocations made by the
//! current thread while a measurement is running.
//!
//! ```
//! use caml_alloc_count::{CountingAllocator, count_allocations};
//!
//! #[global_allocator]
//! static ALLOC: CountingAllocator = CountingAllocator;
//!
//! fn main() {
//!     let (sum, allocations) = count_allocations(|| (1..=10u64).sum::<u64>());
//!     assert_eq!((sum, allocations), (55, 0));
//!
//!     let (v, allocations) = count_allocations(|| vec![0u8; 16]);
//!     assert_eq!((v.len(), allocations), (16, 1));
//! }
//! ```
//!
//! Counting is per thread, so tests running in parallel on other threads
//! cannot disturb a measurement.

#![warn(missing_docs)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static COUNT: Cell<u64> = const { Cell::new(0) };
}

/// Forwards to [`System`], counting allocations (including reallocations)
/// made by a thread inside [`count_allocations`].
#[derive(Debug, Clone, Copy, Default)]
pub struct CountingAllocator;

#[inline]
fn note_allocation() {
    // `try_with`: thread-locals may already be gone during thread teardown,
    // and an allocator must never panic. Both are const-initialized `Cell`s,
    // so accessing them never allocates (no recursion).
    if ARMED.try_with(Cell::get).unwrap_or(false) {
        let _ = COUNT.try_with(|c| c.set(c.get().saturating_add(1)));
    }
}

// SAFETY: every method forwards its arguments unchanged to `System`, which
// is a correct `GlobalAlloc`, so all of `GlobalAlloc`'s layout and pointer
// guarantees are inherited. The added bookkeeping only touches
// const-initialized thread-local `Cell`s and neither allocates nor unwinds.
#[allow(unsafe_code)]
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note_allocation();
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract
        // (non-zero-size layout); it is passed through unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note_allocation();
        // SAFETY: as for `alloc`.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note_allocation();
        // SAFETY: the caller guarantees `ptr` was allocated by this allocator
        // (hence by `System`) with `layout`, and that `new_size` is valid.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller guarantees `ptr` was allocated by this allocator
        // (hence by `System`) with `layout`.
        unsafe { System.dealloc(ptr, layout) }
    }
}

/// Disarms counting even if the measured closure panics.
struct Disarm;

impl Drop for Disarm {
    fn drop(&mut self) {
        ARMED.with(|a| a.set(false));
    }
}

/// Runs `f` and returns its result together with the number of heap
/// allocations (and reallocations) the current thread made during it.
///
/// Only meaningful when [`CountingAllocator`] is the `#[global_allocator]`
/// of the running binary; otherwise the count is always 0.
pub fn count_allocations<R>(f: impl FnOnce() -> R) -> (R, u64) {
    COUNT.with(|c| c.set(0));
    ARMED.with(|a| a.set(true));
    let guard = Disarm;
    let result = f();
    drop(guard);
    (result, COUNT.with(Cell::get))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[global_allocator]
    static ALLOC: CountingAllocator = CountingAllocator;

    #[test]
    fn counts_allocations_inside_the_closure_only() {
        let (v, n) = count_allocations(|| vec![1u8; 64]);
        assert_eq!(v.len(), 64);
        assert_eq!(n, 1);

        let (mut v, n) = count_allocations(|| Vec::<u8>::with_capacity(1));
        assert_eq!(n, 1);
        let ((), n) = count_allocations(|| v.extend_from_slice(&[0; 100]));
        assert_eq!(n, 1, "a reallocation counts");

        let ((), n) = count_allocations(|| {
            std::hint::black_box([0u64; 16]);
        });
        assert_eq!(n, 0);

        // Not armed outside a measurement.
        let _outside = std::hint::black_box(Box::new(0u8));
        let ((), n) = count_allocations(|| ());
        assert_eq!(n, 0);
    }
}
