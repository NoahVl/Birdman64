//! `osInitialize`, interrupt mask, cache ops, address translation and the
//! low-RAM libultra globals.

#![allow(non_upper_case_globals)]

use super::{trace, with};
use std::ffi::c_void;

pub const OS_IM_NONE: u32 = 0x0000_0001;
pub const OS_IM_ALL: u32 = 0x003F_FF01;

/// `OS_TV_NTSC`: the US game picks `OS_VI_NTSC_LAN1` from it (system.c).
#[unsafe(no_mangle)]
pub static mut osTvType: i32 = 1;

/// RDRAM size: 8 MB, matching the window `pw64-game::memmap` maps (Expansion
/// Pak). `_uvDMA` and the texture heap bound use it.
#[unsafe(no_mangle)]
pub static mut osMemSize: u32 = 0x80_0000;

/// Mirrors `osInitialize`: nothing to set up natively beyond the clock
/// (already running) and the globals above.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn osInitialize() {
    trace!("osInitialize");
}

/// Mirrors `osSetIntMask`. There are no asynchronous interrupts; the mask
/// only gates delivery at OS-call checkpoints.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn osSetIntMask(mask: u32) -> u32 {
    with(|k| std::mem::replace(&mut k.int_mask, mask))
}

/// Cache maintenance: no-ops (coherent host memory).
#[unsafe(no_mangle)]
pub extern "C-unwind" fn osInvalDCache(_p: *mut c_void, _n: i32) {}
#[unsafe(no_mangle)]
pub extern "C-unwind" fn osWritebackDCache(_p: *mut c_void, _n: i32) {}
#[unsafe(no_mangle)]
pub extern "C-unwind" fn osWritebackDCacheAll() {}
#[unsafe(no_mangle)]
pub extern "C-unwind" fn osInvalICache(_p: *mut c_void, _n: i32) {}

/// Mirrors `osVirtualToPhysical` for KSEG0: `addr - 0x80000000` mod 2^32.
/// Host addresses outside the window (exe image, stacks) wrap too, and the
/// renderer maps back with `+0x80000000` (native-build.md §2, option C).
#[unsafe(no_mangle)]
pub extern "C-unwind" fn osVirtualToPhysical(p: *mut c_void) -> u32 {
    (p as usize as u32).wrapping_sub(0x8000_0000)
}
