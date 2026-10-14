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
use std::sync::atomic::{AtomicUsize, Ordering};

/// N64 KSEG0 RDRAM window the C code addresses with literals.
pub const RDRAM_BASE: usize = 0x8000_0000;
/// 8 MB: Expansion Pak size (`osMemSize`, the texture heap runs up to it).
pub const RDRAM_SIZE: usize = 0x80_0000;
/// Everything the C code can see must lie below this.
pub const LIMIT_4GB: usize = 1 << 32;

/// Relocation mode, `PW64_RELOC` (docs/notes/option-a.md).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reloc {
    /// Unset / `off`: the fixed low window, no vaddr regions (identity).
    Off,
    /// `alias`: RDRAM is one shared-memory block seen twice, at
    /// [`RDRAM_BASE`] (for C sites that still use the window directly) and
    /// above 4 GB, registered as the vaddr region the C translates to.
    Alias,
    /// `strict`: RDRAM mapped once, above 4 GB; the low window is reserved
    /// no-access (an untranslated literal faults at its C site), the same for
    /// the stack window and the Rust heap (`high_heap`). The image is an
    /// identity region; any other value with bit 31 set that
    /// reaches `PW64_U32`/`PW64_PTR` panics (`vaddr::set_strict`).
    Strict,
}

impl Reloc {
    /// Parses a `PW64_RELOC` value. Pure.
    pub fn parse(v: Option<&str>) -> Result<Reloc, String> {
        match v.map(str::trim) {
            None | Some("" | "0" | "off") => Ok(Reloc::Off),
            Some("alias") => Ok(Reloc::Alias),
            Some("strict") => Ok(Reloc::Strict),
            Some(o) => Err(format!("PW64_RELOC={o}: expected off, alias or strict")),
        }
    }

    /// The process's mode (`PW64_RELOC`, read once). Panics on a bad value.
    pub fn get() -> Reloc {
        static MODE: std::sync::OnceLock<Reloc> = std::sync::OnceLock::new();
        *MODE.get_or_init(|| {
            let v = std::env::var("PW64_RELOC").ok();
            Reloc::parse(v.as_deref()).unwrap_or_else(|e| panic!("{e}"))
        })
    }
}

/// Host address of RDRAM as the C and the HLE address it (vaddr
/// 0x80000000): [`RDRAM_BASE`], or the high view in [`Reloc::Alias`].
static RDRAM_HOST: AtomicUsize = AtomicUsize::new(RDRAM_BASE);

/// Host address of RDRAM's vaddr 0x80000000 (see [`RDRAM_HOST`]).
pub fn rdram_host() -> usize {
    RDRAM_HOST.load(Ordering::Relaxed)
}

/// High-view candidates for [`Reloc::Alias`]: above 4 GB, 64 KB aligned,
/// and with low 32 bits (0x2000_0000) far from 0x80000000, so a pointer
/// that some straggler truncates to 32 bits reads as a ROM offset (bit 31
/// clear) instead of silently hitting the low view.
fn alias_candidates() -> impl Iterator<Item = usize> {
    (0..16).map(|k| 0x7_2000_0000 + k * 0x1_0000_0000)
}

#[cfg(windows)]
mod sys {
    use core::ffi::c_void;
    unsafe extern "system" {
        pub fn VirtualAlloc(addr: *mut c_void, size: usize, ty: u32, prot: u32) -> *mut c_void;
        pub fn CreateFileMappingW(
            file: *mut c_void,
            attrs: *mut c_void,
            prot: u32,
            size_high: u32,
            size_low: u32,
            name: *const u16,
        ) -> *mut c_void;
        pub fn MapViewOfFileEx(
            mapping: *mut c_void,
            access: u32,
            off_high: u32,
            off_low: u32,
            size: usize,
            base: *mut c_void,
        ) -> *mut c_void;
    }
    pub const MEM_COMMIT: u32 = 0x1000;
    pub const MEM_RESERVE: u32 = 0x2000;
    pub const PAGE_READWRITE: u32 = 0x04;
    pub const PAGE_NOACCESS: u32 = 0x01;
    pub const FILE_MAP_WRITE: u32 = 0x02;
    pub const INVALID_HANDLE_VALUE: *mut c_void = usize::MAX as *mut c_void;

