//! Host address-space setup for the "low 4 GB identity window" model
//! (docs/notes/native-build.md §2, option C).
//!
//! The C code stores addresses in 32-bit ints and Gfx words and uses N64 KSEG0
//! literals (0x80xxxxxx) directly, so:
//! - RDRAM is mapped at 0x80000000..0x80800000, exactly where the N64 has it;
//! - the exe is linked at a fixed base 0xC0000000 (Windows: `/BASE
//!   /DYNAMICBASE:NO /HIGHENTROPYVA:NO`; Linux: non-PIE ET_EXEC via lld
//!   `--no-pie --image-base`, see pw64-game/build.rs), so every C global and
//!   function address fits in 32 bits and has bit 31 set (the C code treats
//!   addresses with bit 31 clear as ROM offsets);
//! - [`check_low_4gb`] asserts all of this at startup.

use core::ffi::c_void;

/// N64 KSEG0 RDRAM window the C code addresses with literals.
pub const RDRAM_BASE: usize = 0x8000_0000;
/// 8 MB: Expansion Pak size (`osMemSize`, the texture heap runs up to it).
pub const RDRAM_SIZE: usize = 0x80_0000;
/// Everything the C code can see must lie below this.
pub const LIMIT_4GB: usize = 1 << 32;

#[cfg(windows)]
mod sys {
    use core::ffi::c_void;
    unsafe extern "system" {
        pub fn VirtualAlloc(addr: *mut c_void, size: usize, ty: u32, prot: u32) -> *mut c_void;
    }
    pub const MEM_COMMIT: u32 = 0x1000;
    pub const MEM_RESERVE: u32 = 0x2000;
    pub const PAGE_READWRITE: u32 = 0x04;

    unsafe extern "C" {
        /// Provided by the MSVC linker: the DOS header of this image.
        pub static __ImageBase: u8;
    }
}

/// Maps the zeroed RDRAM window at exactly [`RDRAM_BASE`]. Panics if the
/// range is taken (e.g. the exe was relocated over it). Call once, first.
pub fn map_rdram_window() {
    #[cfg(windows)]
    {
        // SAFETY: plain Win32 allocation at a fixed address; failure is checked.
        let p = unsafe {
            sys::VirtualAlloc(
                RDRAM_BASE as *mut c_void,
                RDRAM_SIZE,
                sys::MEM_RESERVE | sys::MEM_COMMIT,
                sys::PAGE_READWRITE,
            )
        };
        assert_eq!(
            p as usize, RDRAM_BASE,
            "could not map the RDRAM window at {RDRAM_BASE:#x}"
        );
    }
    #[cfg(target_os = "linux")]
    {
        // MAP_FIXED_NOREPLACE: fail (EEXIST) rather than clobber a mapping;
        // pre-4.17 kernels take it as a hint, hence the address check.
        // SAFETY: anonymous mapping at a fixed address; failure is checked.
        let p = unsafe {
            libc::mmap(
                RDRAM_BASE as *mut c_void,
                RDRAM_SIZE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED_NOREPLACE,
                -1,
                0,
            )
        };
        assert_eq!(
            p as usize,
            RDRAM_BASE,
            "could not map the RDRAM window at {RDRAM_BASE:#x}: {}",
            std::io::Error::last_os_error()
        );
    }
    // macOS/arm64 forbids low mappings (`__PAGEZERO`): native-build.md §10.
    #[cfg(not(any(windows, target_os = "linux")))]
    compile_error!("RDRAM window mapping is implemented for Windows and Linux only");
}

/// `(start, end)` of this exe's loaded image (code, data, BSS).
#[cfg(target_os = "linux")]
fn exe_image_range() -> (usize, usize) {
    unsafe extern "C" {
        // Defined by the linker (lld and GNU ld): first byte of the first
        // PT_LOAD and the end of .bss.
        static __executable_start: u8;
        static _end: u8;
    }
    (
        (&raw const __executable_start) as usize,
        (&raw const _end) as usize,
    )
}

