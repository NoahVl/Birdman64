//! Coroutine stacks at fixed addresses in 0x80800000..0xA0000000.
//!
//! Game threads pass pointers to locals into C that keeps addresses in 32-bit
//! ints, so stacks must be below 4 GB. They must also have **bit 31 set**:
//! `_uvMediaCopy`/`uvMemRead` (memory.c:146,195) treat any address without it
//! as a ROM offset, and `uvMemRead` of a ROM value reads it back from a stack
//! temporary (`temp2`); with a stack below 2 GB that recursed until the stack
//! overflowed. So stacks go right above the 8 MB RDRAM window (KSEG0-like
//! addresses, and `(u32)p & 0x80000000` sees RAM).
//! The C's own stack arrays (`gKernelThreadStack`, 8-16 KB) are too small for
//! host code (Rust frames, debug builds, panics), so they are ignored and
//! every thread gets one of these instead.
//!
//! Windows: [`reserve_region`] (called by `pw64_game::memmap::init`, before
//! the window and the GPU driver exist) reserves the whole search window, so
//! a driver allocation can't land in it later; stacks are then committed out
//! of that reservation. Slot layout, bottom up (native-build.md §8):
//! no-access page | [`HEADROOM`] read/write | `PAGE_GUARD` page | stack.
//! A stack overflow hits the guard page, which the kernel reports as
//! STATUS_STACK_OVERFLOW (TEB `DeallocationStack` = the guard page, so there
//! is no "room to grow"); the crash handler then runs on the headroom.

use corosensei::stack::{Stack, StackPointer};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

/// Usable size of every game-thread stack (fully committed).
pub const STACK_SIZE: usize = 1 << 20;
const PAGE: usize = 0x1000;
/// Committed space below the guard page for the exception dispatch of a
/// stack overflow (CONTEXT + record, then the vectored handlers). Sized so
/// a slot is a multiple of the 64 KB allocation granularity (a fallback
/// MEM_RESERVE at an unaligned address is rounded down).
pub const HEADROOM: usize = 0x1_0000 - 2 * PAGE;
/// One slot: no-access page + headroom + guard page + stack (1 MB + 64 KB).
const SLOT_SIZE: usize = PAGE + HEADROOM + PAGE + STACK_SIZE;
const _: () = assert!(SLOT_SIZE.is_multiple_of(0x1_0000));
/// Search window for stacks: just above the RDRAM window (0x80000000 +
/// 8 MB), up to where KSEG1-style literals (0xA4xxxxxx I/O) would start
/// looking like hardware registers.
pub const SEARCH_START: usize = 0x8080_0000;
pub const SEARCH_END: usize = 0xA000_0000;

/// Next address to try; shared by all host threads (tests run in parallel).
static CURSOR: AtomicUsize = AtomicUsize::new(SEARCH_START);
/// [`reserve_region`] succeeded: slots are committed out of the reservation.
static RESERVED: AtomicBool = AtomicBool::new(false);

/// Number of slots that fit the search window (slots start at
/// `SEARCH_START + k * SLOT_SIZE`: [`CURSOR`] only steps by `SLOT_SIZE`).
const SLOTS: usize = (SEARCH_END - SEARCH_START) / SLOT_SIZE;
/// One bit per slot: set while a [`LowStack`] owns it (mapped + committed).
/// Lets the RSP HLE check a pointer without a lock or allocation.
static LIVE: [AtomicU64; SLOTS.div_ceil(64)] = [const { AtomicU64::new(0) }; SLOTS.div_ceil(64)];

/// Slot index whose usable stack range (`[bottom, top)`, not the no-access,
/// headroom or guard pages) contains all of `[addr, addr + len)`. Pure.
pub fn stack_slot_of(addr: usize, len: usize) -> Option<usize> {
    let end = addr.checked_add(len)?;
    if addr < SEARCH_START || end > SEARCH_START + SLOTS * SLOT_SIZE {
        return None;
    }
    let slot = (addr - SEARCH_START) / SLOT_SIZE;
    let base = SEARCH_START + slot * SLOT_SIZE;
    let bottom = base + PAGE + HEADROOM + PAGE;
    (addr >= bottom && end <= base + SLOT_SIZE).then_some(slot)
}

/// True if `[addr, addr + len)` lies in the usable range of one live thread
/// stack. For the RSP HLE (display / audio command lists may point at C
/// locals); O(1), one atomic load.
pub fn in_live_stack(addr: usize, len: usize) -> bool {
    stack_slot_of(addr, len)
        .is_some_and(|s| LIVE[s / 64].load(Ordering::Acquire) & (1 << (s % 64)) != 0)
}

