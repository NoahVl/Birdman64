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
//!
//! Fast path: the macros are `static inline` C that translate through one
//! region ([`VMap`], C global `pw64_vmap`) and call the Rust functions only
//! on a miss. With no regions the [`VMap`] is [`VMap::IDENTITY`], which hits
//! for every 32-bit value, so the fixed-window build never calls Rust.

use core::cell::UnsafeCell;
use core::ffi::c_void;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

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

/// The one region the inline C fast path translates (`struct pw64_vmap`,
/// pw64_native.h; same layout in dylib/pw64_dll_shim.c). `size` is 64-bit so
/// [`VMap::IDENTITY`] can cover all 2^32 vaddrs.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VMap {
    pub host: usize,
    pub size: u64,
    pub vbase: u32,
    pub pad: u32,
}

impl VMap {
    /// No regions: vaddr == host for every 32-bit value.
    pub const IDENTITY: VMap = VMap {
        host: 0,
        size: 1 << 32,
        vbase: 0,
        pad: 0,
    };

    /// The fast-path region for `regions`: RDRAM (vbase 0x80000000) if
    /// registered, else the first; [`Self::IDENTITY`] if none.
    pub fn for_regions(regions: &[Region]) -> VMap {
        regions
            .iter()
            .find(|r| r.vbase == 0x8000_0000)
            .or(regions.first())
            .map_or(Self::IDENTITY, |r| VMap {
                host: r.host,
                size: r.size as u64,
                vbase: r.vbase,
                pad: 0,
            })
    }

    /// `PW64_PTR`'s inline part (pw64_native.h `pw64_ptr_fast`): `None` =
    /// miss, the C calls [`pw64_to_host`].
    pub fn to_host(&self, vaddr: u32) -> Option<usize> {
        let off = vaddr.wrapping_sub(self.vbase);
        ((off as u64) < self.size).then(|| self.host.wrapping_add(off as usize))
    }

    /// `PW64_U32`'s inline part (pw64_native.h `pw64_u32_fast`): `None` =
    /// miss, the C calls [`pw64_to_vaddr`].
    pub fn to_vaddr(&self, host: usize) -> Option<u32> {
        let off = host.wrapping_sub(self.host);
        ((off as u64) < self.size).then(|| self.vbase.wrapping_add(off as u32))
    }
}

/// `UnsafeCell` so the C may read it; written only by [`set_regions`]
/// before the C runs.
#[repr(transparent)]
pub struct VMapCell(UnsafeCell<VMap>);

// SAFETY: written once before the C starts (set_regions), read-only after.
unsafe impl Sync for VMapCell {}

impl VMapCell {
    /// The current fast-path region, for Rust consumers' per-word inline
    /// translation (the C reads this cell directly as its `pw64_vmap`).
    pub fn load(&self) -> &VMap {
        // SAFETY: written once before the C runs (set_regions's contract),
        // read-only after — the C reads the same cell non-atomically too.
        unsafe { &*self.0.get() }
    }
}

/// The static build's fast-path region (read by the C as `pw64_vmap`).
#[allow(non_upper_case_globals)]
#[unsafe(no_mangle)]
pub static pw64_vmap: VMapCell = VMapCell(UnsafeCell::new(VMap::IDENTITY));

/// The game module's own `pw64_vmap` (dylib flavour: a module cannot read
/// exe data), or null.
static MODULE_VMAP: AtomicPtr<VMap> = AtomicPtr::new(core::ptr::null_mut());

/// Dylib flavour: the loaded module's `pw64_vmap` (dylib/pw64_dll_shim.c).
/// [`set_regions`] keeps it equal to ours; the current value is copied now,
/// so the order of the two calls doesn't matter.
///
/// # Safety
/// `p` points at the module's `struct pw64_vmap`, valid for the process
/// lifetime; the C must not be running yet.
pub unsafe fn set_module_vmap(p: *mut VMap) {
    MODULE_VMAP.store(p, Ordering::Release);
    // SAFETY: caller contract; our cell is only written before the C runs.
    unsafe { p.write(*pw64_vmap.0.get()) };
}