/// `(start, end)` of this exe's loaded image (code, data, BSS).
#[cfg(windows)]
fn exe_image_range() -> (usize, usize) {
    // SAFETY: __ImageBase is the mapped PE header of this module; e_lfanew
    // (DOS header +0x3C) points at "PE\0\0", and SizeOfImage is at optional
    // header +56 (PE sig 4 + file header 20 = 24).
    unsafe {
        let base = &raw const sys::__ImageBase;
        let nt = base.add((base.add(0x3C) as *const u32).read_unaligned() as usize);
        let size = (nt.add(24 + 56) as *const u32).read_unaligned() as usize;
        (base as usize, base as usize + size)
    }
}

/// Panics unless `[addr, addr + len)` lies below 4 GB.
pub fn assert_low(what: &str, addr: usize, len: usize) {
    assert!(
        addr.checked_add(len).is_some_and(|end| end <= LIMIT_4GB),
        "{what} at {addr:#x}..+{len:#x} is not below 4 GB: the C code would truncate it"
    );
}

// A C global (memory.c), as a sample of the C data.
#[cfg(feature = "static")]
unsafe extern "C" {
    static D_802B892C: u32;
}

/// `(start, end)` of everything the C's code and globals live in: the exe
/// image (`static`: all C globals), plus (`dylib`) the game module below it
/// at 0xB0000000. hle.rs maps this range 1:1 (the gap between the two holds
/// nothing C-visible).
pub fn image_range() -> (usize, usize) {
    let (start, end) = exe_image_range();
    #[cfg(feature = "dylib")]
    let start = start.min(crate::dylib::game().range.0);
    (start, end)
}

/// `[start, end)` of this exe's image, computed once.
pub fn exe_range() -> (usize, usize) {
    static EXE: std::sync::OnceLock<(usize, usize)> = std::sync::OnceLock::new();
    *EXE.get_or_init(exe_image_range)
}

/// `[start, end)` of the loaded game module (`dylib`; `None` before
/// `dylib::load` and in the `static` flavour).
pub fn module_range() -> Option<(usize, usize)> {
    #[cfg(feature = "dylib")]
    {
        crate::dylib::loaded_range()
    }
    #[cfg(not(feature = "dylib"))]
    {
        None
    }
}

/// `[addr, addr + len)` lies within `[start, end)`. Pure.
pub fn range_holds((start, end): (usize, usize), addr: usize, len: usize) -> bool {
    addr >= start && addr.checked_add(len).is_some_and(|e| e <= end)
}

/// The RSP HLE (display lists, audio command lists) may read
/// `[addr, addr + len)`: the RDRAM window, a live thread stack's usable
/// range, or the exe / game module image (static display lists in .data).
/// Not the reserved-but-uncommitted rest of the stack window, the stack
/// guard pages, or the gap between the module and the exe. Cheap (per
/// command): no locks, no allocation.
pub fn hle_readable(addr: usize, len: usize) -> bool {
    hle_writable(addr, len)
        || range_holds(exe_range(), addr, len)
        || module_range().is_some_and(|r| range_holds(r, addr, len))
}

/// The RSP HLE may write `[addr, addr + len)`: only the RDRAM window and live
/// thread stacks, never code or the images (audio HLE DMA writes).
pub fn hle_writable(addr: usize, len: usize) -> bool {
    range_holds((RDRAM_BASE, RDRAM_BASE + RDRAM_SIZE), addr, len)
        || pw64_platform::os::stack::in_live_stack(addr, len)
}

/// Address of a sample C global and of `bootproc` (the real C one).
fn c_samples() -> (usize, usize) {
    #[cfg(feature = "static")]
    {
        (
            (&raw const D_802B892C) as usize,
            crate::bootproc as unsafe extern "C-unwind" fn(_) as usize,
        )
    }
    #[cfg(feature = "dylib")]
    {
        let g = crate::dylib::game();
        (g.sample_global as usize, g.bootproc as usize)
    }
}

