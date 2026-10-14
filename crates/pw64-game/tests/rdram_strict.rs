//! `PW64_RELOC=strict` (docs/notes/option-a.md S5) in its own process:
//! vaddr regions are once-only per process.

use pw64_game::memmap::{self, RDRAM_SIZE, Reloc};
use pw64_platform::{os::stack, vaddr};

/// As in the game exe: strict mode moves the Rust heap above 4 GB.
#[global_allocator]
static HEAP: pw64_game::high_heap::HighHeap = pw64_game::high_heap::HighHeap;

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
    // Stacks: vaddrs 0x80800000.. map to the high window; a stack local
    // round-trips and the HLE may read/write it through the region.
    let (w0, _) = stack::window();
    assert!(w0 >> 32 != 0, "stacks at {w0:#x}");
    assert_eq!(vaddr::to_host(0x8090_0000), w0 + 0x10_0000);
    assert_eq!(vaddr::to_vaddr(0x8090_0000), 0x8090_0000); // already a vaddr
    let s = stack::LowStack::new();
    let (lo, hi) = s.range();
    let v = vaddr::to_vaddr(hi - 8);
    assert!((0x8080_0000..0xA000_0000).contains(&v), "{v:#x}");
    assert_eq!(vaddr::to_host(v), hi - 8);
    assert!(memmap::hle_writable(lo, hi - lo));
    drop(s);
    // The image is an identity region.
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
    // The audit heap: blocks land above 4 GB, survive realloc, and a heap
    // pointer reaching PW64_U32 panics.
    let early = vec![1u8; 100];
    let a = pw64_game::high_heap::enable().expect("high heap");
    let mut v = vec![7u32; 1000];
    assert!((v.as_ptr() as usize).wrapping_sub(a) < 64 << 30);
    v.extend_from_slice(&[9; 5000]);
    assert!(v.as_ptr() as usize > a && v[999] == 7 && v[5999] == 9);
    let mut early = early;
    early.resize(10_000, 2);
    assert!(early.as_ptr() as usize > a && early[0] == 1);
    let p = v.as_ptr() as usize;
    assert!(std::panic::catch_unwind(move || vaddr::to_vaddr(p)).is_err());
}
