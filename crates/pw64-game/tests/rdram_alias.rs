//! `PW64_RELOC=alias` (docs/notes/option-a.md S4) in its own process:
//! vaddr regions are once-only per process.

use pw64_game::memmap::{self, RDRAM_BASE, RDRAM_SIZE, Reloc};
use pw64_platform::vaddr;

#[test]
fn alias_views_share_rdram() {
    // SAFETY: the only test in this binary; nothing else reads the env yet.
    unsafe { std::env::set_var("PW64_RELOC", "alias") };
    assert_eq!(Reloc::get(), Reloc::Alias);
    memmap::map_rdram_window();
    memmap::register_regions();
    let high = memmap::rdram_host();
    assert!(high >> 32 != 0, "high view {high:#x}");
    assert_eq!(vaddr::to_host(0x803D_A800), high + 0x3D_A800);
    assert_eq!(vaddr::to_vaddr(high + 0x3D_A800), 0x803D_A800);
    // Stacks / image stay low and pass through.
    assert_eq!(vaddr::to_vaddr(0x8090_0000), 0x8090_0000);
    // One block, two views.
    let low = RDRAM_BASE as *mut u32;
    let hi = high as *mut u32;
    // SAFETY: both views are mapped read/write, RDRAM_SIZE bytes each.
    unsafe {
        hi.add(0x1000).write_volatile(0xDEAD_BEEF);
        assert_eq!(low.add(0x1000).read_volatile(), 0xDEAD_BEEF);
        low.add(RDRAM_SIZE / 4 - 1).write_volatile(0x1234_5678);
        assert_eq!(hi.add(RDRAM_SIZE / 4 - 1).read_volatile(), 0x1234_5678);
    }
    // The HLE may write the view the C translates to, not the old window.
    assert!(memmap::hle_writable(high, RDRAM_SIZE));
    assert!(!memmap::hle_writable(RDRAM_BASE, 4));
}
