//! `PW64_RELOC=on` (docs/notes/option-a.md S8, the production relocated
//! mode) in its own process: vaddr regions are once-only per process.

use pw64_game::memmap::{self, RDRAM_BASE, RDRAM_SIZE, Reloc};
use pw64_platform::os::stack::{self, SEARCH_START};
use pw64_platform::vaddr;

/// Reserves (no access) `len` bytes at exactly `base`; true if it landed
/// there, i.e. nothing else holds the range.
fn range_free(base: usize, len: usize) -> bool {
    #[cfg(windows)]
    {
        unsafe extern "system" {
            fn VirtualAlloc(a: *mut u8, s: usize, t: u32, p: u32) -> *mut u8;
        }
        // SAFETY: plain reservation at a fixed address (MEM_RESERVE,
        // PAGE_NOACCESS); the result is only compared.
        unsafe { VirtualAlloc(base as *mut u8, len, 0x2000, 0x01) as usize == base }
    }
    #[cfg(target_os = "linux")]
    // SAFETY: PROT_NONE anonymous mapping that never clobbers; checked.
    unsafe {
        let p = libc::mmap(
            base as *mut libc::c_void,
            len,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE | libc::MAP_FIXED_NOREPLACE,
            -1,
            0,
        );
        p as usize == base
    }
}

#[test]
fn on_mode_regions_without_audits() {
    // SAFETY: the only test in this binary; nothing else reads the env yet.
    unsafe { std::env::set_var("PW64_RELOC", "on") };
    assert_eq!(Reloc::get(), Reloc::On);
    memmap::map_rdram_window();
    memmap::register_regions();
    let high = memmap::rdram_host();
    assert!(high >> 32 != 0, "RDRAM at {high:#x}");
    assert_eq!(vaddr::to_host(0x803D_A800), high + 0x3D_A800);
    let (w0, _) = stack::window();
    assert!(w0 >> 32 != 0, "stacks at {w0:#x}");
    assert_eq!(vaddr::to_host(0x8090_0000), w0 + 0x10_0000);
    let (e0, _) = memmap::exe_range();
    assert_eq!(vaddr::to_vaddr(e0 + 0x10), memmap::EXE_VBASE + 0x10);
    // Strict translation stays: a miss panics, naming the value.
    let miss = std::panic::catch_unwind(|| vaddr::to_host(0xA800_0000));
    let msg = miss.unwrap_err();
    let msg = msg.downcast_ref::<String>().map_or("", String::as_str);
    assert!(msg.contains("0xa8000000") && msg.contains("no C-visible region"), "{msg}");
    assert!(std::panic::catch_unwind(|| vaddr::to_vaddr(0x7_9999_0000)).is_err());
    // No audit pieces: the old windows are not reserved (macOS can't map
    // there), and the Rust heap is the system one.
    assert!(range_free(RDRAM_BASE, RDRAM_SIZE), "old RDRAM window reserved");
    assert!(range_free(SEARCH_START, 0x10_0000), "old stack window reserved");
}
