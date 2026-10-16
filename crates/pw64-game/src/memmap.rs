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
//!
//! `PW64_RELOC=on|alias|strict` (docs/notes/option-a.md) registers vaddr
//! regions instead: RDRAM, stacks and the images may then live anywhere.
//! The `reloc` feature builds an exe (and game module) without the fixed
//! bases; such a build has no fixed-window mode (see [`RELOC_BUILD`]).

use core::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};

/// N64 KSEG0 RDRAM window the C code addresses with literals.
pub const RDRAM_BASE: usize = 0x8000_0000;
/// 8 MB: Expansion Pak size (`osMemSize`, the texture heap runs up to it).
pub const RDRAM_SIZE: usize = 0x80_0000;
/// Everything the C code can see must lie below this.
pub const LIMIT_4GB: usize = 1 << 32;

/// Vaddr of the exe image (the fixed base of the default build).
pub const EXE_VBASE: u32 = 0xC000_0000;
/// Vaddr of the game module image (`dylib`; the fixed base of the default
/// build). Its region must end below [`EXE_VBASE`].
pub const MODULE_VBASE: u32 = 0xB000_0000;

/// `reloc` feature (option-a.md S7): the exe is linked without a fixed base
/// (Windows ASLR / Linux PIE) and the game module loads anywhere, so C globals
/// may lie above 4 GB and only work through the vaddr regions. Such a build
/// has no fixed-window mode: `PW64_RELOC` unset means `on`, `off` is refused.
/// `pw64_reloc` (build.rs) = the feature, or aarch64 (relocatable only).
pub const RELOC_BUILD: bool = cfg!(pw64_reloc);

/// Relocation mode, `PW64_RELOC` (docs/notes/option-a.md).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reloc {
    /// Unset / `off`: the fixed low window, no vaddr regions (identity).
    Off,
    /// `alias`: RDRAM is one shared-memory block seen twice, at
    /// [`RDRAM_BASE`] (for C sites that still use the window directly) and
    /// above 4 GB, registered as the vaddr region the C translates to.
    Alias,
    /// `on` (the `reloc` build's default, option-a.md S8): the production
    /// relocated mode. RDRAM and the stack window mapped once, above 4 GB;
    /// the image(s) are regions at their real host addresses; strict
    /// translation (`vaddr::set_strict`: a value that should be in a region
    /// and isn't panics, naming it). None of [`Reloc::Strict`]'s audit
    /// pieces: no no-access reservation of the old low windows (macOS can't
    /// map there), no high Rust heap (`high_heap` stays [`System`]).
    ///
    /// [`System`]: std::alloc::System
    On,
    /// `strict` (audit mode): [`Reloc::On`], plus the old RDRAM and stack
    /// windows reserved no-access (an untranslated literal faults at its C
    /// site) and the Rust heap moved above 4 GB (`high_heap`, so a Rust heap
    /// pointer reaching `PW64_U32` panics on Windows too).
    Strict,
}

impl Reloc {
    /// RDRAM and the stacks live above 4 GB, translated strictly
    /// ([`Reloc::On`] or [`Reloc::Strict`]).
    pub fn relocated(self) -> bool {
        matches!(self, Reloc::On | Reloc::Strict)
    }
}

impl Reloc {
    /// Parses a `PW64_RELOC` value for this build ([`RELOC_BUILD`]). Pure.
    pub fn parse(v: Option<&str>) -> Result<Reloc, String> {
        Self::parse_for(v, RELOC_BUILD)
    }

