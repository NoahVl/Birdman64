//! Native platform layer for the decomp C: replaces libultra and every other
//! symbol the C code imports.
//!
//! - [`os`]: threads (coroutines), message queues, events, timers, VI timing.
//! - [`pi`]: cartridge ROM access and `mio0_decompress`.
//! - [`ai`]: audio interface over an emulated DAC clock, samples → host sink.
//! - [`headless`]: SP/SI backends: gfx/audio tasks → host hooks,
//!   controllers from host input (`set_controller` / `set_controller1`,
//!   presence count via `set_controllers_present`), `.eep`-file EEPROM.
//! - [`stubs`]: remaining data placeholders.
//! - [`crt`]: CRT replacements bound into the dylib game module (first-run
//!   build, docs/notes/first-run-build.md).
//! - [`swap`]: BE→host swaps for ROM structs copied raw (`PW64_SWAP`).
//! - [`vaddr`]: 32-bit C addresses ↔ host addresses (`PW64_U32`/`PW64_PTR`).

// The `unsafe extern` fns are libultra entry points called from C: their
// safety contract is libultra's (valid pointers of the documented types).
#![allow(clippy::missing_safety_doc)]
#![warn(clippy::undocumented_unsafe_blocks)]

pub mod ai;
pub mod crt;
pub mod headless;
pub mod os;
pub mod pi;
pub mod start;
pub mod stubs;
pub mod swap;
pub mod vaddr;

/// `MAP_FIXED_NOREPLACE` (Linux 4.17+; the same value on every arch). The
/// libc crate leaves it out for Android, whose older kernels take it as a
/// plain hint: every caller checks the returned address anyway.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub const MAP_FIXED_NOREPLACE: core::ffi::c_int = 0x10_0000;

/// Darwin has no `MAP_FIXED_NOREPLACE`, and its `MAP_FIXED` silently
/// replaces existing mappings, so a fresh reservation is a plain hint (0
/// flag) that every caller checks (`address == hint`, else munmap and fail).
/// Re-mapping a slot of our own reservation still uses `MAP_FIXED`.
#[cfg(target_vendor = "apple")]
pub const MAP_FIXED_NOREPLACE: core::ffi::c_int = 0;

/// Darwin: maps `len` bytes of fresh anonymous memory at an address the
/// kernel picks, 64 KB aligned (the stack window base must be), above 4 GB
/// (always true on arm64: the low 4 GB is `__PAGEZERO`; checked anyway).
/// Fallback for when the fixed high candidates don't fit the process's
/// address space (iOS without the extended-VA entitlement has only a few
/// GB above 4 GB). The vaddr layer accepts any host base above 4 GB, so the
/// block needs no particular address. Over-reserves 64 KB and trims.
#[cfg(target_vendor = "apple")]
pub fn mmap_anywhere(len: usize, prot: core::ffi::c_int) -> Option<usize> {
    const ALIGN: usize = 0x1_0000;
    // SAFETY: fresh anonymous mapping; the kernel picks the address, failure
    // is checked, and only parts of that mapping are unmapped again.
    unsafe {
        let p = libc::mmap(
            core::ptr::null_mut(),
            len + ALIGN,
            prot,
            libc::MAP_PRIVATE | libc::MAP_ANON,
            -1,
            0,
        );
        if p == libc::MAP_FAILED {
            return None;
        }
        let start = p as usize;
        let at = start.next_multiple_of(ALIGN);
        if at > start {
            libc::munmap(p, at - start);
        }
        let tail = start + len + ALIGN - (at + len);
        if tail > 0 {
            libc::munmap((at + len) as *mut libc::c_void, tail);
        }
        if at >> 32 == 0 {
            libc::munmap(at as *mut libc::c_void, len);
            return None;
        }
        Some(at)
    }
}

/// Logs the first call of a placeholder implementation (once per name).
pub(crate) fn first_call(name: &'static str) {
    use std::collections::HashSet;
    use std::sync::Mutex;
    static SEEN: Mutex<Option<HashSet<&'static str>>> = Mutex::new(None);
    let mut seen = SEEN.lock().unwrap();
    if seen.get_or_insert_with(HashSet::new).insert(name) {
        eprintln!("[platform] first call: {name} (headless placeholder)");
    }
}