fn set_live(slot_addr: usize, live: bool) {
    let s = (slot_addr - SEARCH_START) / SLOT_SIZE;
    let bit = 1u64 << (s % 64);
    if live {
        LIVE[s / 64].fetch_or(bit, Ordering::AcqRel);
    } else {
        LIVE[s / 64].fetch_and(!bit, Ordering::AcqRel);
    }
}

#[cfg(windows)]
mod sys {
    use core::ffi::c_void;
    unsafe extern "system" {
        pub fn VirtualAlloc(addr: *mut c_void, size: usize, ty: u32, prot: u32) -> *mut c_void;
        pub fn VirtualFree(addr: *mut c_void, size: usize, ty: u32) -> i32;
        pub fn VirtualProtect(addr: *mut c_void, size: usize, prot: u32, old: *mut u32) -> i32;
    }
    pub const MEM_COMMIT: u32 = 0x1000;
    pub const MEM_RESERVE: u32 = 0x2000;
    pub const MEM_DECOMMIT: u32 = 0x4000;
    pub const MEM_RELEASE: u32 = 0x8000;
    pub const PAGE_NOACCESS: u32 = 0x01;
    pub const PAGE_READWRITE: u32 = 0x04;
    pub const PAGE_GUARD: u32 = 0x100;
}

// macOS/arm64 can't map anything below 4 GB (`__PAGEZERO`): native-build.md §10.
#[cfg(not(any(windows, target_os = "linux")))]
compile_error!(
    "pw64-platform: the OS core needs thread stacks at fixed addresses in \
     0x80800000..0xA0000000 (below 4 GB, bit 31 set); implemented for Windows (VirtualAlloc) \
     and Linux (mmap MAP_FIXED_NOREPLACE) only — see docs/notes/native-build.md §10"
);