/// Registers the C-visible regions, once, before the C runs. Panics on a
/// second call, a vaddr range below 0x80000000 or past 4 GB, a host block
/// below 4 GB (a host address that fits 32 bits could not be told apart from
/// a vaddr) unless it is an identity region (`host == vbase`: a block that
/// still sits at its vaddr, e.g. the stacks or the image in strict mode), or
/// overlapping regions (the inline fast path tries one region before the
/// rest). Without this call both conversions are the identity.
pub fn set_regions(regions: Vec<Region>) {
    for (i, r) in regions.iter().enumerate() {
        assert!(
            r.vbase & 0x8000_0000 != 0 && r.vbase.checked_add(r.size).is_some(),
            "vaddr region {r:?}: must lie in 0x80000000..4 GB"
        );
        assert!(
            r.host >> 32 != 0 || r.host == r.vbase as usize,
            "vaddr region {r:?}: host block must lie above 4 GB (or at its vaddr)"
        );
        for o in &regions[..i] {
            let v = r.vbase < o.vbase + o.size && o.vbase < r.vbase + r.size;
            let h = r.host < o.host + o.size as usize && o.host < r.host + r.size as usize;
            assert!(!v && !h, "vaddr regions {o:?} and {r:?} overlap");
        }
    }
    let map = VMap::for_regions(&regions);
    REGIONS
        .set(regions)
        .expect("vaddr regions are registered once, before the C runs");
    // SAFETY: before the C runs (contract above); nothing reads it concurrently.
    unsafe { *pw64_vmap.0.get() = map };
    let module = MODULE_VMAP.load(Ordering::Acquire);
    if !module.is_null() {
        // SAFETY: set_module_vmap's contract.
        unsafe { module.write(map) };
    }
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

/// Strict translation (`PW64_RELOC=on|strict`, option-a.md S5/S8): see
/// [`set_strict`]. Only the miss path reads it.
static STRICT: AtomicBool = AtomicBool::new(false);

/// Strict mode: from now on [`to_vaddr`] / [`to_host`] (and so `PW64_U32` /
/// `PW64_PTR`) panic instead of passing through a value that should have
/// been in a region ([`to_vaddr_strict_in`], [`to_host_strict_in`]). Call
/// with [`set_regions`], before the C runs.
pub fn set_strict() {
    STRICT.store(true, Ordering::Relaxed);
}

/// [`to_vaddr_in`], strict: a host address in no region must be below
/// 0x80000000 (a ROM offset, physical value or small int cast to a pointer)
/// or already a vaddr of some region (`OS_PHYSICAL_TO_K0` and friends apply
/// `PW64_U32` to ints; no host memory lives at a vaddr in strict mode except
/// identity regions, so this is idempotence, not a hidden pointer). Anything
/// else (an unregistered host pointer, a sign-extended s32 vaddr that some C
/// site cast to a pointer) panics, naming the value. Pure.
pub fn to_vaddr_strict_in(regions: &[Region], host: usize) -> u32 {
    for r in regions {
        let off = host.wrapping_sub(r.host);
        if off < r.size as usize {
            return r.vbase + off as u32;
        }
    }
    let is_vaddr = |v: u32| regions.iter().any(|r| v.wrapping_sub(r.vbase) < r.size);
    assert!(
        host < 0x8000_0000 || (host >> 32 == 0 && is_vaddr(host as u32)),
        "PW64_U32({host:#x}): host address in no C-visible region (strict mode; \
         the backtrace names the C site)"
    );
    host as u32
}

/// [`to_host_in`], strict: a vaddr with bit 31 set in no region (an
/// unregistered or stale N64 address) panics; ROM offsets and physical
/// values pass through. Pure.
pub fn to_host_strict_in(regions: &[Region], vaddr: u32) -> usize {
    for r in regions {
        let off = vaddr.wrapping_sub(r.vbase);
        if off < r.size {
            return r.host + off as usize;
        }
    }
    assert!(
        vaddr < 0x8000_0000,
        "PW64_PTR({vaddr:#x}): vaddr in no C-visible region (strict mode; \
         the backtrace names the C site)"
    );
    vaddr as usize
}

/// Host address → vaddr (registered regions).
pub fn to_vaddr(host: usize) -> u32 {
    if STRICT.load(Ordering::Relaxed) {
        to_vaddr_strict_in(regions(), host)
    } else {
        to_vaddr_in(regions(), host)
    }
}

/// Vaddr → host address (registered regions).
pub fn to_host(vaddr: u32) -> usize {
    if STRICT.load(Ordering::Relaxed) {
        to_host_strict_in(regions(), vaddr)
    } else {
        to_host_in(regions(), vaddr)
    }
}

/// `PW64_U32(p)` (pw64_native.h) when the inline fast path misses:
/// pointer → 32-bit address for the C.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn pw64_to_vaddr(p: *const c_void) -> u32 {
    to_vaddr(p as usize)
}