    unsafe extern "C" {
        /// Provided by the MSVC linker: the DOS header of this image.
        pub static __ImageBase: u8;
    }
}

/// Maps the zeroed RDRAM window at exactly [`RDRAM_BASE`]. Panics if the
/// range is taken (e.g. the exe was relocated over it). Call once, first.
/// [`Reloc::Alias`]: maps the shared block twice ([`map_rdram_alias`]);
/// [`Reloc::Strict`]: once, high ([`map_rdram_strict`]). Either way the
/// vaddr regions are registered later, by [`register_regions`].
pub fn map_rdram_window() {
    match Reloc::get() {
        Reloc::Off => {}
        Reloc::Alias => {
            RDRAM_HOST.store(map_rdram_alias(), Ordering::Relaxed);
            return;
        }
        Reloc::Strict => {
            RDRAM_HOST.store(map_rdram_strict(), Ordering::Relaxed);
            relocate_stacks();
            return;
        }
    }
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

/// [`Reloc::Alias`]: one zeroed shared-memory block of [`RDRAM_SIZE`],
/// mapped at [`RDRAM_BASE`] and at the first free [`alias_candidates`]
/// address; returns the high view. Both views are the same pages, so C
/// sites that still use a window literal see what the translated sites
/// wrote. Panics if either view can't be mapped.
fn map_rdram_alias() -> usize {
    #[cfg(windows)]
    let high = {
        // SAFETY: pagefile-backed section (zero-filled) and views at fixed
        // addresses; every result is checked. The handle stays open for the
        // process lifetime (the views keep the section alive anyway).
        unsafe {
            let h = sys::CreateFileMappingW(
                sys::INVALID_HANDLE_VALUE,
                core::ptr::null_mut(),
                sys::PAGE_READWRITE,
                0,
                RDRAM_SIZE as u32,
                core::ptr::null(),
            );
            assert!(
                !h.is_null(),
                "RDRAM alias: CreateFileMapping failed: {}",
                std::io::Error::last_os_error()
            );
            let view = |base: usize| {
                let p = base as *mut c_void;
                sys::MapViewOfFileEx(h, sys::FILE_MAP_WRITE, 0, 0, RDRAM_SIZE, p) as usize
            };
            let low = view(RDRAM_BASE);
            assert_eq!(
                low,
                RDRAM_BASE,
                "could not map the RDRAM window at {RDRAM_BASE:#x}: {}",
                std::io::Error::last_os_error()
            );
            alias_candidates().find(|&a| view(a) == a)
        }
    };
    #[cfg(target_os = "linux")]
    let high = {
        // SAFETY: memfd (zero-filled) mapped shared at fixed addresses with
        // MAP_FIXED_NOREPLACE (never clobbers; pre-4.17 kernels take it as
        // a hint, hence the address checks); every result is checked. The
        // fd stays open (the mappings keep the file alive anyway).
        unsafe {
            let fd = libc::memfd_create(c"pw64-rdram".as_ptr(), libc::MFD_CLOEXEC);
            assert!(
                fd >= 0 && libc::ftruncate(fd, RDRAM_SIZE as libc::off_t) == 0,
                "RDRAM alias: memfd: {}",
                std::io::Error::last_os_error()
            );
            let view = |base: usize| {
                let p = libc::mmap(
                    base as *mut c_void,
                    RDRAM_SIZE,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED | libc::MAP_FIXED_NOREPLACE,
                    fd,
                    0,
                );
                if p != libc::MAP_FAILED && p as usize != base {
                    libc::munmap(p, RDRAM_SIZE);
                }
                p as usize
            };
            let low = view(RDRAM_BASE);
            assert_eq!(
                low,
                RDRAM_BASE,
                "could not map the RDRAM window at {RDRAM_BASE:#x}: {}",
                std::io::Error::last_os_error()
            );
            alias_candidates().find(|&a| view(a) == a)
        }
    };
    high.expect("RDRAM alias: no free high address for the second view")
}

/// High candidates for the stack window in [`Reloc::Strict`]: like
/// [`alias_candidates`], with low 32 bits 0x4000_0000.. (bit 31 clear, apart
/// from RDRAM's; the 504 MB window ends below 0x6000_0000).
fn stack_candidates() -> impl Iterator<Item = usize> {
    (0..16).map(|k| 0x7_4000_0000 + k * 0x1_0000_0000)
}

/// Reserves `[base, base + len)` with no access and no commit. True if it
/// landed exactly there (false: the range is taken).
fn reserve_noaccess(base: usize, len: usize) -> bool {
    #[cfg(windows)]
    {
        // SAFETY: plain Win32 reservation at a fixed address; checked.
        let p = unsafe { sys::VirtualAlloc(base as *mut c_void, len, sys::MEM_RESERVE, sys::PAGE_NOACCESS) };
        p as usize == base
    }
    #[cfg(target_os = "linux")]
    // SAFETY: anonymous PROT_NONE mapping, MAP_FIXED_NOREPLACE (never
    // clobbers; old kernels take it as a hint, hence the check).
    unsafe {
        let p = libc::mmap(
            base as *mut c_void,
            len,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE | libc::MAP_FIXED_NOREPLACE,
            -1,
            0,
        );
        if p != libc::MAP_FAILED && p as usize != base {
            libc::munmap(p, len);
        }
        p as usize == base
    }
}

/// [`Reloc::Strict`] (option-a.md S6): the coroutine stack window moves to
/// the first free [`stack_candidates`] address (`register_regions` maps
/// vaddrs 0x80800000.. there) and the old window is reserved no-access, so
/// a stack address that skipped the vaddr layer faults. Before any stack
/// exists. Panics if either fails.
fn relocate_stacks() {
    use pw64_platform::os::stack::{self, SEARCH_START, WINDOW_SIZE};
    stack_candidates()
        .find(|&a| stack::reserve_region_at(a))
        .expect("stack window: no free high address");
    assert!(
        reserve_noaccess(SEARCH_START, WINDOW_SIZE),
        "could not reserve the old stack window at {SEARCH_START:#x}: {}",
        std::io::Error::last_os_error()
    );
}

/// [`Reloc::Strict`]: zeroed RDRAM at the first free [`alias_candidates`]
/// address (the only view), and the old window at [`RDRAM_BASE`] reserved
/// no-access, so a C site that still uses a window literal faults there
/// (the crash handler names it) instead of reading other memory. Returns
/// the RDRAM block. Panics if either mapping fails.
fn map_rdram_strict() -> usize {
    #[cfg(windows)]
    let (high, low) = {
        let alloc = |base: usize, ty: u32, prot: u32| {
            // SAFETY: plain Win32 allocation at a fixed address; checked.
            unsafe { sys::VirtualAlloc(base as *mut c_void, RDRAM_SIZE, ty, prot) as usize }
        };
        let rw = sys::MEM_RESERVE | sys::MEM_COMMIT;
        let high = alias_candidates().find(|&a| alloc(a, rw, sys::PAGE_READWRITE) == a);
        (high, reserve_noaccess(RDRAM_BASE, RDRAM_SIZE))
    };
    #[cfg(target_os = "linux")]
    let (high, low) = {
        let map = |base: usize, prot: i32| {
            // SAFETY: anonymous mapping, MAP_FIXED_NOREPLACE (never
            // clobbers; old kernels take it as a hint, hence the check).
            unsafe {
                let p = libc::mmap(
                    base as *mut c_void,
                    RDRAM_SIZE,
                    prot,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED_NOREPLACE,
                    -1,
                    0,
                );
                if p != libc::MAP_FAILED && p as usize != base {
                    libc::munmap(p, RDRAM_SIZE);
                }
                p as usize
            }
        };
        let high = alias_candidates().find(|&a| map(a, libc::PROT_READ | libc::PROT_WRITE) == a);
        (high, reserve_noaccess(RDRAM_BASE, RDRAM_SIZE))
    };
    assert!(
        low,
        "could not reserve the old RDRAM window at {RDRAM_BASE:#x}: {}",
        std::io::Error::last_os_error()
    );
    high.expect("RDRAM: no free high address")
}

/// Registers the vaddr regions for [`Reloc::Alias`] / [`Reloc::Strict`]
/// (no-op for [`Reloc::Off`]). Call once, after the game module is loaded
/// (`dylib`: strict mode needs its range) and before `os::boot`.
/// Alias: RDRAM only (stacks and the image pass through unregistered).
/// Strict: RDRAM, the coroutine stack window (high, [`init`]), plus
/// identity regions (`host == vbase`) for the image(s), then
/// `vaddr::set_strict`.
pub fn register_regions() {
    use pw64_platform::os::stack::{self, SEARCH_START, WINDOW_SIZE};
    use pw64_platform::vaddr::{self, Region};
    let mode = Reloc::get();
    if mode == Reloc::Off {
        return;
    }
    let mut regions = vec![Region {
        vbase: RDRAM_BASE as u32,
        size: RDRAM_SIZE as u32,
        host: rdram_host(),
    }];
    if mode == Reloc::Strict {
        let ident = |(start, end): (usize, usize)| {
            assert_low("identity vaddr region", start, end - start);
            Region {
                vbase: start as u32,
                size: (end - start) as u32,
                host: start,
            }
        };
        // Stacks: the high window `init` reserved (S6).
        regions.push(Region {
            vbase: SEARCH_START as u32,
            size: WINDOW_SIZE as u32,
            host: stack::window().0,
        });
        #[cfg(feature = "dylib")]
        regions.push(ident(
            module_range().expect("strict mode: register regions after dylib::load"),
        ));
        regions.push(ident(exe_range()));
    }
    vaddr::set_regions(regions);
    if mode == Reloc::Strict {
        vaddr::set_strict();
    }
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
    // The RDRAM view the vaddr layer translates to (alias mode: the high
    // one only; the HLE never sees low-view addresses there).
    let rdram = rdram_host();
    range_holds((rdram, rdram + RDRAM_SIZE), addr, len)
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
    // Strict mode moves the heap above 4 GB on purpose (high_heap.rs).
    #[cfg(windows)]
    {
        if Reloc::get() != Reloc::Strict {
            assert_low("host heap", heap, 8);
        }
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
    let mut reserved = format!("reserved: stacks {stacks}, module {module}");
    match Reloc::get() {
        Reloc::Off => {}
        Reloc::Alias => reserved += &format!(", RDRAM alias: high view at {:#x}", rdram_host()),
        Reloc::Strict => {
            // S6 audit: Rust heap blocks from now on lie above 4 GB, so one
            // that reaches PW64_U32 panics (high_heap.rs).
            match crate::high_heap::enable() {
                Some(a) => reserved += &format!(", Rust heap at {a:#x}"),
                None => reserved += ", Rust heap: no free high range",
            }
            let host = rdram_host();
            let stacks = pw64_platform::os::stack::window().0;
            reserved += &format!(
                ", RDRAM strict: at {host:#x}, stacks at {stacks:#x}, low windows no-access"
            )
        }
    }
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
    fn reloc_parse() {
        assert_eq!(Reloc::parse(None), Ok(Reloc::Off));
        assert_eq!(Reloc::parse(Some("off")), Ok(Reloc::Off));
        assert_eq!(Reloc::parse(Some(" alias ")), Ok(Reloc::Alias));
        assert_eq!(Reloc::parse(Some("strict")), Ok(Reloc::Strict));
        assert!(Reloc::parse(Some("on")).is_err());
        // A truncated high view must not land in the low window.
        assert!(alias_candidates().all(|a| a >> 32 != 0 && (a as u32) & 0x8000_0000 == 0));
    }

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
        assert!(hle_writable(rdram_host(), RDRAM_SIZE));
        assert!(!hle_writable(rdram_host() + RDRAM_SIZE - 4, 8));
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
