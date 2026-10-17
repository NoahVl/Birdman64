//! Strict-mode audit heap (`PW64_RELOC=strict`, docs/notes/option-a.md S6).
//!
//! Strict mode panics when a host pointer outside every vaddr region reaches
//! `PW64_U32` — but only if that pointer lies above 4 GB. On Windows the
//! process heap sits below 0x80000000, where a Rust heap pointer looks like a
//! ROM offset and passes. With [`HighHeap`] as the global allocator,
//! [`enable`] (called by `memmap::init` in strict mode) moves every later
//! Rust allocation into one arena reserved above 4 GB, so a heap pointer
//! handed to the C and converted or truncated is caught.
//!
//! Off (every other mode, and until [`enable`]): plain forwarding to
//! [`System`]. On: power-of-two size classes, a bump pointer, one free list
//! per class, one lock; memory is never returned to the OS. Built for
//! audits, not speed; blocks allocated before [`enable`] stay in the system
//! heap and are freed there.

use core::alloc::{GlobalAlloc, Layout};
use std::alloc::System;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

/// Address space reserved for the arena (committed on demand).
const ARENA_SIZE: usize = 64 << 30;
/// Commit granularity.
const COMMIT_STEP: usize = 4 << 20;
/// Largest alignment served from the arena (bigger: [`System`]).
const MAX_ALIGN: usize = 0x1_0000;

/// Arena start; 0 = off.
static BASE: AtomicUsize = AtomicUsize::new(0);

struct State {
    /// Bytes handed out from the arena start (incl. alignment padding).
    bump: usize,
    /// Bytes committed from the arena start.
    committed: usize,
    /// Free-list head per size class `1 << k` (0 = empty); each free block
    /// holds the next block's address in its first word.
    free: [usize; usize::BITS as usize],
}

static STATE: Mutex<State> = Mutex::new(State {
    bump: 0,
    committed: 0,
    free: [0; usize::BITS as usize],
});

/// Global allocator: [`System`] until [`enable`], then the high arena.
pub struct HighHeap;

/// Size class (log2 of the block size) for `l`. Pure.
fn class(l: Layout) -> usize {
    l.size()
        .max(l.align())
        .max(16)
        .next_power_of_two()
        .trailing_zeros() as usize
}

/// `p` was allocated from the arena (which is on).
fn in_arena(p: *mut u8) -> bool {
    let base = BASE.load(Ordering::Acquire);
    base != 0 && (p as usize).wrapping_sub(base) < ARENA_SIZE
}

#[cfg(windows)]
mod sys {
    use core::ffi::c_void;
    unsafe extern "system" {
        pub fn VirtualAlloc(addr: *mut c_void, size: usize, ty: u32, prot: u32) -> *mut c_void;
    }
    pub const MEM_COMMIT: u32 = 0x1000;
    pub const MEM_RESERVE: u32 = 0x2000;
    pub const PAGE_NOACCESS: u32 = 0x01;
    pub const PAGE_READWRITE: u32 = 0x04;
}

