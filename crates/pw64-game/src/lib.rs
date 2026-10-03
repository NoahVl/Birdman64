//! The decomp's C game code, compiled natively by `build.rs`.
//!
//! This crate only links the C objects (whole-archive) and declares the few C
//! entry points Rust calls. Every symbol the C code imports (libultra, linker
//! segment symbols, asm-only helpers) is provided by `pw64-platform`.

#[cfg(feature = "static")]
use core::ffi::c_void;

// Links the platform symbols the C objects import.
use pw64_platform as _;
// Links the Rust ports the C calls (ported.txt; rustc drops an unreferenced
// dependency from the link line, hence the anonymous use).
#[cfg(feature = "static")]
use pw64_kernel as _;

#[cfg(feature = "dylib")]
pub mod dylib;
pub mod memmap;

#[cfg(feature = "dylib")]
pub use dylib::bootproc;

// "C-unwind": platform stubs panic, and the panic unwinds through the C frames
// (x64 Windows has table-based unwinding for them) back into Rust.
#[cfg(feature = "static")]
unsafe extern "C-unwind" {
    /// The N64 boot entry (`bootproc`, system.c): clears the framebuffers,
    /// calls `osInitialize`, then creates and starts the Kernel thread.
    pub fn bootproc(arg: *mut c_void);
}

#[cfg(feature = "static")]
unsafe extern "C" {
    /// native/src/pw64_widescreen.c: output aspect, 0 = 4:3.
    static mut pw64_widescreen_aspect: f32;
    /// native/src/pw64_widescreen.c: fill screen, 0 = off.
    static mut pw64_fill_screen: i32;
}

/// Hor+ widescreen for the C side: widens the CPU culling frustum of the
/// main view and drops the flight view's side border bars
/// (renderer.md "Widescreen"). `aspect` = w/h, 0 = off. Call before
/// `bootproc`, on the thread that runs the game.
pub fn set_widescreen_aspect(aspect: f32) {
    // SAFETY: plain C global; the game (the only reader) runs on this
    // thread and hasn't started yet.
    #[cfg(feature = "static")]
    unsafe {
        pw64_widescreen_aspect = aspect
    };
    // SAFETY: same global, exported by the loaded game module.
    #[cfg(feature = "dylib")]
    unsafe {
        *dylib::game().widescreen_aspect = aspect
    };
}

/// Fill screen for the C side: world views draw without their letterbox
/// bars and cull a taller frustum (renderer.md "Fill screen"). Call before
/// `bootproc`, on the thread that runs the game.
pub fn set_fill_screen(fill: bool) {
    // SAFETY: plain C global; the game (the only reader) runs on this
    // thread and hasn't started yet.
    #[cfg(feature = "static")]
    unsafe {
        pw64_fill_screen = fill as i32
    };
    // SAFETY: same global, exported by the loaded game module.
    #[cfg(feature = "dylib")]
    unsafe {
        *dylib::game().fill_screen = fill as i32
    };
}

#[cfg(all(test, feature = "static"))]
mod tests {
    /// Linking this test binary proves every C import resolves.
    #[test]
    fn c_links() {
        let entry = super::bootproc as unsafe extern "C-unwind" fn(_);
        assert_ne!(entry as usize, 0);
    }

    /// `memmap::init` maps the window at a fixed address: once per process.
    fn init_once() {
        static INIT: std::sync::Once = std::sync::Once::new();
        INIT.call_once(|| println!("{}", super::memmap::init()));
    }

    /// The test exe gets the same low fixed base as `pw64` (build.rs), so the
    /// full address-space setup must hold here too.
    #[test]
    fn memmap_low_4gb() {
        init_once();
        // The window is usable.
        let p = super::memmap::RDRAM_BASE as *mut u32;
        unsafe {
            p.write_volatile(0x1234_5678);
            assert_eq!(p.read_volatile(), 0x1234_5678);
        }
    }

    /// Boots the real C with no ROM loaded: `bootproc` and the threads it
    /// starts run on the low coroutine stacks until the first cartridge DMA,
    /// whose Rust panic ("ROM not loaded") must unwind through the C frames
    /// and out of `os::run` (C-unwind + the C's unwind tables, both targets).
    #[test]
    fn boot_without_rom_unwinds_through_c() {
        use pw64_platform::os;
        init_once();
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
