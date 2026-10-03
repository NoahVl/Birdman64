//! `dylib` feature: the game C lives in a module built on the player's machine
//! (docs/notes/first-run-build.md), loaded here at a fixed base below 4 GB.
//!
//! The module (`pw64game.dll` / `libpw64game.so`) has no imports from the
//! exe: its calls into Rust go through jump slots that [`load`] fills via
//! `pw64_dll_bind` (dylib/pw64_dll_shim.c), from the build.rs-generated
//! [`IMPORTS`] table. The exe gets the few C symbols it needs by name.

use core::ffi::{CStr, c_char, c_int, c_void};
use std::path::Path;
use std::sync::OnceLock;

include!(concat!(env!("OUT_DIR"), "/dll_imports.rs"));

/// Where the module is linked (`/BASE /FIXED`, lld `--image-base`): below the
/// exe (0xC0000000), above the coroutine stacks (end 0xA0000000), bit 31 set.
pub const DLL_BASE: usize = 0xB000_0000;
/// End of the range [`reserve_module_range`] keeps free (the exe's base).
const DLL_RESERVE_END: usize = 0xC000_0000;

/// Windows: reserves `DLL_BASE..0xC0000000` (no commit) at startup, before
/// the window and the GPU driver exist, so no driver allocation can take
/// the module's fixed range by the time it loads (after the ROM step and
/// possibly a build); [`load`] releases it just before `LoadLibraryW`.
/// False if the range is already taken (the load then reports it).
/// Linux: no-op (unhinted mmaps go top-down from high addresses).
pub fn reserve_module_range() -> bool {
    sys::reserve(DLL_BASE, DLL_RESERVE_END - DLL_BASE)
}

/// Must equal the module's `pw64_dll_abi()` (the builder passes it as
/// `-DPW64_DLL_ABI`): the pw64-game package version plus a hash of the
/// import list, the shim and the embedded build kit (T10, build.rs
/// `dylib_abi`). A module built for another ABI is refused and rebuilt.
pub const DLL_ABI: &str = env!("DLL_ABI");

/// The loaded module's entry points.
pub struct Game {
    pub bootproc: unsafe extern "C-unwind" fn(*mut c_void),
    pub widescreen_aspect: *mut f32,
    /// `pw64_fill_screen` (pw64_widescreen.c): fill screen flag.
    pub fill_screen: *mut i32,
    /// `D_802B892C` (memory.c): the sample global memmap checks.
    pub sample_global: *const u32,
    /// `[start, end)` of the module image.
    pub range: (usize, usize),
}

// SAFETY: plain addresses into the module, which is never unloaded.
unsafe impl Send for Game {}
unsafe impl Sync for Game {}

static GAME: OnceLock<Game> = OnceLock::new();

/// The loaded module. Panics before [`load`].
pub fn game() -> &'static Game {
    GAME.get()
        .expect("game module not loaded (pw64_game::dylib::load)")
}

/// `[start, end)` of the module image once loaded, else `None` (no panic).
pub fn loaded_range() -> Option<(usize, usize)> {
    GAME.get().map(|g| g.range)
}

/// Calls the module's `bootproc` (the fn `pw64` hands to `os::boot`).
///
/// # Safety
/// Same contract as the C `bootproc`: once, on the OS core's boot thread.
pub unsafe extern "C-unwind" fn bootproc(arg: *mut c_void) {
    // SAFETY: forwarded; the pointer came from the loaded module.
    unsafe { (game().bootproc)(arg) }
}

/// `pw64_dll_bind` callback: name → Rust function address, null if unknown.
extern "C" fn lookup(name: *const c_char) -> *const c_void {
    // SAFETY: the module passes its own NUL-terminated string constants.
    let name = unsafe { CStr::from_ptr(name) };
    IMPORTS
        .iter()
        .find(|(n, _)| *n == name)
        .map_or(core::ptr::null(), |(_, f)| f.0)
}

/// Loads the module at [`DLL_BASE`], checks its ABI string, binds its imports
/// and resolves the entry points. Errors are player-facing sentences.
pub fn load(path: &Path) -> Result<(), String> {
    if GAME.get().is_some() {
        return Ok(());
    }
    let h = sys::open(path)?;
    let r = bind_module(h, path);
    if r.is_err() {
        // Unmap a refused module: it sits at the fixed base, so a rebuilt
        // module (Retry on the setup screen) could not load while it stays
        // mapped, and Windows could not delete its file for the rebuild.
        sys::close(h);
    }
    r
}