/// Reserves `ARENA_SIZE` at exactly `base`, no access. True on success.
fn reserve(base: usize) -> bool {
    #[cfg(windows)]
    {
        // SAFETY: plain reservation at a fixed address; checked.
        let p = unsafe {
            sys::VirtualAlloc(
                base as *mut _,
                ARENA_SIZE,
                sys::MEM_RESERVE,
                sys::PAGE_NOACCESS,
            )
        };
        p as usize == base
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    // SAFETY: anonymous PROT_NONE mapping that never clobbers (old kernels
    // take NOREPLACE as a hint, hence the check).
    unsafe {
        let p = libc::mmap(
            base as *mut libc::c_void,
            ARENA_SIZE,
            libc::PROT_NONE,
            libc::MAP_PRIVATE
                | libc::MAP_ANONYMOUS
                | libc::MAP_NORESERVE
                | pw64_platform::MAP_FIXED_NOREPLACE,
            -1,
            0,
        );
        if p != libc::MAP_FAILED && p as usize != base {
            libc::munmap(p, ARENA_SIZE);
        }
        p as usize == base
    }
}

/// Makes `[addr, addr + len)` of the reservation read/write.
fn commit(addr: usize, len: usize) -> bool {
    #[cfg(windows)]
    {
        // SAFETY: pages inside our own reservation.
        let p =
            unsafe { sys::VirtualAlloc(addr as *mut _, len, sys::MEM_COMMIT, sys::PAGE_READWRITE) };
        !p.is_null()
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    // SAFETY: pages inside our own reservation.
    unsafe {
        libc::mprotect(
            addr as *mut libc::c_void,
            len,
            libc::PROT_READ | libc::PROT_WRITE,
        ) == 0
    }
}

/// Turns the arena on (once; later calls return the same base). Every
/// allocation from now on lands above 4 GB. Returns the arena start, or
/// `None` if no candidate range was free (the heap then stays as it is).
/// Only effective in a binary that installs [`HighHeap`] as
/// `#[global_allocator]`.
pub fn enable() -> Option<usize> {
    let _guard = STATE.lock().unwrap_or_else(PoisonError::into_inner);
    let cur = BASE.load(Ordering::Acquire);
    if cur != 0 {
        return Some(cur);
    }
    // 1 TB, 2 TB, ...: far above everything else we map. Then 128..384 GB
    // for 39-bit VA kernels (common aarch64 Linux: user space ends at
    // 512 GB), still above the RDRAM/stack candidates (< 96 GB).
    let base = (1..16)
        .map(|k| k << 40)
        .chain((2..6).map(|k| k << 36))
        .find(|&a| reserve(a))?;
    BASE.store(base, Ordering::Release);
    Some(base)
}

/// Returns an arena block of layout `l` to its free list. Cold, like
/// [`arena_alloc`]: with the arena off the global allocator stays a plain
/// forward to [`System`] (option-a.md "Performance").
///
/// # Safety
/// `p` came from [`arena_alloc`] with a layout of the same class as `l`.
#[cold]
#[inline(never)]
unsafe fn arena_free(p: *mut u8, l: Layout) {
    let mut s = STATE.lock().unwrap_or_else(PoisonError::into_inner);
    let k = class(l);
    // SAFETY: our block (≥ 16 bytes, aligned): link it into its list.
    unsafe { (p as *mut usize).write(s.free[k]) };
    s.free[k] = p as usize;
}

/// Allocates from the arena (`base` = its start); null if it is exhausted.
#[cold]
#[inline(never)]
fn arena_alloc(base: usize, l: Layout) -> *mut u8 {
    let k = class(l);
    let mut s = STATE.lock().unwrap_or_else(PoisonError::into_inner);
    let head = s.free[k];
    if head != 0 {
        // SAFETY: a freed block of this class; its first word links the list.
        s.free[k] = unsafe { (head as *const usize).read() };
        return head as *mut u8;
    }
    let size = 1usize << k;
    let start = s.bump.next_multiple_of(size.min(MAX_ALIGN));
    let end = start + size;
    if end > ARENA_SIZE {
        return core::ptr::null_mut();
    }
    if end > s.committed {
        let to = end.next_multiple_of(COMMIT_STEP).min(ARENA_SIZE);
        if !commit(base + s.committed, to - s.committed) {
            return core::ptr::null_mut();
        }
        s.committed = to;
    }
    s.bump = end;
    (base + start) as *mut u8
}

// SAFETY: blocks are disjoint (bump or a free list hands each out once),
// at least `size` bytes, aligned to `align` (block size = a power of two ≥
// both, start aligned to min(block size, MAX_ALIGN) ≥ align); everything
// else is the system allocator's.
unsafe impl GlobalAlloc for HighHeap {
    #[inline]
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let base = BASE.load(Ordering::Acquire);
        if base == 0 || l.align() > MAX_ALIGN {
            // SAFETY: forwarded with the caller's layout.
            return unsafe { System.alloc(l) };
        }
        arena_alloc(base, l)
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        if BASE.load(Ordering::Acquire) == 0 {
            // SAFETY: forwarded with the caller's layout.
            return unsafe { System.alloc_zeroed(l) };
        }
        // SAFETY: the caller's layout; a non-null block holds `l.size()` bytes.
        unsafe {
            let p = self.alloc(l);
            if !p.is_null() {
                p.write_bytes(0, l.size());
            }
            p
        }
    }

    #[inline]
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        if in_arena(p) {
            // SAFETY: an arena block of layout `l`.
            unsafe { arena_free(p, l) }
        } else {
            // SAFETY: a system block (allocated before `enable`, or over-aligned).
            unsafe { System.dealloc(p, l) }
        }
    }

    #[inline]
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new_size: usize) -> *mut u8 {
        if BASE.load(Ordering::Acquire) == 0 {
            // SAFETY: forwarded; off means every block is a system block.
            return unsafe { System.realloc(p, l, new_size) };
        }
        // SAFETY: the caller's contract, forwarded.
        unsafe { self.realloc_on(p, l, new_size) }
    }
}

impl HighHeap {
    /// [`GlobalAlloc::realloc`] with the arena on (cold: strict mode only).
    #[cold]
    #[inline(never)]
    unsafe fn realloc_on(&self, p: *mut u8, l: Layout, new_size: usize) -> *mut u8 {
        let Ok(new) = Layout::from_size_align(new_size, l.align()) else {
            return core::ptr::null_mut();
        };
        if in_arena(p) && class(new) == class(l) {
            return p;
        }
        // Move (a system block allocated before `enable` moves high too).
        // SAFETY: caller contract; both blocks hold min(old, new) bytes.
        unsafe {
            let q = self.alloc(new);
            if !q.is_null() {
                core::ptr::copy_nonoverlapping(p, q, l.size().min(new_size));
                self.dealloc(p, l);
            }
            q
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_classes() {
        let c = |s, a| class(Layout::from_size_align(s, a).unwrap());
        assert_eq!(c(1, 1), 4);
        assert_eq!(c(16, 8), 4);
        assert_eq!(c(17, 8), 5);
        assert_eq!(c(8, 64), 6);
        assert_eq!(c(1 << 20, 16), 20);
    }
}