/// Asserts that everything the C code sees lies below 4 GB: the exe image
/// (all C globals and functions, bit 31 set) and the RDRAM window (plus, on
/// Windows, the host heap and the current stack). Returns a one-line summary
/// of the addresses.
pub fn check_low_4gb() -> String {
    let (img0, img1) = image_range();
    assert_low("exe image", img0, img1 - img0);
    assert!(
        img0 >= RDRAM_BASE + RDRAM_SIZE,
        "exe image at {img0:#x}: must be above the RDRAM window (bit 31 set = RAM for the C)"
    );
    // Implied by the two checks above, stated for the reader: the C treats
    // `(u32)p & 0x80000000 == 0` as a ROM offset (memory.c, filesystem.c).
    assert!(
        img0 & 0x8000_0000 != 0 && (img1 - 1) & 0x8000_0000 != 0,
        "exe image {img0:#x}..{img1:#x} must have bit 31 set (else the C reads it as ROM)"
    );
    let (c_global, c_func) = c_samples();
    for (what, a) in [("C global", c_global), ("C function", c_func)] {
        assert!(
            (img0..img1).contains(&a),
            "{what} {a:#x} outside the exe image"
        );
    }
    assert_low("RDRAM window", RDRAM_BASE, RDRAM_SIZE);
    let heap = Box::new(0u64);
    let heap = &raw const *heap as usize;
    let local = 0u8;
    let stack = &raw const local as usize;
    // Windows happens to put the host heap and the main stack low, and this
    // was asserted from the start. Neither is ever C-visible: the C allocates
    // only from the window/arena, Rust hands it no heap buffers (statics like
    // `osTvType` live in the image), and it runs on the pw64-platform
    // coroutine stacks (0x80800000..). On Linux both are high (glibc brk is
    // randomised up to 1 GB past the image; stacks at 0x7ff…), so only report.
    #[cfg(windows)]
    {
        assert_low("host heap", heap, 8);
        assert_low("stack", stack, 1);
    }
    format!(
        "memmap: image {img0:#x}..{img1:#x} (C global {c_global:#x}, fn {c_func:#x}), \
         RDRAM {RDRAM_BASE:#x}..{:#x}, heap {heap:#x}, stack {stack:#x}",
        RDRAM_BASE + RDRAM_SIZE
    )
}

/// Startup: map the RDRAM window, then check the address-space invariants.
/// `dylib`: the game module isn't loaded yet (the launcher builds or finds it
/// after the ROM step, `pw64` firstrun.rs), so the check runs there, after
/// `dylib::load`; this only maps the window.
///
/// Windows: also reserves the coroutine-stack window and (`dylib`) the game
/// module's fixed range right away, before the window and the GPU driver
/// exist: a large driver reservation landing there later would make the
/// stacks or the `/FIXED` module fail ("address range taken").
pub fn init() -> String {
    map_rdram_window();
    let stacks = pw64_platform::os::stack::reserve_region();
    #[cfg(feature = "dylib")]
    let module = crate::dylib::reserve_module_range();
    #[cfg(not(feature = "dylib"))]
    let module = false;
    let reserved = format!("reserved: stacks {stacks}, module {module}");
    #[cfg(feature = "static")]
    {
        format!("{}, {reserved}", check_low_4gb())
    }
    #[cfg(not(feature = "static"))]
    {
        format!(
            "memmap: RDRAM {RDRAM_BASE:#x}..{:#x} (image check after the game module loads), {reserved}",
            RDRAM_BASE + RDRAM_SIZE
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_holds_bounds() {
        let r = (0x1000, 0x2000);
        assert!(range_holds(r, 0x1000, 0x1000));
        assert!(range_holds(r, 0x1ffc, 4));
        assert!(!range_holds(r, 0x1ffc, 8));
        assert!(!range_holds(r, 0xffc, 8));
        assert!(!range_holds(r, usize::MAX - 2, 4));
    }

    /// Reads cover the exe image; writes never do (no code patching).
    #[test]
    fn hle_ranges() {
        assert!(hle_writable(RDRAM_BASE, RDRAM_SIZE));
        assert!(!hle_writable(RDRAM_BASE + RDRAM_SIZE - 4, 8));
        let (e0, e1) = exe_range();
        // black_box: in release LLVM folds `fn address - RDRAM_BASE` into a
        // rip-relative addend that overflows 32 bits ("value of -2147483712 is
        // too large for field of 4 bytes", Linux dylib test build).
        let code = std::hint::black_box(hle_ranges as fn() as usize);
        assert!((e0..e1).contains(&code));
        assert!(hle_readable(code, 4));
        assert!(!hle_writable(code, 4));
        // Stack window outside any live slot, and the module/exe gap.
        assert!(!hle_readable(0x9FF8_0000, 4));
        assert!(!hle_readable(0xB800_0000, 4));
    }
}