/// [`load`] after `open`: ABI check, range check, import binding, entry
/// points. Nothing of the module has run yet when this fails.
fn bind_module(h: *mut c_void, path: &Path) -> Result<(), String> {
    let sym = |name: &CStr| -> Result<*mut c_void, String> {
        let p = sys::symbol(h, name);
        if p.is_null() {
            Err(format!(
                "{}: missing symbol {}; delete it to rebuild the game",
                path.display(),
                name.to_string_lossy()
            ))
        } else {
            Ok(p)
        }
    };
    // SAFETY: the symbols are the shim's functions with these signatures
    // (dylib/pw64_dll_shim.c); addresses are checked non-null above.
    unsafe {
        let abi: extern "C" fn() -> *const c_char = core::mem::transmute(sym(c"pw64_dll_abi")?);
        let abi = CStr::from_ptr(abi()).to_string_lossy().into_owned();
        if abi != DLL_ABI {
            return Err(format!(
                "{} was built for another version ({abi}, need {DLL_ABI}); delete it to rebuild",
                path.display()
            ));
        }
        let range_fn: extern "C" fn(*mut *const c_void, *mut *const c_void) =
            core::mem::transmute(sym(c"pw64_dll_range")?);
        let (mut start, mut end) = (core::ptr::null(), core::ptr::null());
        range_fn(&mut start, &mut end);
        let range = (start as usize, end as usize);
        if range.0 != DLL_BASE {
            return Err(format!(
                "{} loaded at {:#x}, not {DLL_BASE:#x} (address range taken)",
                path.display(),
                range.0
            ));
        }
        let bind: extern "C" fn(extern "C" fn(*const c_char) -> *const c_void) -> c_int =
            core::mem::transmute(sym(c"pw64_dll_bind")?);
        let missing = bind(lookup);
        if missing != 0 {
            return Err(format!(
                "{}: import #{missing} unknown to this exe; delete it to rebuild",
                path.display()
            ));
        }
        let game = Game {
            bootproc: core::mem::transmute::<*mut c_void, unsafe extern "C-unwind" fn(*mut c_void)>(
                sym(c"bootproc")?,
            ),
            widescreen_aspect: sym(c"pw64_widescreen_aspect")?.cast(),
            fill_screen: sym(c"pw64_fill_screen")?.cast(),
            sample_global: sym(c"D_802B892C")?.cast(),
            range,
        };
        let _ = GAME.set(game);
        // Crash symbolization (first-run-build.md T14): the tracer prints
        // RIPs inside the module as `pw64game+0x<off>` and logs where its
        // `.map` is. Both spike_build.sh and the builder's cache layout
        // write that map next to the module as `pw64game.map`.
        pw64_platform::os::crash::set_game_module(
            range,
            path.parent().unwrap_or(path).join("pw64game.map"),
        );
    }
    Ok(())
}

