//! 32-bit N64 addresses ("vaddr") ↔ host addresses (docs/notes/option-a.md).
//!
//! The C keeps addresses in 32-bit slots (display-list words, the allocator's
//! bookkeeping, the filesystem, literals like `0x803DA800`). Every conversion
//! between such a slot and a real pointer goes through `PW64_U32` /
//! `PW64_PTR` (pw64_native.h), which call [`pw64_to_vaddr`] /
//! [`pw64_to_host`]. Each registered [`Region`] maps a vaddr range (bit 31
//! set) to a host block that may lie anywhere. Values outside every region
//! pass through unchanged: ROM offsets and physical (`K0 - 0x80000000`)
//! addresses, and, until regions are registered, everything (the fixed
//! low-4 GB window build, where vaddr == host address).

use core::ffi::c_void;
use std::sync::OnceLock;

/// One C-visible host block and the vaddr range the C sees it at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region {
    /// First vaddr (bit 31 set: the C reads bit 31 clear as a ROM offset).
    pub vbase: u32,
    /// Length in bytes.
    pub size: u32,
    /// Host address of `vbase`.
    pub host: usize,
}

static REGIONS: OnceLock<Vec<Region>> = OnceLock::new();

/// Registers the C-visible regions, once, before the C runs. Panics on a
/// second call, a vaddr range below 0x80000000 or past 4 GB, or a host block
/// below 4 GB (a host address that fits 32 bits could not be told apart from
/// a vaddr). Without this call both conversions are the identity.
pub fn set_regions(regions: Vec<Region>) {
    for r in &regions {
        assert!(
            r.vbase & 0x8000_0000 != 0 && r.vbase.checked_add(r.size).is_some(),
            "vaddr region {r:?}: must lie in 0x80000000..4 GB"
        );
        assert!(r.host >> 32 != 0, "vaddr region {r:?}: host block must lie above 4 GB");
    }
    REGIONS
        .set(regions)
        .expect("vaddr regions are registered once, before the C runs");
}

/// The registered regions (empty: identity mapping).
pub fn regions() -> &'static [Region] {
    REGIONS.get().map_or(&[], Vec::as_slice)
}

/// Host address → vaddr through `regions`. Pure. A host address in no region
/// keeps its low 32 bits: a vaddr / ROM offset / physical value that was
/// cast to a pointer (possibly sign-extended from an `s32`) round-trips.
pub fn to_vaddr_in(regions: &[Region], host: usize) -> u32 {
    for r in regions {
        let off = host.wrapping_sub(r.host);
        if off < r.size as usize {
            return r.vbase + off as u32;
        }
    }
    if !regions.is_empty() {
        // Relocated build: anything above 4 GB that isn't a sign-extended
        // 32-bit value is a host pointer the C must not store in 32 bits.
        let hi = (host as u64) >> 32;
        assert!(
            hi == 0 || hi == 0xFFFF_FFFF,
            "host address {host:#x} is in no C-visible region: the C would truncate it"
        );
    }
    host as u32
}

/// Vaddr → host address through `regions`. Pure. Values in no region
/// (ROM offsets, physical addresses) are zero-extended unchanged.
pub fn to_host_in(regions: &[Region], vaddr: u32) -> usize {
    for r in regions {
        let off = vaddr.wrapping_sub(r.vbase);
        if off < r.size {
            return r.host + off as usize;
        }
    }
    vaddr as usize
}

/// Host address → vaddr (registered regions).
pub fn to_vaddr(host: usize) -> u32 {
    to_vaddr_in(regions(), host)
}

/// Vaddr → host address (registered regions).
pub fn to_host(vaddr: u32) -> usize {
    to_host_in(regions(), vaddr)
}

/// `PW64_U32(p)` (pw64_native.h): pointer → 32-bit address for the C.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn pw64_to_vaddr(p: *const c_void) -> u32 {
    to_vaddr(p as usize)
}

/// `PW64_PTR(a)` (pw64_native.h): 32-bit address from the C → pointer.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn pw64_to_host(a: u32) -> *mut c_void {
    to_host(a) as *mut c_void
}

#[cfg(test)]
mod tests {
    use super::*;

    const HIGH: usize = 0x7_1234_0000;
    const R: [Region; 1] = [Region {
        vbase: 0x8000_0000,
        size: 0x80_0000,
        host: HIGH,
    }];

    #[test]
    fn identity_without_regions() {
        for a in [0u32, 0x1050, 0x800D_A800, 0xC000_1234, u32::MAX] {
            assert_eq!(to_host_in(&[], a), a as usize);
            assert_eq!(to_vaddr_in(&[], a as usize), a);
        }
        // Fixed-window build: a sign-extended s32 address truncates back.
        assert_eq!(to_vaddr_in(&[], 0xFFFF_FFFF_803D_A800), 0x803D_A800);
    }

    #[test]
    fn relocated_region_round_trips() {
        assert_eq!(to_host_in(&R, 0x803D_A800), HIGH + 0x3D_A800);
        assert_eq!(to_vaddr_in(&R, HIGH + 0x3D_A800), 0x803D_A800);
        assert_eq!(to_vaddr_in(&R, HIGH + 0x7F_FFFF), 0x807F_FFFF);
        // One past the end is not in the region.
        assert_eq!(to_host_in(&R, 0x8080_0000), 0x8080_0000);
    }

    /// ROM offsets and physical (K0 − 0x80000000) values pass through both
    /// ways, so `OS_PHYSICAL_TO_K0`-style arithmetic keeps working.
    #[test]
    fn rom_and_physical_pass_through() {
        for a in [0x1050u32, 0x003D_A800, 0x7FFF_FFFF] {
            assert_eq!(to_vaddr_in(&R, to_host_in(&R, a)), a);
        }
        assert_eq!(to_vaddr_in(&R, 0xFFFF_FFFF_803D_A800), 0x803D_A800);
    }

    #[test]
    #[should_panic(expected = "in no C-visible region")]
    fn unregistered_host_pointer_panics_when_relocated() {
        to_vaddr_in(&R, 0x7_9999_0000);
    }
}