/// `PW64_PTR(a)` (pw64_native.h) when the inline fast path misses:
/// 32-bit address from the C → pointer.
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

    /// The inline C fast path, then the Rust call on a miss, equals the
    /// plain lookup for every kind of value: no regions, one, several.
    #[test]
    fn fast_path_matches_lookup() {
        let multi = [
            Region {
                vbase: 0x8080_0000,
                size: 0x100_0000,
                host: 0x9_0000_0000,
            },
            R[0],
            Region {
                vbase: 0xB000_0000,
                size: 0x1000_0000,
                host: 0x8_0000_0000,
            },
        ];
        let vaddrs = [
            0u32,
            0x1050,
            0x003D_A800,
            0x7FFF_FFFF,
            0x8000_0000,
            0x803D_A800,
            0x807F_FFFF,
            0x8080_0000,
            0x817F_FFFF,
            0x8180_0000,
            0xB000_0010,
            0xC000_1234,
            u32::MAX,
        ];
        for regions in [&[][..], &R[..], &multi[..]] {
            let m = VMap::for_regions(regions);
            for &a in &vaddrs {
                let h = to_host_in(regions, a);
                assert_eq!(m.to_host(a).unwrap_or(h), h, "{a:#x}");
                let back = to_vaddr_in(regions, h);
                assert_eq!(m.to_vaddr(h).unwrap_or(back), back, "{h:#x}");
            }
            // Sign-extended s32 (miss in every map, the slow path truncates).
            assert_eq!(m.to_vaddr(0xFFFF_FFFF_803D_A800), None);
        }
        // No regions: every 32-bit value hits (the C never calls Rust).
        let id = VMap::for_regions(&[]);
        for &a in &vaddrs {
            assert_eq!(id.to_host(a), Some(a as usize));
            assert_eq!(id.to_vaddr(a as usize), Some(a));
        }
        assert_eq!(VMap::for_regions(&multi).vbase, 0x8000_0000);
    }

    /// Panics before registering anything, so the once-only global stays unset.
    #[test]
    #[should_panic(expected = "overlap")]
    fn overlapping_regions_rejected() {
        let mut b = R[0];
        b.vbase += 0x10_0000;
        b.host = 0x9_0000_0000;
        set_regions(vec![R[0], b]);
    }

    #[test]
    #[should_panic(expected = "in no C-visible region")]
    fn unregistered_host_pointer_panics_when_relocated() {
        to_vaddr_in(&R, 0x7_9999_0000);
    }

    /// Strict: identity regions translate to themselves; ROM offsets, physical
    /// values and (to_vaddr) vaddrs pass; everything else panics.
    #[test]
    fn strict_mode() {
        let rs = [
            R[0],
            Region {
                vbase: 0x8080_0000,
                size: 0x1000_0000,
                host: 0x8080_0000,
            },
        ];
        assert_eq!(to_host_strict_in(&rs, 0x8090_0010), 0x8090_0010);
        assert_eq!(to_vaddr_strict_in(&rs, 0x8090_0010), 0x8090_0010);
        assert_eq!(to_host_strict_in(&rs, 0x803D_A800), HIGH + 0x3D_A800);
        for a in [0u32, 0x1050, 0x003D_A800, 0x7FFF_FFFF] {
            assert_eq!(to_vaddr_strict_in(&rs, to_host_strict_in(&rs, a)), a);
        }
        let panics = |f: fn() -> usize| std::panic::catch_unwind(f).is_err();
        let v = |h: usize| std::panic::catch_unwind(|| to_vaddr_strict_in(&R, h)).is_err();
        assert!(panics(|| to_host_strict_in(&R, 0x8090_0000)));
        assert!(panics(|| to_host_strict_in(&R, 0xC000_0000)));
        // Already a vaddr (an int through PW64_U32): idempotent.
        assert_eq!(to_vaddr_strict_in(&R, 0x8004_1820), 0x8004_1820);
        assert!(v(0x8090_0000));
        assert!(v(0xFFFF_FFFF_803D_A800));
        assert!(v(0x7_9999_0000));
    }
}
