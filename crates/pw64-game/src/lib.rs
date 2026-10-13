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
    /// native/src/pw64_split.c: split-screen layout, 0 = off.
    static mut pw64_split: i32;
    /// native/src/pw64_collide.c: split-screen pilot collisions, 0 = off.
    static mut pw64_split_collide: i32;
    /// native/src/pw64_split.c: split-screen layout, 0 = top/bottom, 1 =
    /// left/right.
    static mut pw64_split_layout: i32;
    /// native/src/pw64_split.c: share the best split-screen score, 0 = off.
    static mut pw64_split_share: i32;
    /// native/src/pw64_split.c: pending live split-screen switch (0/1, -1 =
    /// none); the game applies and clears it in a menu state.
    static mut pw64_split_req: i32;
    /// native/src/pw64_split.c: 1 = player 2's controller isn't connected
    /// (the main menu shows a hint).
    static mut pw64_split_hint: i32;
    /// native/src/pw64_visibility.c: most detailed model LOD, 0 = off.
    static mut pw64_infinite_lod: i32;
    /// native/src/pw64_visibility.c: whole-island view distance, 0 = off.
    static mut pw64_infinite_view_distance: i32;
}

/// Infinite LOD (models always at their most detailed level) and infinite
/// view distance (far plane covering the whole island, no distance haze)
/// for the C side (pw64_visibility.c). Call before `bootproc`, on the
/// thread that runs the game.
pub fn set_visibility(infinite_lod: bool, infinite_view_distance: bool) {
    // SAFETY: plain C globals; the game (the only reader) runs on this
    // thread and hasn't started yet.
    #[cfg(feature = "static")]
    unsafe {
        pw64_infinite_lod = infinite_lod as i32;
        pw64_infinite_view_distance = infinite_view_distance as i32;
    }
    // SAFETY: same globals, exported by the loaded game module.
    #[cfg(feature = "dylib")]
    unsafe {
        *dylib::game().infinite_lod = infinite_lod as i32;
        *dylib::game().infinite_view_distance = infinite_view_distance as i32;
    }
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

/// Split-screen for the C side (splitscreen.md): 0 = off, 1 = on. On means
/// two players (`pw64_players = 2`, set from this in gameInit): supported
/// vehicles run a second pilot on controller 2 in the second view (layout:
/// [`set_split_layout`]). Call before `bootproc`, on the thread that runs
/// the game.
pub fn set_split(layout: u8) {
    // SAFETY: plain C global; the game (the only reader) runs on this
    // thread and hasn't started yet.
    #[cfg(feature = "static")]
    unsafe {
        pw64_split = layout as i32
    };
    // SAFETY: same global, exported by the loaded game module.
    #[cfg(feature = "dylib")]
    unsafe {
        *dylib::game().split = layout as i32
    };
}

/// Whether split-screen is on in the game right now (`pw64_split`): the
/// boot value from [`set_split`], or a live switch by the main menu's
/// Players item or a [`request_split`] the game has applied.
pub fn split_on() -> bool {
    // SAFETY: plain C global, read volatile (the game thread writes it).
    #[cfg(feature = "static")]
    let v = unsafe { core::ptr::addr_of!(pw64_split).read_volatile() };
    // SAFETY: same global, exported by the loaded game module.
    #[cfg(feature = "dylib")]
    let v = unsafe { dylib::game().split.read_volatile() };
    v != 0
}

/// Asks the game to switch split-screen on/off live (splitscreen.md "Live
/// switch"): it applies the request in its next menu state that holds no
/// test (title, file/class/test menus) and then [`split_on`] follows. May be
/// called any time (the settings row).
pub fn request_split(on: bool) {
    // SAFETY: plain C global; the game reads and clears it on its thread.
    #[cfg(feature = "static")]
    unsafe {
        core::ptr::addr_of_mut!(pw64_split_req).write_volatile(on as i32)
    };
    // SAFETY: same global, exported by the loaded game module.
    #[cfg(feature = "dylib")]
    unsafe {
        dylib::game().split_req.write_volatile(on as i32)
    };
}

/// The [`request_split`] the game hasn't applied yet (`None`: nothing
/// pending).
pub fn split_req_pending() -> Option<bool> {
    // SAFETY: plain C global, read volatile (the game thread clears it).
    #[cfg(feature = "static")]
    let v = unsafe { core::ptr::addr_of!(pw64_split_req).read_volatile() };
    // SAFETY: same global, exported by the loaded game module.
    #[cfg(feature = "dylib")]
    let v = unsafe { dylib::game().split_req.read_volatile() };
    (v >= 0).then_some(v != 0)
}

/// Main-menu hint for the C side: true = player 2's controller isn't
/// connected ("Connect a second controller" under the menu while
/// split-screen is on). May be called any time.
pub fn set_split_hint(missing: bool) {
    // SAFETY: plain C flag; the game reads it on its own thread.
    #[cfg(feature = "static")]
    unsafe {
        pw64_split_hint = missing as i32
    };
    // SAFETY: same global, exported by the loaded game module.
    #[cfg(feature = "dylib")]
    unsafe {
        *dylib::game().split_hint = missing as i32
    };
}

/// Split-screen pilot collisions for the C side (splitscreen.md
/// "Interactions toggle"): on = the two pilots can crash into each other.
/// The game reads the flag at each test start, so unlike [`set_split`]
/// this may be called any time (also live from the settings screen).
pub fn set_split_collide(on: bool) {
    // SAFETY: plain C flag; the game reads it on its own thread.
    #[cfg(feature = "static")]
    unsafe {
        pw64_split_collide = on as i32
    };
    // SAFETY: same global, exported by the loaded game module.
    #[cfg(feature = "dylib")]
    unsafe {
        *dylib::game().split_collide = on as i32
    };
}

/// Split-screen view layout for the C side (splitscreen.md "Viewports"):
/// false = top/bottom, true = left/right (side by side). The game latches
/// it at each test start, so like [`set_split_collide`] this may be called
/// any time (also live from the settings screen).
pub fn set_split_layout(left_right: bool) {
    // SAFETY: plain C flag; the game reads it on its own thread.
    #[cfg(feature = "static")]
    unsafe {
        pw64_split_layout = left_right as i32
    };
    // SAFETY: same global, exported by the loaded game module.
    #[cfg(feature = "dylib")]
    unsafe {
        *dylib::game().split_layout = left_right as i32
    };
}

/// Split-screen "Share best score" for the C side: on = when player 2's run
/// beat player 1's, player 1's save gets player 2's run (only player 1's
/// results are saved). Read when the results close, so this may be called
/// any time (also live from the settings screen).
pub fn set_split_share(on: bool) {
    // SAFETY: plain C flag; the game reads it on its own thread.
    #[cfg(feature = "static")]
    unsafe {
        pw64_split_share = on as i32
    };
    // SAFETY: same global, exported by the loaded game module.
    #[cfg(feature = "dylib")]
    unsafe {
        *dylib::game().split_share = on as i32
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
