//! The allocator's policy for Kimo's memory.
//!
//! glibc keeps what a burst freed. It raises its `mmap` threshold to the size
//! of the largest block `free` was handed (up to 32 MB), and from then on
//! serves blocks of that size from the heap and keeps them. A GUI that renders
//! in bursts — a font scan that reads a name table out of every installed face,
//! a rasterised page of a few megabytes, a 19 MB CJK collection read once —
//! walks the threshold up to tens of megabytes and then never gives the arena
//! back: measured on this app, `[heap]` sat at 46.5 MB while the whole mail
//! cache was 4.6 MB (see `docs/fix.md`).
//!
//! `init` pins the threshold at glibc's 128 KiB default, which is also what
//! turns the dynamic adjustment off, so a transient larger than that is a
//! private mapping returned the moment it is dropped. Measured on the machine
//! in `docs/fix.md`: idle RSS 253 MB → 208 MB — the same as starting the app
//! with `MALLOC_MMAP_THRESHOLD_=131072` — and `[heap]` 46.5 MB → 17.6 MB.
//!
//! There is deliberately no `malloc_trim` call anywhere. With the threshold
//! pinned, four of them at the points where a burst ends idled within two
//! megabytes of a build without them, and `malloc_trim` walks the arenas under
//! the allocator's lock — a stutter in a frame for nothing.

/// Set the allocator's policy once, before anything large is allocated.
pub(crate) fn init() {
    #[cfg(target_env = "gnu")]
    // SAFETY: `mallopt` is glibc's, and `M_MMAP_THRESHOLD` takes a byte size.
    // Called from `main` before the first allocation that could matter; the
    // return value only says whether the parameter was accepted.
    unsafe {
        mallopt(M_MMAP_THRESHOLD, 128 * 1024);
    }
}

#[cfg(target_env = "gnu")]
extern "C" {
    fn mallopt(param: i32, value: i32) -> i32;
}

/// glibc's `M_MMAP_THRESHOLD`, the parameter `init` sets (`malloc.h`).
#[cfg(target_env = "gnu")]
const M_MMAP_THRESHOLD: i32 = -3;
