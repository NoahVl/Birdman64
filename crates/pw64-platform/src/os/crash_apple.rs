//! Darwin (macOS/iOS): no crash tracer yet, only what callers need.
//!
//! [`install`] is a no-op (the default Mach exception/signal behaviour and
//! the system crash reporter apply); [`exe_image_range`] walks this
//! executable's Mach-O load commands, which `memmap::image_range` needs.

use std::path::PathBuf;

pub fn set_crash_log_path(_path: PathBuf) {}

pub fn set_game_module(_range: (usize, usize), _map: PathBuf) {}

/// No symbolized backtrace on Darwin yet.
pub fn panic_frames(_skip: u32) -> String {
    String::new()
}

pub fn install() {}

const MH_MAGIC_64: u32 = 0xfeed_facf;
const LC_SEGMENT_64: u32 = 0x19;
const HEADER_SIZE: usize = 32;

/// Size in bytes from the start of the Mach-O image (its header, the start of
/// `__TEXT`) to the end of the last mapped segment that holds code or data:
/// `__PAGEZERO` (unmapped) and `__LINKEDIT` (symbols, signature) are left out.
/// `hdr` = the header plus all load commands. Pure.
pub fn image_extent(hdr: &[u8]) -> Option<usize> {
    let u32_at = |o: usize| Some(u32::from_le_bytes(hdr.get(o..o + 4)?.try_into().ok()?));
    let u64_at = |o: usize| Some(u64::from_le_bytes(hdr.get(o..o + 8)?.try_into().ok()?));
    if u32_at(0)? != MH_MAGIC_64 {
        return None;
    }
    let ncmds = u32_at(16)?;
    let (mut off, mut text, mut end) = (HEADER_SIZE, None, 0u64);
    for _ in 0..ncmds {
        let (cmd, size) = (u32_at(off)?, u32_at(off + 4)? as usize);
        if cmd == LC_SEGMENT_64 {
            let name = hdr.get(off + 8..off + 24)?;
            let name = &name[..name.iter().position(|&b| b == 0).unwrap_or(16)];
            let (vmaddr, vmsize) = (u64_at(off + 24)?, u64_at(off + 32)?);
            if name == b"__TEXT" {
                text = Some(vmaddr);
            }
            if !matches!(name, b"__PAGEZERO" | b"__LINKEDIT") {
                end = end.max(vmaddr.checked_add(vmsize)?);
            }
        }
        off = off.checked_add(size.max(8))?;
    }
    usize::try_from(end.checked_sub(text?)?).ok()
}

/// `(start, end)` of this executable's loaded image (dyld may slide it).
pub fn exe_image_range() -> (usize, usize) {
    unsafe extern "C" {
        /// Defined by the linker: the Mach-O header of the main executable.
        static _mh_execute_header: u8;
    }
    let base = (&raw const _mh_execute_header) as usize;
    // SAFETY: the mapped header of the running executable: ncmds/sizeofcmds
    // (header +16/+20) describe the load commands that follow it.
    let hdr = unsafe {
        let p = base as *const u8;
        let sizeofcmds = (p.add(20) as *const u32).read_unaligned() as usize;
        std::slice::from_raw_parts(p, HEADER_SIZE + sizeofcmds)
    };
    let extent = image_extent(hdr).expect("executable Mach-O header");
    (base, base + extent)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(name: &str, vmaddr: u64, vmsize: u64) -> Vec<u8> {
        let mut c = Vec::new();
        c.extend(LC_SEGMENT_64.to_le_bytes());
        c.extend(72u32.to_le_bytes());
        let mut n = [0u8; 16];
        n[..name.len()].copy_from_slice(name.as_bytes());
        c.extend(n);
        c.extend(vmaddr.to_le_bytes());
        c.extend(vmsize.to_le_bytes());
        c.resize(72, 0);
        c
    }

    #[test]
    fn extent_skips_pagezero_and_linkedit() {
        let segs = [
            seg("__PAGEZERO", 0, 1 << 32),
            seg("__TEXT", 1 << 32, 0x4000),
            seg("__DATA", (1 << 32) + 0x4000, 0x8000),
            seg("__LINKEDIT", (1 << 32) + 0xc000, 0x100000),
        ];
        let mut h = Vec::new();
        h.extend(MH_MAGIC_64.to_le_bytes());
        h.extend([0u8; 12]);
        h.extend((segs.len() as u32).to_le_bytes());
        h.extend((72u32 * segs.len() as u32).to_le_bytes());
        h.extend([0u8; 8]);
        for s in &segs {
            h.extend(s);
        }
        assert_eq!(image_extent(&h), Some(0xc000));
        assert_eq!(image_extent(&h[..40]), None);
    }

    /// This test binary's own range contains one of its functions.
    #[test]
    fn range_contains_this_code() {
        let (a, b) = exe_image_range();
        let here = range_contains_this_code as fn() as usize;
        assert!((a..b).contains(&here));
    }
}
