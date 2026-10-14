//! `PW64_RELOC=strict` (docs/notes/option-a.md S5) in its own process:
//! vaddr regions are once-only per process.

use pw64_game::memmap::{self, RDRAM_SIZE, Reloc};
use pw64_platform::vaddr;

#[test]
fn strict_regions() {
    // SAFETY: the only test in this binary; nothing else reads the env yet.
    unsafe { std::env::set_var("PW64_RELOC", "strict") };
    assert_eq!(Reloc::get(), Reloc::Strict);
    memmap::map_rdram_window();
    memmap::register_regions();
    let high = memmap::rdram_host();
    assert!(high >> 32 != 0, "RDRAM at {high:#x}");
    assert_eq!(vaddr::to_host(0x803D_A800), high + 0x3D_A800);
    assert_eq!(vaddr::to_vaddr(high + RDRAM_SIZE - 1), 0x807F_FFFF);
    // Stacks and the image are identity regions.
    assert_eq!(vaddr::to_vaddr(0x8090_0000), 0x8090_0000);
    let (e0, _) = memmap::exe_range();
    assert_eq!(vaddr::to_host(e0 as u32), e0);
    // ROM offsets / physical values pass; the rest panics.
    assert_eq!(vaddr::to_host(0x3D_A800), 0x3D_A800);
    assert!(std::panic::catch_unwind(|| vaddr::to_host(0xA800_0000)).is_err());
    assert!(std::panic::catch_unwind(|| vaddr::to_vaddr(0x7_9999_0000)).is_err());
    // RDRAM is usable; the HLE writes it, not the old window.
    // SAFETY: RDRAM_SIZE read/write bytes at `high`.
    unsafe {
        let p = (high as *mut u32).add(RDRAM_SIZE / 4 - 1);
        p.write_volatile(0x1234_5678);
        assert_eq!(p.read_volatile(), 0x1234_5678);
    }
    assert!(memmap::hle_writable(high, RDRAM_SIZE));
    assert!(!memmap::hle_writable(memmap::RDRAM_BASE, 4));
}