/// Windows: reserves (MEM_RESERVE, no commit) the whole stack window, so
/// nothing else (GPU driver, overlay) can take it before the game threads
/// exist. Call once, early. False (and stacks fall back to per-slot
/// reserve+commit) if part of the window is already taken. Linux: no-op
/// returning false: mmap places unhinted mappings top-down from high
/// addresses, so nothing lands in this low window on its own.
pub fn reserve_region() -> bool {
    #[cfg(windows)]
    {
        if RESERVED.load(Ordering::Acquire) {
            return true;
        }
        // SAFETY: plain Win32 reservation at a fixed address; failure is checked.
        let p = unsafe {
            sys::VirtualAlloc(
                SEARCH_START as *mut _,
                SEARCH_END - SEARCH_START,
                sys::MEM_RESERVE,
                sys::PAGE_NOACCESS,
            )
        };
        if p as usize == SEARCH_START {
            RESERVED.store(true, Ordering::Release);
            return true;
        }
        false
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// A fully committed stack at a fixed address in the range above (slot
/// layout in the module docs). Fully committing it (instead of Windows'
/// guard page growth scheme) keeps the TEB bookkeeping trivial.
pub struct LowStack {
    /// Slot start (the no-access page).
    slot: usize,
    /// Committed out of the [`reserve_region`] reservation: drop decommits.
    in_reservation: bool,
}

impl LowStack {
    pub fn new() -> Self {
        loop {
            let slot = CURSOR.fetch_add(SLOT_SIZE, Ordering::Relaxed);
            assert!(
                slot + SLOT_SIZE <= SEARCH_END,
                "no free address range in 0x80800000..0xA0000000 for a thread stack"
            );
            let in_reservation = RESERVED.load(Ordering::Acquire);
            // A slot already in use just moves on to the next one.
            if map_fixed(slot, in_reservation) {
                set_live(slot, true);
                return Self {
                    slot,
                    in_reservation,
                };
            }
        }
    }

    /// The guard page: lowest page of the stack range, one-shot PAGE_GUARD.
    fn guard(&self) -> usize {
        self.slot + PAGE + HEADROOM
    }

    /// Lowest usable stack address.
    fn bottom(&self) -> usize {
        self.guard() + PAGE
    }

    fn top(&self) -> usize {
        self.slot + SLOT_SIZE
    }

    /// Diagnostics: the usable address range `[bottom, top)`.
    pub fn range(&self) -> (usize, usize) {
        (self.bottom(), self.top())
    }
}

impl Default for LowStack {
    fn default() -> Self {
        Self::new()
    }
}

/// Commits `SLOT_SIZE` read/write bytes at exactly `slot` (out of the
/// reservation, or reserving it here) and sets up the bottom no-access page
/// and the guard page above the headroom. False if the range is taken.
#[cfg(windows)]
fn map_fixed(slot: usize, in_reservation: bool) -> bool {
    use core::ffi::c_void;
    let ty = if in_reservation {
        sys::MEM_COMMIT
    } else {
        sys::MEM_RESERVE | sys::MEM_COMMIT
    };
    // SAFETY: plain Win32 allocation at a fixed address; failure is checked.
    // Inside the reservation the slot is ours (CURSOR hands each out once).
    let p = unsafe { sys::VirtualAlloc(slot as *mut c_void, SLOT_SIZE, ty, sys::PAGE_READWRITE) };
    if p.is_null() {
        assert!(
            !in_reservation,
            "committing a thread stack failed: {}",
            std::io::Error::last_os_error()
        );
        return false;
    }
    assert_eq!(p as usize, slot);
    let mut old = 0;
    // Setting PAGE_GUARD makes Windows move the *calling* thread's TEB
    // StackLimit to just above the new guard page (measured: whichever
    // coroutine ran osCreateThread then had StackLimit > StackBase, and the
    // next `__chkstk` probed down from there into the neighbour slot's
    // guard page). Put the caller's StackLimit back afterwards.
    let saved_limit = teb_stack_limit();
    // SAFETY: pages of the allocation we own.
    unsafe {
        let ok = sys::VirtualProtect(p, PAGE, sys::PAGE_NOACCESS, &mut old);
        assert_ne!(ok, 0, "VirtualProtect on the stack no-access page failed");
        let guard = (slot + PAGE + HEADROOM) as *mut c_void;
        let ok = sys::VirtualProtect(guard, PAGE, sys::PAGE_READWRITE | sys::PAGE_GUARD, &mut old);
        assert_ne!(ok, 0, "VirtualProtect on the stack guard page failed");
    }
    set_teb_stack_limit(saved_limit);
    true
}

/// TEB `StackLimit` (gs:[0x10]) of the calling thread.
#[cfg(windows)]
fn teb_stack_limit() -> usize {
    let v: usize;
    // SAFETY: reads the current thread's TEB (x64: gs points at it).
    unsafe { core::arch::asm!("mov {}, gs:[0x10]", out(reg) v, options(nostack, readonly)) };
    v
}

#[cfg(windows)]
fn set_teb_stack_limit(v: usize) {
    // SAFETY: restores the value read by `teb_stack_limit` on this thread.
    unsafe { core::arch::asm!("mov gs:[0x10], {}", in(reg) v, options(nostack)) };
}

#[cfg(target_os = "linux")]
fn map_fixed(slot: usize, _in_reservation: bool) -> bool {
    // MAP_FIXED_NOREPLACE (Linux 4.17+) fails with EEXIST instead of
    // clobbering an existing mapping; older kernels treat it as a hint, so
    // the address is checked too. MAP_NORESERVE: commit lazily like any
    // Linux stack (no TEB-style bookkeeping to keep consistent here).
    // SAFETY: anonymous mapping at a free fixed address; failure is checked.
    let p = unsafe {
        libc::mmap(
            slot as *mut libc::c_void,
            SLOT_SIZE,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE
                | libc::MAP_ANONYMOUS
                | libc::MAP_FIXED_NOREPLACE
                | libc::MAP_NORESERVE,
            -1,
            0,
        )
    };
    if p == libc::MAP_FAILED {
        return false;
    }
    if p as usize != slot {
        // SAFETY: the mapping the kernel just gave us elsewhere.
        unsafe { libc::munmap(p, SLOT_SIZE) };
        return false;
    }
    // Everything below the stack is PROT_NONE: the SIGSEGV handler runs on
    // the thread's sigaltstack, so no headroom is needed here.
    // SAFETY: the first pages of the mapping we own.
    let ok = unsafe { libc::mprotect(p, PAGE + HEADROOM + PAGE, libc::PROT_NONE) };
    assert_eq!(ok, 0, "mprotect on the stack guard pages failed");
    true
}

impl Drop for LowStack {
    fn drop(&mut self) {
        // Before unmapping: the HLE must not accept pointers into it.
        set_live(self.slot, false);
        // SAFETY: releases (or, inside the reservation, decommits so the
        // window stays reserved) the whole allocation made in `new`.
        #[cfg(windows)]
        unsafe {
            if self.in_reservation {
                sys::VirtualFree(self.slot as *mut _, SLOT_SIZE, sys::MEM_DECOMMIT);
            } else {
                sys::VirtualFree(self.slot as *mut _, 0, sys::MEM_RELEASE);
            }
        }
        // SAFETY: as above.
        #[cfg(target_os = "linux")]
        unsafe {
            let _ = self.in_reservation;
            libc::munmap(self.slot as *mut libc::c_void, SLOT_SIZE);
        }
    }
}

// SAFETY: base/limit describe memory we own for the stack's lifetime, aligned
// to the page size (> STACK_ALIGNMENT).
unsafe impl Stack for LowStack {
    fn base(&self) -> StackPointer {
        StackPointer::new(self.top()).unwrap()
    }

    fn limit(&self) -> StackPointer {
        StackPointer::new(self.bottom()).unwrap()
    }

    #[cfg(windows)]
    fn teb_fields(&self) -> corosensei::stack::StackTebFields {
        // DeallocationStack = the guard page: a guard hit then has no room
        // to "grow" into, so the kernel raises STATUS_STACK_OVERFLOW at
        // once instead of walking the guard down through the headroom.
        corosensei::stack::StackTebFields {
            StackBase: self.top(),
            StackLimit: self.bottom(),
            DeallocationStack: self.guard(),
            GuaranteedStackBytes: 0,
        }
    }

    #[cfg(windows)]
    fn update_teb_fields(&mut self, _stack_limit: usize, _guaranteed_stack_bytes: usize) {
        // Fully committed: nothing grows, nothing to record.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stack range is page aligned, STACK_SIZE long, and leaves the
    /// guard page + headroom + no-access page below it inside the slot.
    #[test]
    fn slot_layout() {
        let s = LowStack::new();
        let (lo, hi) = s.range();
        assert_eq!(hi - lo, STACK_SIZE);
        assert_eq!(lo % PAGE, 0);
        assert_eq!(lo - s.slot, PAGE + HEADROOM + PAGE);
        assert!((SEARCH_START..=SEARCH_END).contains(&hi));
        // The top of the stack is writable.
        // SAFETY: committed read/write memory we own.
        unsafe { ((hi - 8) as *mut u64).write_volatile(1) };
        assert!(in_live_stack(lo, STACK_SIZE));
        assert!(!in_live_stack(lo - 1, 1), "guard page is not data");
        let slot = s.slot;
        drop(s);
        assert!(!in_live_stack(slot + SLOT_SIZE - 8, 8), "freed slot");
    }

    /// Pure slot arithmetic: only the usable stack part of one slot.
    #[test]
    fn stack_slot_ranges() {
        let bottom = |k: usize| SEARCH_START + k * SLOT_SIZE + 2 * PAGE + HEADROOM;
        let top = |k: usize| SEARCH_START + (k + 1) * SLOT_SIZE;
        assert_eq!(stack_slot_of(bottom(0), STACK_SIZE), Some(0));
        assert_eq!(stack_slot_of(bottom(3) + 16, 4), Some(3));
        assert_eq!(stack_slot_of(top(3) - 4, 4), Some(3));
        // Straddles into the next slot's no-access page.
        assert_eq!(stack_slot_of(top(3) - 4, 8), None);
        // No-access page, headroom, guard page.
        assert_eq!(stack_slot_of(SEARCH_START, 4), None);
        assert_eq!(stack_slot_of(bottom(0) - PAGE, 4), None);
        assert_eq!(stack_slot_of(bottom(0) - 2, 4), None);
        // Outside the window, past the last whole slot, overflow.
        assert_eq!(stack_slot_of(SEARCH_START - 4, 4), None);
        assert_eq!(stack_slot_of(SEARCH_END - 4, 4), None);
        assert_eq!(stack_slot_of(usize::MAX - 2, 4), None);
    }

    /// Creating a stack (PAGE_GUARD) must not move the caller's TEB
    /// StackLimit (it made `__chkstk` probe into a neighbour's guard page).
    #[cfg(windows)]
    #[test]
    fn new_stack_keeps_the_callers_stack_limit() {
        let before = teb_stack_limit();
        let _s = LowStack::new();
        assert_eq!(teb_stack_limit(), before);
    }
}