#[cfg(windows)]
mod sys {
    use core::ffi::{CStr, c_char, c_void};
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};

    unsafe extern "system" {
        fn LoadLibraryW(name: *const u16) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const c_char) -> *mut c_void;
        fn FreeLibrary(module: *mut c_void) -> i32;
        fn VirtualAlloc(addr: *mut c_void, size: usize, ty: u32, prot: u32) -> *mut c_void;
        fn VirtualFree(addr: *mut c_void, size: usize, ty: u32) -> i32;
    }

    /// Start of the reservation `reserve` made (0: none). Released once.
    static RESERVED: AtomicUsize = AtomicUsize::new(0);

    /// MEM_RESERVE of `[addr, addr + size)`; true if we hold it now.
    pub fn reserve(addr: usize, size: usize) -> bool {
        const MEM_RESERVE: u32 = 0x2000;
        const PAGE_NOACCESS: u32 = 0x01;
        if RESERVED.load(Ordering::Acquire) != 0 {
            return true;
        }
        // SAFETY: plain reservation at a fixed address; failure is checked.
        let p = unsafe { VirtualAlloc(addr as *mut c_void, size, MEM_RESERVE, PAGE_NOACCESS) };
        if p as usize != addr {
            return false;
        }
        RESERVED.store(addr, Ordering::Release);
        true
    }

    /// Frees the `reserve` range for the loader (no-op without one).
    fn release() {
        const MEM_RELEASE: u32 = 0x8000;
        let addr = RESERVED.swap(0, Ordering::AcqRel);
        if addr != 0 {
            // SAFETY: the reservation made in `reserve`; nothing is in it.
            unsafe { VirtualFree(addr as *mut c_void, 0, MEM_RELEASE) };
        }
    }

    /// Unloads a module that was refused (no code of it ran; no DllMain).
    pub fn close(h: *mut c_void) {
        // SAFETY: handle from `open`, nothing references the module.
        unsafe { FreeLibrary(h) };
    }

    /// `LoadLibraryW` (UTF-16 path: non-ASCII user dirs are fine). The module
    /// is `/FIXED` (no relocations): if its range is taken, loading fails.
    pub fn open(path: &Path) -> Result<*mut c_void, String> {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        // The module's range was held since startup; hand it to the loader.
        release();
        // SAFETY: NUL-terminated UTF-16 path.
        let h = unsafe { LoadLibraryW(wide.as_ptr()) };
        if h.is_null() {
            return Err(format!(
                "could not load {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            ));
        }
        Ok(h)
    }

    pub fn symbol(h: *mut c_void, name: &CStr) -> *mut c_void {
        // SAFETY: valid module handle and NUL-terminated name.
        unsafe { GetProcAddress(h, name.as_ptr()) }
    }
}

#[cfg(target_os = "linux")]
mod sys {
    use core::ffi::{CStr, c_void};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    /// No reservation on Linux (see `reserve_module_range`).
    pub fn reserve(_addr: usize, _size: usize) -> bool {
        false
    }

    /// `dlopen` (RTLD_NOW: every import resolved up front). ld.so maps an
    /// ET_DYN at its linked address when that range is free (glibc passes
    /// the first PT_LOAD vaddr as the mmap hint); `load` checks it landed.
    pub fn open(path: &Path) -> Result<*mut c_void, String> {
        let c = std::ffi::CString::new(path.as_os_str().as_bytes())
            .map_err(|_| format!("bad path {}", path.display()))?;
        // SAFETY: NUL-terminated path.
        let h = unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if h.is_null() {
            // SAFETY: dlerror returns a NUL-terminated string after a failure.
            let err = unsafe { CStr::from_ptr(libc::dlerror()) };
            return Err(format!(
                "could not load {}: {}",
                path.display(),
                err.to_string_lossy()
            ));
        }
        Ok(h)
    }

    pub fn symbol(h: *mut c_void, name: &CStr) -> *mut c_void {
        // SAFETY: valid handle and NUL-terminated name.
        unsafe { libc::dlsym(h, name.as_ptr()) }
    }

    /// Unloads a module that was refused (no code of it ran).
    pub fn close(h: *mut c_void) {
        // SAFETY: handle from `open`, nothing references the module.
        unsafe { libc::dlclose(h) };
    }
}

#[cfg(test)]
mod tests {
    /// With `PW64_GAME_DLL` set (a module from dylib/spike_build.sh or the
    /// builder): load it, boot the real C with no ROM, and require the
    /// "ROM not loaded" panic to unwind from Rust through the module's C
    /// frames (jump-slot thunks, the module's own unwind tables) and out of
    /// `os::run` — the dylib twin of lib.rs `boot_without_rom_unwinds_through_c`.
    /// Skipped (passes) without the env var: CI has no module yet.
    #[test]
    fn module_boots_and_unwinds_without_rom() {
        let Some(path) = std::env::var_os("PW64_GAME_DLL") else {
            eprintln!("PW64_GAME_DLL not set: skipped");
            return;
        };
        use pw64_platform::os;
        crate::memmap::map_rdram_window();
        super::load(std::path::Path::new(&path)).unwrap();
        eprintln!("{}", crate::memmap::check_low_4gb());
        os::boot(super::bootproc, core::ptr::null_mut());
        let err = std::panic::catch_unwind(|| {
            os::run(&os::RunConfig {
                max_retraces: Some(600),
                throttle: false,
            })
        })
        .expect_err("the C can't get far without a ROM");
        let msg = err
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| err.downcast_ref::<&str>().copied())
            .unwrap_or("");
        assert!(msg.contains("ROM not loaded"), "unexpected panic: {msg}");
    }
}
