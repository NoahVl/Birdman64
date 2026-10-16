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