    /// [`Self::parse`] for a fixed-base (`reloc_build` false) or a `reloc`
    /// build: the latter defaults to `on` and has no `off`. Pure.
    pub fn parse_for(v: Option<&str>, reloc_build: bool) -> Result<Reloc, String> {
        match v.map(str::trim) {
            None | Some("") if reloc_build => Ok(Reloc::On),
            Some("0" | "off") if reloc_build => Err(
                "PW64_RELOC=off: this is a relocatable build (feature `reloc`), which \
                 has no fixed-window mode; use on, alias or strict"
                    .to_string(),
            ),
            None | Some("" | "0" | "off") => Ok(Reloc::Off),
            Some("1" | "on") => Ok(Reloc::On),
            Some("alias") => Ok(Reloc::Alias),
            Some("strict") => Ok(Reloc::Strict),
            Some(o) => Err(format!("PW64_RELOC={o}: expected off, on, alias or strict")),
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
#[inline]
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
/// [`Reloc::On`] / [`Reloc::Strict`]: once, high ([`map_rdram_high`]), and
/// the stack window high too ([`relocate_stacks`]); strict also reserves
/// the old windows no-access. Either way the vaddr regions are registered
/// later, by [`register_regions`].
pub fn map_rdram_window() {
    match Reloc::get() {
        Reloc::Off => {}
        Reloc::Alias => {
            RDRAM_HOST.store(map_rdram_alias(), Ordering::Relaxed);
            return;
        }
        mode @ (Reloc::On | Reloc::Strict) => {
            let audit = mode == Reloc::Strict;
            RDRAM_HOST.store(map_rdram_high(audit), Ordering::Relaxed);
            relocate_stacks(audit);
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
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // MAP_FIXED_NOREPLACE: fail (EEXIST) rather than clobber a mapping;
        // pre-4.17 kernels take it as a hint, hence the address check.
        // SAFETY: anonymous mapping at a fixed address; failure is checked.
        let p = unsafe {
            libc::mmap(
                RDRAM_BASE as *mut c_void,
                RDRAM_SIZE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | pw64_platform::MAP_FIXED_NOREPLACE,
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
    #[cfg(not(any(windows, target_os = "linux", target_os = "android")))]
    compile_error!("RDRAM window mapping is implemented for Windows, Linux and Android only");
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
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let high = {
        // SAFETY: memfd (zero-filled) mapped shared at fixed addresses with
        // MAP_FIXED_NOREPLACE (never clobbers; pre-4.17 kernels take it as
        // a hint, hence the address checks); every result is checked. The
        // fd stays open (the mappings keep the file alive anyway).
        unsafe {
            // Bionic's memfd_create wrapper needs API 30; the syscall works
            // on every kernel Android 8 ships (3.17+).
            #[cfg(target_os = "android")]
            let fd = libc::syscall(
                libc::SYS_memfd_create,
                c"pw64-rdram".as_ptr(),
                libc::MFD_CLOEXEC,
            ) as libc::c_int;
            #[cfg(target_os = "linux")]
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
                    libc::MAP_SHARED | pw64_platform::MAP_FIXED_NOREPLACE,
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

/// High candidates for the stack window in [`Reloc::On`]/[`Reloc::Strict`]: like
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
    #[cfg(any(target_os = "linux", target_os = "android"))]
    // SAFETY: anonymous PROT_NONE mapping, MAP_FIXED_NOREPLACE (never
    // clobbers; old kernels take it as a hint, hence the check).
    unsafe {
        let p = libc::mmap(
            base as *mut c_void,
            len,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE | pw64_platform::MAP_FIXED_NOREPLACE,
            -1,
            0,
        );
        if p != libc::MAP_FAILED && p as usize != base {
            libc::munmap(p, len);
        }
        p as usize == base
    }
}

/// [`Reloc::On`]/[`Reloc::Strict`] (option-a.md S6): the coroutine stack
/// window moves to the first free [`stack_candidates`] address
/// (`register_regions` maps vaddrs 0x80800000.. there). `audit` (strict):
/// the old window is reserved no-access too, so a stack address that
/// skipped the vaddr layer faults. Before any stack exists. Panics if
/// either fails.
fn relocate_stacks(audit: bool) {
    use pw64_platform::os::stack::{self, SEARCH_START, WINDOW_SIZE};
    stack_candidates()
        .find(|&a| stack::reserve_region_at(a))
        .expect("stack window: no free high address");
    assert!(
        !audit || reserve_noaccess(SEARCH_START, WINDOW_SIZE),
        "could not reserve the old stack window at {SEARCH_START:#x}: {}",
        std::io::Error::last_os_error()
    );
}

/// [`Reloc::On`]/[`Reloc::Strict`]: zeroed RDRAM at the first free
/// [`alias_candidates`] address (the only view). `audit` (strict): the old
/// window at [`RDRAM_BASE`] is reserved no-access too, so a C site that
/// still uses a window literal faults there (the crash handler names it)
/// instead of reading other memory. Returns the RDRAM block. Panics if a
/// mapping fails.
fn map_rdram_high(audit: bool) -> usize {
    #[cfg(windows)]
    let (high, low) = {
        let alloc = |base: usize, ty: u32, prot: u32| {
            // SAFETY: plain Win32 allocation at a fixed address; checked.
            unsafe { sys::VirtualAlloc(base as *mut c_void, RDRAM_SIZE, ty, prot) as usize }
        };
        let rw = sys::MEM_RESERVE | sys::MEM_COMMIT;
        let high = alias_candidates().find(|&a| alloc(a, rw, sys::PAGE_READWRITE) == a);
        (high, !audit || reserve_noaccess(RDRAM_BASE, RDRAM_SIZE))
    };
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let (high, low) = {
        let map = |base: usize, prot: i32| {
            // SAFETY: anonymous mapping, MAP_FIXED_NOREPLACE (never
            // clobbers; old kernels take it as a hint, hence the check).
            unsafe {
                let p = libc::mmap(
                    base as *mut c_void,
                    RDRAM_SIZE,
                    prot,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | pw64_platform::MAP_FIXED_NOREPLACE,
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
        (high, !audit || reserve_noaccess(RDRAM_BASE, RDRAM_SIZE))
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
/// Both: RDRAM, plus the image(s) at their fixed vaddrs ([`image_regions`]:
/// the exe at [`EXE_VBASE`], the module at [`MODULE_VBASE`], wherever they
/// were loaded; identity in the fixed-base build).
/// Alias: the stacks pass through unregistered (they stay at their vaddrs).
/// On / strict: also the coroutine stack window (high, [`init`]), then
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
    if mode.relocated() {
        // Stacks: the high window `init` reserved (S6).
        regions.push(Region {
            vbase: SEARCH_START as u32,
            size: WINDOW_SIZE as u32,
            host: stack::window().0,
        });
    }
    #[cfg(feature = "dylib")]
    let module = Some(module_range().expect("PW64_RELOC: register regions after dylib::load"));
    #[cfg(not(feature = "dylib"))]
    let module = None;
    regions.extend(image_regions(exe_range(), module));
    vaddr::set_regions(regions);
    if mode.relocated() {
        vaddr::set_strict();
    }
}

/// The vaddr regions of the exe image `exe` (at [`EXE_VBASE`]) and the game
/// module image `module` (at [`MODULE_VBASE`]), `[start, end)` host ranges
/// loaded anywhere (option-a.md S7). Panics if an image does not fit its
/// vaddr range (exe: up to 4 GB, module: up to [`EXE_VBASE`]). Pure.
pub fn image_regions(
    exe: (usize, usize),
    module: Option<(usize, usize)>,
) -> Vec<pw64_platform::vaddr::Region> {
    let region = |what: &str, (start, end): (usize, usize), vbase: u32, limit: u64| {
        let size = end - start;
        assert!(
            vbase as u64 + size as u64 <= limit,
            "{what} image {start:#x}..{end:#x} does not fit its vaddr range \
             {vbase:#x}..{limit:#x}"
        );
        pw64_platform::vaddr::Region {
            vbase,
            size: size as u32,
            host: start,
        }
    };
    let mut v: Vec<_> = module
        .map(|m| region("game module", m, MODULE_VBASE, EXE_VBASE as u64))
        .into_iter()
        .collect();
    v.push(region("exe", exe, EXE_VBASE, LIMIT_4GB as u64));
    v
}

/// `(start, end)` of the app `.so`'s loaded image (no `__executable_start`
/// on Android; the crash tracer finds it with `dl_iterate_phdr`).
#[cfg(target_os = "android")]
fn exe_image_range() -> (usize, usize) {
    pw64_platform::os::crash::exe_image_range()
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
/// image (`static`: all C globals), plus (`dylib`) the game module (below it
/// at 0xB0000000 in the fixed-base build; anywhere with `reloc`). Only
/// [`check_low_4gb`] uses the span; the gap between the two holds nothing
/// C-visible.
pub fn image_range() -> (usize, usize) {
    let (start, end) = exe_image_range();
    #[cfg(feature = "dylib")]
    let (start, end) = {
        let m = crate::dylib::game().range;
        (start.min(m.0), end.max(m.1))
    };
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
#[inline]
pub fn range_holds((start, end): (usize, usize), addr: usize, len: usize) -> bool {
    addr >= start && addr.checked_add(len).is_some_and(|e| e <= end)
}

/// The RSP HLE (display lists, audio command lists) may read
/// `[addr, addr + len)`: the RDRAM window, a live thread stack's usable
/// range, or the exe / game module image (static display lists in .data).
/// Not the reserved-but-uncommitted rest of the stack window, the stack
/// guard pages, or the gap between the module and the exe. Cheap (per
/// command): no locks, no allocation. Inline, so the common RDRAM case
/// stays inline in the HLE's per-word reads (option-a.md "Performance").
#[inline]
pub fn hle_readable(addr: usize, len: usize) -> bool {
    in_rdram(addr, len) || readable_outside_rdram(addr, len)
}

/// [`hle_readable`] outside RDRAM (out of line: rare).
#[inline(never)]
fn readable_outside_rdram(addr: usize, len: usize) -> bool {
    pw64_platform::os::stack::in_live_stack(addr, len)
        || range_holds(exe_range(), addr, len)
        || module_range().is_some_and(|r| range_holds(r, addr, len))
}

/// The RSP HLE may write `[addr, addr + len)`: only the RDRAM window and live
/// thread stacks, never code or the images (audio HLE DMA writes).
#[inline]
pub fn hle_writable(addr: usize, len: usize) -> bool {
    in_rdram(addr, len) || pw64_platform::os::stack::in_live_stack(addr, len)
}

/// `[addr, addr + len)` lies in the RDRAM view the vaddr layer translates to
/// (alias mode: the high one only; the HLE never sees low-view addresses
/// there).
#[inline]
fn in_rdram(addr: usize, len: usize) -> bool {
    let rdram = rdram_host();
    range_holds((rdram, rdram + RDRAM_SIZE), addr, len)
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
/// of the addresses. With `PW64_RELOC` set the image checks are skipped: the
/// images are vaddr regions at [`EXE_VBASE`]/[`MODULE_VBASE`] wherever they
/// load ([`register_regions`]; `set_regions` checks them).
pub fn check_low_4gb() -> String {
    let fixed = Reloc::get() == Reloc::Off;
    let (img0, img1) = image_range();
    if fixed {
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
    }
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
    // Strict mode moves the heap above 4 GB on purpose (high_heap.rs), and
    // a `reloc` exe (high-entropy ASLR) may get both high.
    #[cfg(windows)]
    if fixed {
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
    // `reloc`: the module has no fixed range (dylib.rs).
    #[cfg(all(feature = "dylib", not(pw64_reloc)))]
    let module = crate::dylib::reserve_module_range();
    #[cfg(not(all(feature = "dylib", not(pw64_reloc))))]
    let module = false;
    let mut reserved = format!("reserved: stacks {stacks}, module {module}");
    match Reloc::get() {
        Reloc::Off => {}
        Reloc::Alias => reserved += &format!(", RDRAM alias: high view at {:#x}", rdram_host()),
        Reloc::On => {
            let stacks = pw64_platform::os::stack::window().0;
            reserved += &format!(", relocated: RDRAM at {:#x}, stacks at {stacks:#x}", rdram_host())
        }
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
        let p = |v| Reloc::parse_for(v, false);
        assert_eq!(p(None), Ok(Reloc::Off));
        assert_eq!(p(Some("off")), Ok(Reloc::Off));
        assert_eq!(p(Some(" alias ")), Ok(Reloc::Alias));
        assert_eq!(p(Some("strict")), Ok(Reloc::Strict));
        assert!(p(Some("yes")).is_err());
        assert_eq!(p(Some("on")), Ok(Reloc::On));
        // A `reloc` build: production relocated mode by default, no fixed window.
        let r = |v| Reloc::parse_for(v, true);
        assert_eq!(r(None), Ok(Reloc::On));
        assert_eq!(r(Some("")), Ok(Reloc::On));
        assert_eq!(r(Some("strict")), Ok(Reloc::Strict));
        assert_eq!(r(Some("alias")), Ok(Reloc::Alias));
        assert!(Reloc::On.relocated() && Reloc::Strict.relocated());
        assert!(!Reloc::Off.relocated() && !Reloc::Alias.relocated());
        assert!(r(Some("off")).is_err() && r(Some("0")).is_err());
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

    /// Images map to their fixed vaddrs wherever they load; one that would
    /// overrun its vaddr range is refused.
    #[test]
    fn image_regions_fixed_vbases() {
        let exe = (0x7FF6_1234_0000, 0x7FF6_1534_0000);
        let m = (0x7FFA_0000_0000, 0x7FFA_0200_0000);
        let r = image_regions(exe, Some(m));
        let parts = |i: usize| (r[i].vbase, r[i].size, r[i].host);
        assert_eq!(parts(0), (MODULE_VBASE, 0x200_0000, m.0));
        assert_eq!(parts(1), (EXE_VBASE, 0x300_0000, exe.0));
        // The fixed-base build: identity.
        let r = image_regions((0xC000_0000, 0xC100_0000), None);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].host, r[0].vbase as usize);
        let big = std::panic::catch_unwind(|| image_regions(exe, Some((0, 0x1000_0001))));
        assert!(big.is_err());
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
