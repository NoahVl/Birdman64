//! CRT replacements for the first-run build (docs/notes/first-run-build.md,
//! task T1): the game module links no CRT and imports nothing, so its C's
//! `memcpy memset strlen ldiv powf sqrtf` are jump thunks (`C(name)` in
//! dylib/pw64_dll_imports.h) bound by `pw64_dll_bind` to these. Before T1
//! they came from ucrtbase.dll / the exe's libc; now everything is the exe's
//! CRT (the same `powf` on every machine; Linux: no `--as-needed` risk of
//! libm being dropped from the exe). Unused (dead) in the static build.

use core::ffi::{CStr, c_char, c_int, c_void};

/// Mirrors `memcpy`.
///
/// # Safety
/// C's `memcpy` contract: `n` bytes readable at `src`, writable at `dest`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pw64_crt_memcpy(
    dest: *mut c_void,
    src: *const c_void,
    n: usize,
) -> *mut c_void {
    // C allows NULL with n == 0; `ptr::copy` requires non-null.
    if n != 0 {
        // `copy` (memmove), not `copy_nonoverlapping`: an overlapping memcpy
        // is UB in C but works with the MSVC/glibc CRTs the static build
        // uses; here it would be Rust UB. Same result either way when the
        // decomp's calls don't overlap.
        // SAFETY: C's contract: `n` bytes readable at `src`, writable at `dest`.
        unsafe { core::ptr::copy(src.cast::<u8>(), dest.cast::<u8>(), n) };
    }
    dest
}

/// Mirrors `memset`.
///
/// # Safety
/// C's `memset` contract: `n` bytes writable at `dest`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pw64_crt_memset(dest: *mut c_void, byte: c_int, n: usize) -> *mut c_void {
    if n != 0 {
        // SAFETY: C's contract: `n` bytes writable at `dest`.
        unsafe { core::ptr::write_bytes(dest.cast::<u8>(), byte as u8, n) };
    }
    dest
}

/// Mirrors `strlen`.
///
/// # Safety
/// C's `strlen` contract: `s` points at a NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pw64_crt_strlen(s: *const c_char) -> usize {
    // SAFETY: C's contract: NUL-terminated string.
    unsafe { CStr::from_ptr(s) }.count_bytes()
}

/// `ldiv_t`: Windows LLP64 `long` is 32-bit (the Linux shadow stdlib.h
/// inlines ldiv, so the slot is bound but never called there).
#[repr(C)]
pub struct Ldiv {
    pub quot: i32,
    pub rem: i32,
}

/// Mirrors `ldiv` (truncated division, `rem` gets `n`'s sign).
#[unsafe(no_mangle)]
pub extern "C" fn pw64_crt_ldiv(n: i32, d: i32) -> Ldiv {
    Ldiv {
        quot: n / d,
        rem: n % d,
    }
}

/// Mirrors `powf`: the exe's `f32::powf`, not the player's ucrtbase.
#[unsafe(no_mangle)]
pub extern "C" fn pw64_crt_powf(x: f32, y: f32) -> f32 {
    f32::powf(x, y)
}

/// Mirrors `sqrtf`.
#[unsafe(no_mangle)]
pub extern "C" fn pw64_crt_sqrtf(x: f32) -> f32 {
    f32::sqrt(x)
}
