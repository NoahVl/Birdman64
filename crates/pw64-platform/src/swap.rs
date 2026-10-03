//! Big-endian → host byte swapping for ROM structs that the C copies raw
//! (native-build.md §3, task 6). Called from the patch set right after the
//! raw copy (`_uvMediaCopy`/`uvConsumeBytes` with a struct size), through the
//! macros in `pw64_native.h`:
//!
//! ```c
//! PW64_SWAP(vtxTable, vtxCount, Vtx, "6h4b");   // count elements of sizeof(Vtx)
//! ```
//!
//! The layout string describes one element as a sequence of `[N]c` items:
//! `b` = 1 byte kept, `h` = 2-byte swap, `w` = 4-byte swap, `d` = 8-byte swap.
//! Its total must equal the element size (`sizeof(T)` from the C side), else
//! this panics: a cheap check that the layout matches the host struct.
//! Structs with pointer fields have a different ROM layout and can't be
//! swapped in place: those get field-wise loaders instead (see the notes).

use std::ffi::{CStr, c_char, c_void};

/// Parses `layout` into (item size, repeat) pairs; panics on a bad string.
fn parse(layout: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut n: Option<usize> = None;
    for c in layout.chars() {
        if let Some(d) = c.to_digit(10) {
            n = Some(n.unwrap_or(0) * 10 + d as usize);
            continue;
        }
        let size = match c {
            'b' => 1,
            'h' => 2,
            'w' => 4,
            'd' => 8,
            ' ' => continue,
            _ => panic!("pw64_swap: bad layout char {c:?} in {layout:?}"),
        };
        out.push((size, n.take().unwrap_or(1)));
    }
    assert!(n.is_none(), "pw64_swap: trailing count in {layout:?}");
    out
}

/// Swaps `count` elements of `stride` bytes at `data` per `layout`.
pub fn swap_slice(data: &mut [u8], stride: usize, layout: &str) {
    let items = parse(layout);
    let total: usize = items.iter().map(|(s, n)| s * n).sum();
    assert_eq!(
        total, stride,
        "pw64_swap: layout {layout:?} covers {total} bytes, element is {stride}"
    );
    assert_eq!(data.len() % stride, 0);
    for elem in data.chunks_exact_mut(stride) {
        let mut off = 0;
        for &(size, n) in &items {
            for _ in 0..n {
                elem[off..off + size].reverse();
                off += size;
            }
        }
    }
}

/// C entry point behind `PW64_SWAP`: in-place BE→LE of `count` elements.
///
/// # Safety
/// `p` must be writable for `count * stride` bytes; `layout` a
/// NUL-terminated descriptor string.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn pw64_swap(
    p: *mut c_void,
    count: u32,
    stride: u32,
    layout: *const c_char,
) {
    // SAFETY: the C passes a NUL-terminated literal.
    let layout = unsafe { CStr::from_ptr(layout) }.to_str().unwrap();
    if count == 0 {
        // Still validate the layout against the element size.
        swap_slice(&mut [], stride as usize, layout);
        return;
    }
    assert!(!p.is_null(), "pw64_swap: null pointer ({layout})");
    let len = count as usize * stride as usize;
    // SAFETY: the caller owns `count * stride` bytes at `p`.
    let data = unsafe { std::slice::from_raw_parts_mut(p.cast::<u8>(), len) };
    if crate::os::trace_enabled() {
        eprintln!("[swap] {layout} stride {stride} count {count}");
    }
    swap_slice(data, stride as usize, layout);
}

/// `pw64_rsp()`: the caller's approximate RSP (at entry, the value is a few
/// words into the call). Diagnostics use it to pin which coroutine stack slot
/// a patched C frame is running on.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn pw64_rsp() -> u32 {
    #[cfg(target_arch = "x86_64")]
    // SAFETY: reads the architectural stack pointer.
    unsafe {
        let rsp: u64;
        core::arch::asm!("mov {rsp}, rsp", rsp = out(reg) rsp, options(nomem));
        rsp as u32
    }
    #[cfg(not(target_arch = "x86_64"))]
    0
}

/// `pw64_fatal(msg, value)`: a clean panic for the native C helpers (the
/// game's own `_uvDebugPrintf` is compiled out).
///
/// # Safety
/// `msg` must point at a NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn pw64_fatal(msg: *const c_char, value: u32) -> ! {
    // SAFETY: the C passes a NUL-terminated literal.
    let msg = unsafe { CStr::from_ptr(msg) }.to_string_lossy();
    panic!("{msg} (0x{value:x})");
}

/// `pw64_log(msg, value)`: a one-line trace from patched C code, replacing the
/// RCP register pokes and `_uvDebugPrintf` calls that are compiled out.
/// Prints only under `PW64_TRACE_OS`.
///
/// # Safety
/// `msg` must point at a NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn pw64_log(msg: *const c_char, value: u32) {
    if !crate::os::trace_enabled() {
        return;
    }
    // SAFETY: the C passes a NUL-terminated literal.
    let msg = unsafe { CStr::from_ptr(msg) }.to_string_lossy();
    eprintln!("[c] {msg} (0x{value:x})");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vtx_layout() {
        // Vtx: s16 ob[3], u16 flag, s16 tc[2], u8 cn[4].
        let mut v = [
            0x00, 0x01, 0xFF, 0xFE, 0x12, 0x34, 0, 0, 0x40, 0, 0, 0x20, 1, 2, 3, 4,
        ];
        swap_slice(&mut v, 16, "6h4b");
        assert_eq!(
            v,
            [
                0x01, 0x00, 0xFE, 0xFF, 0x34, 0x12, 0, 0, 0, 0x40, 0x20, 0, 1, 2, 3, 4
            ]
        );
    }

    #[test]
    fn words_and_counts() {
        let mut v = [1, 2, 3, 4, 5, 6, 7, 8];
        swap_slice(&mut v, 4, "w");
        assert_eq!(v, [4, 3, 2, 1, 8, 7, 6, 5]);
        let mut v = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
        swap_slice(&mut v, 10, "2b 2h 1w");
        assert_eq!(v, [1, 2, 4, 3, 6, 5, 10, 9, 8, 7]);
    }

    #[test]
    #[should_panic(expected = "covers")]
    fn size_mismatch_panics() {
        swap_slice(&mut [0; 8], 8, "3h");
    }
}
