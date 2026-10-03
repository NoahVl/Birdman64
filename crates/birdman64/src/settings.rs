//! In-game settings screen (`docs/notes/settings.md`): an egui overlay drawn
//! over the game frame by the window thread.
//!
//! - **Trigger:** the configured keyboard key (default **F10**) or, on every
//!   pad, the configured button (default **Select** / Create / Capture). Both
//!   are inputs the game never reads — `input.rs` never binds them — so only
//!   the overlay consumes a press. Configurable in `pw64.toml`
//!   `[input] settings_key` / `settings_pad_button`.
//! - **Pause model:** while open, the OS core is paused (`pw64_platform::os`
//!   `PAUSED`: the clock is frozen, so threads sit in their waits and no
//!   timer/retrace fires — nothing to replay on resume), and the input thread
//!   routes pad input here instead of to the game.
//! - **Rendering:** plain egui + egui-wgpu on the shared wgpu device, own
//!   render pass on top of the game frame, window thread only. When closed,
//!   none of this runs — game rendering is untouched.
//! - **Pages (U2):** Main ("Paused": Resume, Controls, Display & graphics,
//!   Volume, Open save folder, Quit), Graphics (display + picture quality
//!   rows, Reset to defaults, Back) and Controls (rebinding). B / Escape on a
//!   sub-page goes back to Main with the selection on the row that opened it.
//! - **Quit (U3):** the Main page's "Quit Birdman64" row asks twice: the
//!   first confirm arms a red "Press again to quit" for 3 s, the second
//!   exits (`App::redraw` takes it after the D2 auto-save above ran). Any
//!   other row or navigation disarms.
//! - **Settings → effect:** display mode (with the exclusive resolution and
//!   its keep-it countdown), render resolution, scale filter, volume, frame
//!   rate (`fps`) and V-Sync apply live (display mode / fps / vsync via
//!   `Gpu::present_dirty` → `App::apply_present`); MSAA, widescreen and
//!   fill screen need a restart (always marked with a weak " (restart)",
//!   the value turns orange when the staged value differs from the running
//!   one, and a banner lists what a restart would apply).
//! - **Auto-save (D2):** there is no Save row. Every close path (Esc / B /
//!   Start on Main, Resume, the settings key, the panel's X) funnels into
//!   [`Overlay::close`], which writes `pw64.toml` (`config::save_settings`,
//!   round-trip through `toml::Table`, so unknown keys survive; comments
//!   don't) when anything differs from the values staged at open. A
//!   restart-pending close queues the usual toast; a failed save queues
//!   "Couldn't save settings: {e}".
//! - **UI zoom (U1):** the panel and toasts scale with the window height
//!   ([`ui_zoom`]): 1 at 720 logical px, up to 2.5, so the text is readable
//!   on a TV. The panel is centered with a dimmed backdrop.
//! - **Bindings:** the Controls page (the "Controls ›" row) rebinds
//!   live: a Bind row captures the next key (Escape cancels; the settings
//!   key and F11 are rejected) or pad button (the input thread's pad
//!   capture, 5 s timeout); Reset rows restore the defaults. Save writes
//!   the resulting `[input.keyboard]` / `[input.gamepad]` maps.

use crate::window::Gpu;
use crate::{audio, config, input, opts};
use egui_wgpu::Renderer as EguiRenderer;
use gilrs::Button;
use pw64_gfx::TexFilter;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use wgpu::FilterMode;
use winit::keyboard::KeyCode;

static OPEN: AtomicBool = AtomicBool::new(false);

/// egui UI zoom (U1): 1 at 720 logical px tall (the 960x720 window), more on
/// bigger windows, so the panel is readable on a TV. Pointer coordinates
/// must be divided by `ppp * zoom` (`Overlay::ppp` stores the product).
pub(crate) fn ui_zoom(height_px: u32, ppp: f32) -> f32 {
    (height_px as f32 / ppp / 720.0).clamp(1.0, 2.5)
}

/// Is the overlay open (the game paused, pads routed here)? Read by the
/// input thread and the window.
pub fn is_open() -> bool {
    OPEN.load(Ordering::Acquire)
}

/// The keyboard trigger (`[input] settings_key`, default F10).
pub fn key() -> KeyCode {
    static KEY: OnceLock<KeyCode> = OnceLock::new();
    *KEY.get_or_init(|| {
        let name = config::get().input.settings.key.as_deref();
        name.and_then(|n| {
            let k = input::parse_key(n);
            if k.is_none() {
                eprintln!("[config] input.settings_key = {n:?}: unknown key; using F10");
            }
            k
        })
        .unwrap_or(KeyCode::F10)
    })
}

/// The pad trigger (`[input] settings_pad_button`, default Select). Never
/// bound to an N64 slot (`input::gamepad_bindings`), so the game never sees it.
pub fn pad_button() -> Button {
    static BTN: OnceLock<Button> = OnceLock::new();
    *BTN.get_or_init(|| {
        let name = config::get().input.settings.pad_button.as_deref();
        name.and_then(|n| {
            let b = input::parse_gamepad_button(n);
            if b.is_none() {
                eprintln!("[config] input.settings_pad_button = {n:?}: unknown; using Select");
            }
            b
        })
        .unwrap_or(Button::Select)
    })
}

/// One selectable row (R4/U2): per page, in navigation order
/// ([`Overlay::rows`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    /// A settings option (Graphics: display + picture quality; Main: Volume).
    Opt(Opt),
    /// A binding slot of one input kind (Controls page): capture the next
    /// input and bind it to the slot.
    Bind(Kind, usize),
    /// Reset one input kind to its defaults (Controls page).
    Reset(Kind),
    /// Main: back to the game (closes the screen, D2 auto-save applies).
    Resume,
    /// Main: open the Graphics page.
    Graphics,
    /// Graphics: stage the built-in defaults and apply the live ones.
    ResetGraphics,
    /// Open the Controls page (Main page).
    Controls,
    /// Leave a sub-page (back to Main).
    Back,
    /// Main: open the save/data folder in the file manager (U15).
    OpenSaveFolder,
    /// Main: quit the game (two confirms, U3).
    Quit,
    /// Graphics: the restart banner's "Restart now" row (U16). Only on the
    /// page while restart-only changes are pending (the banner's row).
    RestartNow,
}

/// Which input kind a Bind / Reset row edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Keyboard,
    Gamepad,
}

/// The overlay pages (U2): Main ("Paused": Resume / Controls / Display &
/// graphics / Volume), Graphics (display + picture quality) and Controls
/// (rebinding). B / Escape on a sub-page goes back to Main.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Main,
    Graphics,
    Controls,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Opt {
    Msaa,
    Scale,
    Filter,
    TexFilter,
    Widescreen,
    FillScreen,
    Fps,
    Vsync,
    DisplayMode,
    Resolution,
    Volume,
    /// OLED care (O3): the HUD drifts (spreads burn-in) and dims.
    OledDrift,
    OledBrightness,
    /// U25: the window title shows the frame rate.
    ShowFps,
    /// U26: pause (open the overlay) when the window loses focus or the
    /// last controller disconnects (Main page).
    PauseBackground,
    /// Switch 2 pads over Bluetooth LE (`ble.rs`, Controls page): live,
    /// switches the scanner on/off without a restart.
    Ble,
}

/// Frame-rate choices (the running one is added if it's another rate).
const FPS_STEPS: [opts::Fps; 8] = [
    opts::Fps::Monitor,
    opts::Fps::Hz(30),
    opts::Fps::Hz(60),
    opts::Fps::Hz(120),
    opts::Fps::Hz(144),
    opts::Fps::Hz(165),
    opts::Fps::Hz(240),
    opts::Fps::Uncapped,
];

const MSAA_STEPS: [u32; 3] = [1, 4, 8];
/// Render resolution steps (`PW64_SCALE`), fractions of the window size;
/// below 1 upscales (blurry), above 1 supersamples.
const SCALE_STEPS: [f32; 7] = [0.5, 0.75, 1.0, 1.5, 2.0, 3.0, 4.0];
/// Volume in permille, 10 % steps.
const VOLUME_STEPS: [u32; 11] = [0, 100, 200, 300, 400, 500, 600, 700, 800, 900, 1000];
/// Stick deflection (raw N64 units) that counts as a d-pad press.
const NAV_STICK: i8 = 40;
const REPEAT_DELAY: Duration = Duration::from_millis(350);
const REPEAT_RATE: Duration = Duration::from_millis(120);
/// The keep-resolution countdown (S8b): how long an applied exclusive
/// resolution stays up without a confirm before it reverts.
const REVERT: Duration = Duration::from_secs(10);
/// A gamepad capture gives up after this long (a keyboard capture waits
/// indefinitely: the next `on_key` press ends it).
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the Quit row stays armed ("Press again to quit", U3).
const QUIT_ARM: Duration = Duration::from_secs(3);

/// The navigation footer (U1, weak 13 pt). The pad letters are the U6
/// family names of South / East once that lands; until then the literal
/// A / B.
const FOOTER: &str = "Up/Down: Select   Left/Right: Change   Enter / A: Confirm   Esc / B: Back";

/// A binding capture in progress (`Overlay::capture`): which kind + slot
/// row is being captured, the slot's name (bind target), and when it
/// started (the gamepad capture times out).
struct Capture {
    kind: Kind,
    slot: usize,
    name: &'static str,
    started: Instant,
}

/// Restart-only options as the running game uses them (env > config >
/// default; captured once, they can't change until a restart). Frame rate
/// and V-Sync are live (in `Gpu`), so they are not here any more.
struct Running {
    msaa: u32,
    widescreen: Option<f32>,
    fill_screen: bool,
}

/// The S8b keep-resolution countdown: what to go back to when an applied
/// exclusive resolution is not confirmed in time (the previous display mode
/// and `fullscreen_res`).
struct Revert {
    mode: opts::DisplayMode,
    res: Option<opts::Res>,
    deadline: Instant,
}

/// The previous display mode + resolution a revert restores.
type RevertTo = (opts::DisplayMode, Option<opts::Res>);

/// The overlay: staged restart options, mirrors of the live ones, and the
/// egui state. Created on first open.
pub struct Overlay {
    ctx: egui::Context,
    renderer: Option<EguiRenderer>,
    /// Cleared by Escape / B / Start / Close / the window's X; the window
    /// closes the overlay when `render` returns false.
    show: bool,
    running: Running,
    // Staged (restart) options.
    msaa: u32,
    widescreen: Vec<Option<String>>,
    widescreen_sel: usize,
    /// Staged fill screen (restart-only, like widescreen).
    fill_screen: bool,
    /// S8: the staged widescreen is the monitor default from "Reset to
    /// defaults": saving removes the key (the file follows the display)
    /// instead of pinning a value. Cleared by any Widescreen row change.
    widescreen_follow: bool,
    /// S7: the restart-only values (MSAA, widescreen aspect, fill screen)
    /// this session last saved; `open` stages these instead of the running
    /// ones while that restart is pending.
    saved_restart: Option<(u32, Option<f32>, bool)>,
    fps: Vec<opts::Fps>,
    fps_sel: usize,
    vsync: bool,
    // Live options: pushed to the window's `Gpu` every frame.
    scale: f32,
    filter: FilterMode,
    /// Texture filter (`PW64_FILTER`): the window renderer's, live.
    tex_filter: TexFilter,
    /// Display mode choices (Exclusive left out when the window can't do
    /// it) and the staged index. Left/right only stages; A/Enter applies
    /// (a monitor mode switch per step would flicker for seconds).
    display_choices: Vec<opts::DisplayMode>,
    display_mode_sel: usize,
    /// Resolution choices for an exclusive fullscreen: the desktop mode
    /// (`None`, labelled `res_desktop`) first, then the monitor's video
    /// modes, highest first.
    res_choices: Vec<Option<opts::Res>>,
    res_sel: usize,
    /// The desktop choice's label (the monitor's current mode).
    res_desktop: String,
    /// A resolution change waiting to be applied (staged like the mode).
    apply_display_mode: bool,
    apply_res: bool,
    /// The running display mode / exclusive resolution (mirrored from the
    /// window every frame; the staged values are compared against these
    /// for the restart banner and the Save toast).
    running_mode: opts::DisplayMode,
    running_res: Option<opts::Res>,
    /// The keep-resolution countdown (`None`: not running).
    revert: Option<Revert>,
    /// A revert decision (timeout / B) waiting for `apply_live`, which
    /// alone can reach the window.
    pending_revert: Option<RevertTo>,
    /// Volume in permille (0..1000); applied live on change.
    volume: u32,
    /// OLED care (O3, live): the HUD drifts and dims by these.
    oled_drift: bool,
    oled_brightness: f32,
    /// U25 (live): the window title shows the frame rate.
    show_fps: bool,
    /// U26 (live): pause when the window loses focus / the last pad
    /// disconnects (Main page row).
    pause_background: bool,
    /// Switch 2 pads over BLE (live: `ble::set_enabled` on change).
    ble: bool,
    /// Toast queued by a close that saved restart-pending changes (or
    /// failed to save); the window takes it (`take_toast`) and shows it
    /// over the game.
    saved_toast: Option<String>,
    /// The staged values as they were at open (D2): close() saves only what
    /// differs from this snapshot.
    snapshot: config::SavedSettings,
    /// The staged scale's target size in px (`Gpu::fb_size_for_scale`,
    /// shown next to the Render resolution row).
    fb: (u32, u32),
    /// Quit row state (U3): the arm time while "Press again to quit" shows
    /// (`None`: not armed), and a confirmed quit the window takes with
    /// [`Overlay::take_quit`] after the close-save.
    quit_armed: Option<Instant>,
    quit_requested: bool,
    /// A confirmed "Restart now" (U16) the window takes with
    /// [`Overlay::take_restart`] after the close-save: spawn + exit there.
    restart_requested: bool,
    /// Why the last "Restart now" failed to spawn (shown under the restart
    /// banner, U16); cleared with the banner itself.
    restart_error: Option<String>,
    /// The monitor's refresh rate in whole Hz (the Frame-rate row's
    /// "Match display ({hz} Hz)" value, U5); `None`: unknown.
    monitor_hz: Option<u32>,
    /// A binding capture in progress (`None`: idle).
    capture: Option<Capture>,
    /// Red line for a rejected key ("reserved") or a timed-out pad capture.
    capture_error: Option<String>,
    /// Window scale factor (physical px per egui point) times the UI zoom
    /// (U1): pointer events divide by this.
    ppp: f32,
    /// Queued pointer events for the next frame (positions in egui points).
    events: Vec<egui::Event>,
    /// Selection and pad-nav edge/repeat state. `sel` indexes the current
    /// page's `rows()` vector.
    sel: usize,
    /// The page shown (U2: Main, Graphics, Controls).
    page: Page,
    /// The Main row that opened the current sub-page (`back()` rests the
    /// selection on it).
    main_row: Row,
    /// True while pad/keyboard navigation just moved the selection: the
    /// ScrollArea scrolls the selected row back into view (U1). Hover/click
    /// don't move the view (the pointer is already there).
    follow_sel: bool,
    prev_pad: input::Pad,
    held_dir: Option<i32>,
    next_repeat: Option<Instant>,
    started: Instant,
}

impl Overlay {
    pub fn new() -> Self {
        let running = Running {
            msaa: opts::msaa(),
            widescreen: opts::widescreen(),
            fill_screen: opts::fill_screen(),
        };
        let staged_fill = running.fill_screen;
        // Staged lists up front so the overlay is consistent before open()
        // (open re-stages from the live values; `staged_settings` reads
        // these in tests).
        let (widescreen, widescreen_sel) = widescreen_choices(running.widescreen);
        let (fps, fps_sel) = fps_choices(opts::Fps::Monitor);
        Self {
            ctx: egui::Context::default(),
            renderer: None,
            show: false,
            msaa: running.msaa,
            running,
            widescreen,
            widescreen_sel,
            fill_screen: staged_fill,
            widescreen_follow: false,
            saved_restart: None,
            fps,
            fps_sel,
            vsync: false,
            scale: 1.0,
            filter: FilterMode::Linear,
            tex_filter: TexFilter::Bilinear,
            display_choices: display_mode_choices(true),
            display_mode_sel: 0,
            res_choices: vec![None],
            res_sel: 0,
            res_desktop: "Desktop".into(),
            apply_display_mode: false,
            apply_res: false,
            running_mode: opts::DisplayMode::Windowed,
            running_res: None,
            revert: None,
            pending_revert: None,
            volume: 1000,
            oled_drift: opts::oled_drift(),
            oled_brightness: opts::oled_brightness(),
            show_fps: opts::show_fps(),
            pause_background: opts::pause_in_background(),
            ble: crate::ble::is_on(),
            saved_toast: None,
            snapshot: config::SavedSettings::default(),
            fb: (960, 720),
            quit_armed: None,
            quit_requested: false,
            restart_requested: false,
            restart_error: None,
            monitor_hz: None,
            capture: None,
            capture_error: None,
            ppp: 1.0,
            events: Vec::new(),
            sel: 0,
            page: Page::Main,
            main_row: Row::Resume,
            follow_sel: false,
            prev_pad: input::Pad::default(),
            held_dir: None,
            next_repeat: None,
            started: Instant::now(),
        }
    }

    /// Opens: stage the current settings, then route the pads here. The
    /// caller pauses the OS core.
    pub fn open(&mut self, gpu: &Gpu) {
        self.show = true;
        self.sel = 0;
        self.page = Page::Main;
        self.events.clear();
        self.capture = None;
        self.capture_error = None;
        self.apply_display_mode = false;
        self.apply_res = false;
        self.revert = None;
        self.pending_revert = None;
        self.restart_error = None;
        self.stage_restart_rows();
        // Frame rate and V-Sync are live: stage from the running values.
        (self.fps, self.fps_sel) = fps_choices(gpu.fps);
        self.vsync = gpu.vsync;
        self.scale = gpu.scale;
        self.filter = gpu.filter;
        self.tex_filter = gpu.renderer.options.filter;
        // OLED care is live: stage from the running values (O3).
        self.oled_drift = gpu.oled_drift;
        self.oled_brightness = gpu.oled_brightness;
        // U25/U26 live rows stage from the window too.
        self.show_fps = gpu.show_fps;
        self.pause_background = gpu.pause_background;
        // BLE pads (live): the switch as it stands.
        self.ble = crate::ble::is_on();
        self.fb = gpu.fb_size_for_scale(self.scale);
        // Display mode (live): stage the running one; Exclusive is left out
        // of the choices when this window can't do it (Wayland).
        self.display_choices = display_mode_choices(gpu.exclusive_ok);
        self.display_mode_sel = self
            .display_choices
            .iter()
            .position(|&m| m == gpu.display_mode)
            .unwrap_or(0);
        // Resolution choices (used in exclusive fullscreen only): the
        // monitor's current mode first, then every video mode.
        let monitor = gpu.window.current_monitor();
        // U5: the Frame-rate row names the rate it tracks.
        self.monitor_hz = monitor
            .as_ref()
            .and_then(|m| m.refresh_rate_millihertz())
            .filter(|hz| *hz > 0)
            .map(|hz| (hz + 500) / 1000);
        self.res_desktop = monitor.as_ref().map_or_else(
            || "Desktop".into(),
            |m| {
                let s = m.size();
                match m.refresh_rate_millihertz() {
                    None | Some(0) => "Desktop".into(),
                    Some(hz) => format!(
                        "Desktop ({}×{} @ {} Hz)",
                        s.width,
                        s.height,
                        (hz + 500) / 1000
                    ),
                }
            },
        );
        let modes = monitor
            .as_ref()
            .map(|m| {
                m.video_modes()
                    .map(|v| {
                        (
                            v.size().width,
                            v.size().height,
                            (v.refresh_rate_millihertz() + 500) / 1000,
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        self.res_choices = resolution_choices(modes);
        self.res_sel = self
            .res_choices
            .iter()
            .position(|r| *r == gpu.fullscreen_res)
            .unwrap_or(0);
        self.running_mode = gpu.display_mode;
        self.running_res = gpu.fullscreen_res;
        // U1: pointer coordinates divide by ppp * zoom.
        let native_ppp = gpu.window.scale_factor() as f32;
        self.ppp = native_ppp * ui_zoom(gpu.config.height, native_ppp);
        self.volume = (audio::volume() * 1000.0).round().clamp(0.0, 1000.0) as u32;
        // D2: what the screen shows right now; close() saves what differs.
        self.snapshot = self.staged_settings();
        OPEN.store(true, Ordering::Release);
        input::overlay_toggled(true);
        // Whatever is held right now (the pad that opened it) is not a press.
        self.prev_pad = input::overlay_pad();
        self.held_dir = nav_dir(self.prev_pad);
        self.next_repeat = None;
    }

    /// Closes: hand the pads back to the game. The caller unpauses the OS
    /// core (window.rs). Every close path funnels here (D2): the staged
    /// values that differ from the snapshot taken in `open` are saved.
    pub fn close(&mut self) {
        self.show = false;
        // A pad capture still armed (the pad Select that closed the overlay)
        // must not outlive it.
        self.end_capture();
        // Closing keeps what was applied: the countdown has nothing to revert
        // to once the overlay stops redrawing.
        self.revert = None;
        // D2 auto-save: write what changed since open. A restart-pending
        // close queues the usual toast, a failed save the error toast.
        self.save_changes();
        if OPEN.swap(false, Ordering::AcqRel) {
            input::overlay_toggled(false);
        }
    }

    /// S7: stages the restart-only rows (MSAA, widescreen, fill screen) from
    /// what this session last saved (that restart is still pending), else
    /// from the running values, so a saved change can be undone before the
    /// restart. The banner and "(restart)" marks keep comparing against
    /// `running`.
    fn stage_restart_rows(&mut self) {
        let (msaa, widescreen, fill) = self.saved_restart.unwrap_or((
            self.running.msaa,
            self.running.widescreen,
            self.running.fill_screen,
        ));
        self.msaa = msaa;
        (self.widescreen, self.widescreen_sel) = widescreen_choices(widescreen);
        self.fill_screen = fill;
        self.widescreen_follow = false;
    }

    /// The D2 auto-save, shared by `close` and the "Restart now" row (U16):
    /// writes what changed since the snapshot taken at open. A restart-pending
    /// save queues the usual toast, a failed save the error toast.
    fn save_changes(&mut self) {
        let staged = self.staged_settings();
        if staged != self.snapshot {
            match config::save_settings(changed_since(staged, &self.snapshot)) {
                Ok(()) => {
                    // S7: the next open stages these, not the running ones.
                    self.saved_restart = Some((
                        self.msaa,
                        choice_aspect(&self.widescreen[self.widescreen_sel]),
                        self.fill_screen,
                    ));
                    let pending = self.pending_restart();
                    if !pending.is_empty() {
                        self.saved_toast = Some(format!(
                            "Saved. Restart Birdman64 to apply: {}",
                            pending.join(", ")
                        ));
                    }
                }
                Err(e) => {
                    eprintln!("[settings] {e}");
                    self.saved_toast = Some(format!("Couldn't save settings: {e}"));
                }
            }
        }
    }

    /// The toast from the last successful Save (`None`: nothing pending).
    /// Taken by the window after the overlay frame (`saved_toast`).
    pub fn take_toast(&mut self) -> Option<String> {
        self.saved_toast.take()
    }

    /// A confirmed quit (U3): taken by the window after `close` ran (the
    /// D2 auto-save has written the staged settings by then).
    pub fn take_quit(&mut self) -> bool {
        std::mem::take(&mut self.quit_requested)
    }

    /// A confirmed "Restart now" (U16): taken by the window like
    /// [`Overlay::take_quit`] after `close` ran (the D2 auto-save and the
    /// spawn have both happened; only the exit is left).
    pub fn take_restart(&mut self) -> bool {
        std::mem::take(&mut self.restart_requested)
    }

    /// Disarms the Quit row once its 3 s window has passed (U3). Called
    /// each overlay frame; a later confirm then arms it again instead of
    /// quitting.
    fn quit_check(&mut self) {
        if self.quit_armed.is_some_and(|t| t.elapsed() >= QUIT_ARM) {
            self.quit_armed = None;
        }
    }

    /// The "Restart now" row (U16): save (the D2 auto-save), start this exe
    /// again with the original command line, then quit. A spawn failure
    /// keeps the overlay open: the banner stays and the status line says why.
    fn restart_now(&mut self) {
        self.restart_with(restart_command());
    }

    /// `restart_now`'s body, taking the command so tests can inject one
    /// without spawning anything real.
    fn restart_with(&mut self, cmd: std::io::Result<std::process::Command>) {
        self.save_changes();
        // Drop the U14 running marker before the new instance starts, or it
        // can find it (we quit only after the spawn) and toast "didn't close
        // properly".
        let lock = crate::paths::running_lock();
        // Only a marker that was there goes back on failure (tests and
        // non-window runs have none; creating one would leave a stray file).
        let had_lock = std::fs::remove_file(&lock).is_ok();
        // S9: hand over the single-instance lock; the new copy also waits a
        // few seconds for it (`sys::RESTARTED_ENV`) in case of a race.
        crate::sys::release_instance();
        let err = match cmd {
            Ok(mut cmd) => match cmd.env(crate::sys::RESTARTED_ENV, "1").spawn() {
                Ok(_) => {
                    // The window takes this after `close` ran (which already
                    // happened below via `show = false`) and exits the
                    // process; the spawned copy takes over.
                    self.restart_requested = true;
                    self.show = false;
                    return;
                }
                Err(e) => format!("Couldn't restart: {e}"),
            },
            Err(e) => format!("Couldn't restart: {e}"),
        };
        // Still running: put the marker and the instance lock back.
        if had_lock {
            let _ = std::fs::File::create(&lock);
        }
        crate::sys::reclaim_instance();
        self.restart_error = Some(err);
    }

    /// Arms a capture on a Bind row (confirm). A still-running capture is
    /// ended first (its pad capture disarmed).
    fn start_capture(&mut self, kind: Kind, slot: usize, name: &'static str) {
        self.end_capture();
        if kind == Kind::Gamepad {
            input::start_pad_capture();
        }
        self.capture_error = None;
        self.capture = Some(Capture {
            kind,
            slot,
            name,
            started: Instant::now(),
        });
    }

    /// Ends the running capture (done, cancelled, or the overlay closed):
    /// disarms the pad capture and re-syncs the pad edge state, so the
    /// button that ended the capture doesn't re-confirm the row.
    fn end_capture(&mut self) {
        if self.capture.take().is_some() {
            input::cancel_pad_capture();
            self.capture_error = None;
            self.prev_pad = input::overlay_pad();
        }
    }

    /// Gamepad capture poll + timeout, each overlay frame (the keyboard
    /// capture is driven by `on_key` instead).
    fn poll_capture(&mut self) {
        let Some(cap) = &self.capture else { return };
        if cap.kind != Kind::Gamepad {
            return;
        }
        if let Some(b) = input::take_captured_button() {
            let name = cap.name;
            input::bind_button(name, b);
            self.end_capture();
        } else if cap.started.elapsed() >= CAPTURE_TIMEOUT {
            self.capture_error = Some("No button pressed. Select the row to try again.".into());
            self.end_capture();
        }
    }

    /// The next key of a keyboard capture (`on_key` routed it here).
    /// Reserved keys are rejected with a red line (the settings key never
    /// reaches the game, and F11 fullscreens instead of reaching it); any
    /// other key binds and ends the capture.
    fn capture_key(&mut self, code: KeyCode) {
        let Some(cap) = &self.capture else { return };
        if code == key() || code == KeyCode::F11 {
            // Reserved keys (U5 wording): the settings key by name, F11 by
            // what it actually does.
            let name = key_label(&format!("{code:?}"));
            self.capture_error = Some(if code == key() {
                format!(
                    "{name} opens settings and can't be used. Press another key, or Esc to cancel."
                )
            } else {
                format!(
                    "{name} switches fullscreen and can't be used. Press another key, or Esc to cancel."
                )
            });
            return;
        }
        let name = cap.name;
        input::bind_key(name, code);
        self.end_capture();
    }

    /// A on the banner (any confirm): keep the applied resolution (S8b).
    fn revert_confirm(&mut self) {
        self.revert = None;
    }

    /// B / Escape: revert now. `None` when nothing was pending.
    fn revert_action(&mut self) -> Option<RevertTo> {
        let r = self.revert.take()?;
        Some((r.mode, r.res))
    }

    /// Checked once per overlay frame: past the deadline, revert (S8b).
    fn revert_check(&mut self, now: Instant) -> Option<RevertTo> {
        if self.revert.as_ref().is_some_and(|r| now >= r.deadline) {
            self.revert_action()
        } else {
            None
        }
    }

    /// Points the staged Display mode / Resolution rows at `mode` / `res`
    /// (after a revert). A value missing from the choices keeps the row.
    fn restage_display(&mut self, mode: opts::DisplayMode, res: Option<opts::Res>) {
        if let Some(i) = self.display_choices.iter().position(|&m| m == mode) {
            self.display_mode_sel = i;
        }
        if let Some(i) = self.res_choices.iter().position(|r| *r == res) {
            self.res_sel = i;
        }
    }

    /// The staged Display mode choice.
    fn display_mode_choice(&self) -> opts::DisplayMode {
        self.display_choices[self.display_mode_sel]
    }

    /// The staged Resolution choice (`None` = the desktop mode).
    fn res_choice(&self) -> Option<opts::Res> {
        self.res_choices[self.res_sel]
    }

    /// Feeds one winit key event (called only while open). Repeats are
    /// allowed for the direction keys so holding scrolls. While a capture
    /// runs it owns the keyboard: Escape cancels, a keyboard capture binds
    /// the next key, everything else (nav, repeats) is ignored.
    pub fn on_key(&mut self, code: KeyCode, repeat: bool, pressed: bool) {
        if !pressed {
            return;
        }
        if self.capture.is_some() {
            if code == KeyCode::Escape {
                self.end_capture();
            } else if !repeat
                && self
                    .capture
                    .as_ref()
                    .is_some_and(|c| c.kind == Kind::Keyboard)
            {
                self.capture_key(code);
            }
            return;
        }
        match code {
            KeyCode::ArrowUp => self.move_sel(-1),
            KeyCode::ArrowDown => self.move_sel(1),
            KeyCode::ArrowLeft => self.change(-1, false),
            KeyCode::ArrowRight => self.change(1, false),
            KeyCode::Enter | KeyCode::NumpadEnter | KeyCode::Space if !repeat => self.confirm(),
            KeyCode::Escape => {
                // While the keep-resolution countdown runs, back reverts
                // instead of closing (the timeout would never fire once the
                // overlay stops redrawing). On a sub-page, back goes to
                // Main instead of closing.
                if self.revert.is_some() {
                    self.pending_revert = self.revert_action();
                } else if self.page != Page::Main {
                    self.back();
                } else {
                    self.show = false;
                }
            }
            _ => {}
        }
    }

    /// Cursor position from the window, physical px.
    pub fn pointer_moved(&mut self, x: f32, y: f32) {
        let pos = egui::pos2(x / self.ppp, y / self.ppp);
        self.events.push(egui::Event::PointerMoved(pos));
    }

    /// Primary mouse button at physical px `(x, y)`.
    pub fn pointer_button(&mut self, x: f32, y: f32, pressed: bool) {
        self.events.push(egui::Event::PointerButton {
            pos: egui::pos2(x / self.ppp, y / self.ppp),
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        });
    }

    /// Runs one overlay frame over the game frame already in `view` (after
    /// the game render, before present): pad navigation, the egui pass, the
    /// live settings. Returns false once the overlay wants to close.
    pub fn render(&mut self, gpu: &mut Gpu, view: &wgpu::TextureView) -> bool {
        self.poll_capture();
        self.quit_check();
        self.pad_nav();
        self.running_mode = gpu.display_mode;
        self.running_res = gpu.fullscreen_res;
        // U1: the UI zooms with the window height (readable on a TV).
        // Pointer coordinates divide by ppp * zoom.
        let native_ppp = gpu.window.scale_factor() as f32;
        let zoom = ui_zoom(gpu.config.height, native_ppp);
        if (self.ctx.zoom_factor() - zoom).abs() > f32::EPSILON {
            // Takes effect at the start of the next pass. Only on change:
            // while it is pending egui replaces the fresh screen rect with
            // the previous pass's (its anti-jitter hack), so calling it
            // every frame would freeze the layout.
            self.ctx.set_zoom_factor(zoom);
        }
        self.ppp = native_ppp * zoom;
        // Follow the window size (the row shows the staged scale's pixels).
        self.fb = gpu.fb_size_for_scale(self.scale);
        let raw = raw_input_zoom(
            gpu,
            native_ppp,
            self.started.elapsed().as_secs_f64(),
            std::mem::take(&mut self.events),
        );
        let ctx = self.ctx.clone();
        let out = ctx.run(raw, |ctx| self.build_ui(ctx));
        // S8b: the keep-resolution deadline is checked each overlay frame
        // (after the UI pass, so an expired banner never shows).
        if let Some(a) = self.revert_check(Instant::now()) {
            self.pending_revert = Some(a);
        }
        self.apply_live(gpu);
        // Load: draw over the game frame already in the target.
        paint(
            gpu,
            &self.ctx,
            &mut self.renderer,
            out,
            view,
            wgpu::LoadOp::Load,
        );
        if self.show {
            // The game is paused, so no Frame events drive the loop.
            gpu.window.request_redraw();
        }
        self.show
    }

    /// Pushes the live options to the window.
    fn apply_live(&mut self, gpu: &mut Gpu) {
        if std::mem::take(&mut self.apply_display_mode) {
            gpu.set_display_mode(self.display_mode_choice());
            // The monitor (refresh) may change with the mode: re-read the
            // present rate/mode (R-a).
            gpu.present_dirty = true;
        }
        if std::mem::take(&mut self.apply_res) {
            // The previous mode + resolution are what a timeout / B goes
            // back to (S8b).
            let prev = (gpu.display_mode, gpu.fullscreen_res);
            gpu.fullscreen_res = self.res_choice();
            gpu.set_display_mode(opts::DisplayMode::Exclusive);
            gpu.present_dirty = true;
            self.revert = Some(Revert {
                mode: prev.0,
                res: prev.1,
                deadline: Instant::now() + REVERT,
            });
        }
        if let Some((mode, res)) = std::mem::take(&mut self.pending_revert) {
            gpu.fullscreen_res = res;
            gpu.set_display_mode(mode);
            gpu.present_dirty = true;
            // Re-stage what was reverted to: otherwise the auto-save on close
            // writes the rejected mode + resolution and the next launch
            // starts in it.
            self.restage_display(mode, res);
        }
        gpu.scale = self.scale;
        gpu.filter = self.filter;
        // Shaders/pipelines are cached per filter: the next frame just uses
        // the other set (the dump renderer keeps the startup value).
        gpu.renderer.options.filter = self.tex_filter;
        // OLED care (O3): live, like the filters. The drift time comes from
        // the window's clock at the next drain.
        gpu.oled_drift = self.oled_drift;
        gpu.oled_brightness = self.oled_brightness;
        // U25/U26: live rows the window reads itself.
        gpu.show_fps = self.show_fps;
        gpu.pause_background = self.pause_background;
        // Frame rate and V-Sync are live: the window recomputes the present
        // rate + mode (`present_dirty` → `App::apply_present`).
        if gpu.fps != self.fps[self.fps_sel] || gpu.vsync != self.vsync {
            gpu.fps = self.fps[self.fps_sel];
            gpu.vsync = self.vsync;
            gpu.present_dirty = true;
        }
    }

    /// Builds the panel (inside the egui pass): a centered, fixed-width
    /// window over a dimmed backdrop (U1).
    fn build_ui(&mut self, ctx: &egui::Context) {
        let screen = ctx.content_rect();
        ctx.layer_painter(egui::LayerId::background()).rect_filled(
            screen,
            0.0,
            egui::Color32::from_black_alpha(140),
        );
        let title = match self.page {
            Page::Main => "Paused",
            Page::Controls => "Controls",
            Page::Graphics => "Display & graphics",
        };
        // The wide Controls page (U9): with room for both, the binding list
        // scrolls on the left and the controller drawing sits fixed on the
        // right, vertically centred. Narrower windows stack them: the
        // drawing fixed above the scrolling list.
        let wide = self.page == Page::Controls && screen.width() >= 1000.0;
        let width = if wide {
            880.0_f32.min(screen.width() * 0.85)
        } else {
            460.0
        };
        let mut open = self.show;
        egui::Window::new(egui::RichText::new(title).size(18.0))
            .open(&mut open)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .collapsible(false)
            .resizable(false)
            .default_width(width)
            .min_width(width)
            .show(ctx, |ui| {
                if self.page != Page::Controls {
                    egui::ScrollArea::vertical()
                        .max_height(0.8 * screen.height())
                        .show(ui, |ui| self.ui_rows(ui));
                } else if wide {
                    self.controls_wide(ui, 0.8 * screen.height());
                } else {
                    // The drawing never scrolls (U10): fixed above the list.
                    crate::pad_art::draw(ui, self.selected_slot().as_deref());
                    ui.add_space(4.0);
                    egui::ScrollArea::vertical()
                        .max_height(0.8 * screen.height())
                        .show(ui, |ui| self.ui_rows(ui));
                }
            });
        self.show &= open;
    }

    /// The wide Controls page (U9): the binding list (with its help line and
    /// footer) scrolls in a left column; the controller drawing sits fixed in
    /// the right one, vertically centred, so its highlight tracks the
    /// selection while the list scrolls.
    fn controls_wide(&mut self, ui: &mut egui::Ui, max_h: f32) {
        const LIST_W: f32 = 440.0;
        // Main axis left-to-right, children centred vertically (the drawing
        // column: the list sizes itself).
        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
            // The list column: its own region with a vertical layout (the
            // left-to-right one must not leak into the rows).
            ui.allocate_ui_with_layout(
                egui::vec2(LIST_W, max_h),
                egui::Layout::top_down_justified(egui::Align::Min),
                |ui| {
                    egui::ScrollArea::vertical()
                        .max_height(max_h)
                        .id_salt("pw64 controls wide")
                        .show(ui, |ui| self.ui_rows(ui));
                },
            );
            ui.add_space(8.0);
            // The drawing, centred in what is left (`draw` clamps its own
            // width to MAX_W).
            ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                crate::pad_art::draw(ui, self.selected_slot().as_deref());
            });
        });
    }

    /// Every selectable row of the current page, in navigation order
    /// (U2 pages). `ui_rows` draws exactly these, so keyboard/pad selection
    /// and mouse clicks stay in sync.
    fn rows(&self) -> Vec<Row> {
        match self.page {
            Page::Main => vec![
                Row::Resume,
                Row::Controls,
                Row::Graphics,
                Row::Opt(Opt::Volume),
                // U26: pause when the window goes to the background.
                Row::Opt(Opt::PauseBackground),
                Row::OpenSaveFolder,
                Row::Quit,
            ],
            Page::Graphics => {
                let mut rows = vec![
                    Row::Opt(Opt::DisplayMode),
                    Row::Opt(Opt::Resolution),
                    Row::Opt(Opt::Widescreen),
                    Row::Opt(Opt::FillScreen),
                    Row::Opt(Opt::Fps),
                    Row::Opt(Opt::Vsync),
                    // U25: the frame rate in the window title.
                    Row::Opt(Opt::ShowFps),
                    Row::Opt(Opt::Scale),
                    Row::Opt(Opt::Filter),
                    Row::Opt(Opt::TexFilter),
                    Row::Opt(Opt::Msaa),
                    // OLED care (O3): live, no restart.
                    Row::Opt(Opt::OledDrift),
                    Row::Opt(Opt::OledBrightness),
                    Row::ResetGraphics,
                    Row::Back,
                ];
                // U16: the restart banner's "Restart now" row, first on the
                // page so it sits directly under the banner text (which only
                // shows while changes are pending).
                if !self.pending_restart().is_empty() {
                    rows.insert(0, Row::RestartNow);
                }
                rows
            }
            // The Controls page (U9 order): the keyboard slots in player
            // order, the keyboard reset, the gamepad slots, the gamepad
            // reset, and Back.
            Page::Controls => {
                let kb = input::keyboard_binding_rows();
                let gp = input::gamepad_binding_rows();
                let mut rows: Vec<Row> = ordered_slots(&kb)
                    .into_iter()
                    .map(|i| Row::Bind(Kind::Keyboard, i))
                    .collect();
                rows.push(Row::Reset(Kind::Keyboard));
                rows.extend(
                    ordered_slots(&gp)
                        .into_iter()
                        .map(|i| Row::Bind(Kind::Gamepad, i)),
                );
                rows.push(Row::Reset(Kind::Gamepad));
                // Switch 2 pads over Bluetooth (experimental), under the
                // controller section.
                rows.push(Row::Opt(Opt::Ble));
                rows.push(Row::Back);
                rows
            }
        }
    }

    /// Opens a sub-page, remembering the Main row that opened it (for
    /// `back`).
    fn enter_page(&mut self, page: Page, from: Row) {
        self.main_row = from;
        self.page = page;
        self.sel = 0;
        self.follow_sel = true;
    }

    /// B / Escape / the Back row on a sub-page: back to Main, with the
    /// selection resting on the row that opened it.
    fn back(&mut self) {
        if self.page != Page::Main {
            self.page = Page::Main;
            self.sel = self
                .rows()
                .iter()
                .position(|r| *r == self.main_row)
                .unwrap_or(0);
            self.follow_sel = true;
        }
    }

    fn ui_rows(&mut self, ui: &mut egui::Ui) {
        let rows = self.rows();
        let at = |r: Row| rows.iter().position(|&x| x == r).unwrap();
        match self.page {
            Page::Main => {
                self.text_row(ui, at(Row::Resume), "Resume game");
                self.text_row(ui, at(Row::Controls), "Controls \u{203a}");
                self.text_row(ui, at(Row::Graphics), "Display & graphics \u{203a}");
                self.opt_row(ui, at(Row::Opt(Opt::Volume)), Opt::Volume);
                self.opt_row(ui, at(Row::Opt(Opt::PauseBackground)), Opt::PauseBackground);
                self.text_row(ui, at(Row::OpenSaveFolder), "Open save folder");
                // Quit (U3): armed, the row turns into a red second chance.
                let quit_text = if self.quit_armed.is_some() {
                    egui::RichText::new("Press again to quit")
                        .size(16.0)
                        .color(egui::Color32::LIGHT_RED)
                } else {
                    egui::RichText::new("Quit Birdman64").size(16.0)
                };
                self.selectable(ui, at(Row::Quit), quit_text.into());
            }
            Page::Graphics => {
                // S8b: the keep-resolution countdown, above everything.
                if let Some(r) = &self.revert {
                    let ms = r
                        .deadline
                        .saturating_duration_since(Instant::now())
                        .as_millis() as u64;
                    ui.colored_label(
                        egui::Color32::YELLOW,
                        format!(
                            "Keep this resolution? A = keep, reverting in {} s",
                            ms.div_ceil(1000)
                        ),
                    );
                    ui.add_space(2.0);
                }
                // S5: restart-only options whose staged value differs, listed
                // at the top so the rows below can't be missed.
                let pending = self.pending_restart();
                if !pending.is_empty() {
                    ui.colored_label(
                        egui::Color32::ORANGE,
                        format!("Restart Birdman64 to apply: {}", pending.join(", ")),
                    );
                    ui.add_space(2.0);
                    // U16: the banner's own row: save and restart now.
                    self.text_row(ui, at(Row::RestartNow), "Restart now");
                    ui.add_space(2.0);
                    // A failed restart spawn (U16): the banner stays and the
                    // status line says why.
                    if let Some(e) = &self.restart_error {
                        ui.colored_label(egui::Color32::LIGHT_RED, e);
                        ui.add_space(2.0);
                    }
                }
                for o in [
                    Opt::DisplayMode,
                    Opt::Resolution,
                    Opt::Widescreen,
                    Opt::FillScreen,
                    Opt::Fps,
                    Opt::Vsync,
                    Opt::ShowFps,
                    Opt::Scale,
                    Opt::Filter,
                    Opt::TexFilter,
                    Opt::Msaa,
                    Opt::OledDrift,
                    Opt::OledBrightness,
                ] {
                    self.opt_row(ui, at(Row::Opt(o)), o);
                }
                self.text_row(ui, at(Row::ResetGraphics), "Reset to defaults");
                self.text_row(ui, at(Row::Back), "\u{2039} Back");
            }
            Page::Controls => {
                if let Some(e) = &self.capture_error {
                    ui.colored_label(egui::Color32::LIGHT_RED, e);
                    ui.add_space(2.0);
                }
                let kb = input::keyboard_binding_rows();
                let gp = input::gamepad_binding_rows();
                // (The controller drawing is drawn by `build_ui`, outside
                // this list, so it never scrolls away: U10.)
                ui.strong("Keyboard");
                for (p, i) in ordered_slots(&kb).into_iter().enumerate() {
                    let (name, value) = &kb[i];
                    self.bind_row(
                        ui,
                        p,
                        self.is_capturing(Kind::Keyboard, i)
                            .then_some("press a key\u{2026} (Esc cancels)"),
                        &slot_label(name),
                        &value_labels(value, key_label),
                        slot_action(name),
                    );
                }
                self.text_row(ui, kb.len(), "Reset keyboard to defaults");
                ui.add_space(4.0);
                ui.strong("Controller");
                // U9 status line: which pad the button names below come
                // from.
                match input::active_pad() {
                    Some(pad) => {
                        ui.label(format!("{} connected", pad.name));
                    }
                    None => {
                        ui.weak(
                            egui::RichText::new(
                                "No controller connected. Plug one in: it works right away.",
                            )
                            .size(14.0),
                        );
                    }
                }
                for (p, i) in ordered_slots(&gp).into_iter().enumerate() {
                    let (name, value) = &gp[i];
                    self.bind_row(
                        ui,
                        kb.len() + 1 + p,
                        self.is_capturing(Kind::Gamepad, i)
                            .then_some("press a button\u{2026} (5 s)"),
                        &slot_label(name),
                        &value_labels(value, button_label),
                        slot_action(name),
                    );
                }
                // Not rebindable (the sticks drive them): weak, no row.
                ui.weak(egui::RichText::new("Stick: left stick").size(14.0));
                let family = input::active_pad().map_or(input::PadFamily::Generic, |p| p.family);
                ui.weak(
                    egui::RichText::new(format!(
                        "C buttons: right stick (C Up also {})",
                        input::button_name(Button::North, family)
                    ))
                    .size(14.0),
                );
                self.text_row(ui, kb.len() + 1 + gp.len(), "Reset gamepad to defaults");
                ui.add_space(4.0);
                self.opt_row(ui, at(Row::Opt(Opt::Ble)), Opt::Ble);
                // What BLE is doing right now (redrawn every overlay frame).
                for line in ble_status_lines(crate::ble::locked_by(), &crate::ble::status()) {
                    ui.weak(egui::RichText::new(line).size(14.0));
                }
                ui.add_space(4.0);
                self.text_row(ui, at(Row::Back), "\u{2039} Back");
            }
        }
        // U5: one weak, wrapped help line for the selected row, above the
        // footer.
        ui.add_space(4.0);
        ui.weak(egui::RichText::new(help(rows[self.sel])).size(14.0));
        // Footer (U1): how to navigate, then the version line.
        ui.add_space(6.0);
        ui.weak(egui::RichText::new(FOOTER).size(13.0));
        ui.weak(egui::RichText::new(crate::version_line()).small());
        self.follow_sel = false;
    }

    /// One selectable row: highlight the current selection, support clicks.
    /// Disabled while they do nothing: the Frame-rate row with V-Sync on
    /// (the display paces the ticks), the Resolution row outside an
    /// exclusive fullscreen.
    fn opt_row(&mut self, ui: &mut egui::Ui, i: usize, o: Opt) {
        let disabled = (o == Opt::Fps && self.vsync)
            || (o == Opt::Resolution && self.display_mode_choice() != opts::DisplayMode::Exclusive)
            || (o == Opt::Ble && crate::ble::locked_by().is_some());
        if disabled {
            let (name, value, hint) = self.row_text(o);
            let text = format!("{name}: {value}  {hint}");
            ui.add_enabled_ui(false, |ui| {
                let resp = ui.selectable_label(
                    self.sel == i,
                    egui::RichText::new(text.trim_end()).size(16.0),
                );
                // Hover/clicks on a disabled widget don't fire; the selection
                // still shows where keyboard/pad navigation is, and still
                // scrolls into view (U1).
                if self.sel == i && self.follow_sel {
                    resp.scroll_to_me(Some(egui::Align::Center));
                }
            });
            return;
        }
        // Restart rows get their value in orange while it differs from the
        // running one, and a weak "(restart)" suffix (always, so the label
        // can't be missed).
        let text: egui::WidgetText = match self.restart_row(o, ui.visuals()) {
            Some(t) => t,
            None => {
                let (name, value, hint) = self.row_text(o);
                egui::RichText::new(format!("{name}: {value}  {hint}").trim_end())
                    .size(16.0)
                    .into()
            }
        };
        self.selectable(ui, i, text);
    }

    /// A plain selectable row (Resume / the sub-page rows / Back): same look
    /// as the option rows.
    fn text_row(&mut self, ui: &mut egui::Ui, i: usize, text: &str) {
        self.selectable(ui, i, egui::RichText::new(text).size(16.0).into());
    }

    /// True while this slot is being rebound (its row shows the prompt).
    fn is_capturing(&self, kind: Kind, slot: usize) -> bool {
        self.capture
            .as_ref()
            .is_some_and(|c| c.kind == kind && c.slot == slot)
    }

    /// One Bind row of the Controls page (`i`: the row's index in
    /// `rows()`, `prompt`: the waiting text while this slot is being
    /// rebound): "{slot} {binding}" plus a weak second line saying what
    /// the slot does (U9).
    fn bind_row(
        &mut self,
        ui: &mut egui::Ui,
        i: usize,
        prompt: Option<&'static str>,
        name: &str,
        value: &str,
        action: &'static str,
    ) {
        let value = prompt.unwrap_or(value);
        let text = egui::RichText::new(format!("{name}  {value}"))
            .size(16.0)
            .into();
        self.selectable(ui, i, text);
        // The slot's action, weak and under the row (U9).
        ui.weak(egui::RichText::new(action).size(13.0));
    }

    /// The slot name of the selected Bind row (the pad drawing highlights
    /// it, U10). `None` for the other rows.
    fn selected_slot(&self) -> Option<String> {
        let rows = self.rows();
        match rows.get(self.sel) {
            Some(Row::Bind(kind, i)) => {
                let rows = match kind {
                    Kind::Keyboard => input::keyboard_binding_rows(),
                    Kind::Gamepad => input::gamepad_binding_rows(),
                };
                rows.get(*i).map(|(n, _)| n.to_string())
            }
            _ => None,
        }
    }

    /// Shared selectable-row tail: hover selects (only while the pointer
    /// moves), click selects + confirms. Pad/keyboard navigation scrolls the
    /// selected row back into view (`follow_sel`, U1).
    fn selectable(&mut self, ui: &mut egui::Ui, i: usize, text: egui::WidgetText) {
        let resp = ui.selectable_label(self.sel == i, text);
        // Hover selects only while the mouse moves: a cursor resting over the
        // panel must not snap keyboard / pad navigation back every frame.
        if resp.hovered() && ui.input(|inp| inp.pointer.delta() != egui::Vec2::ZERO) {
            self.sel = i;
        }
        if resp.clicked() {
            self.sel = i;
            self.confirm();
        }
        if self.sel == i && self.follow_sel {
            resp.scroll_to_me(Some(egui::Align::Center));
        }
    }

    /// (label, value, hint) of one row. Hints are gone (U1): restart rows
    /// keep the weak " (restart)" suffix, live rows say nothing. Labels and
    /// values are the player-facing wording (U5).
    fn row_text(&self, o: Opt) -> (&'static str, String, &'static str) {
        let restart = |changed: bool| if changed { "(restart)" } else { "" };
        match o {
            Opt::Msaa => (
                "Anti-aliasing",
                msaa_value(self.msaa),
                restart(self.msaa != self.running.msaa),
            ),
            Opt::Scale => {
                let pct = (self.scale * 100.0).round() as u32;
                (
                    "Render resolution",
                    format!("{pct}% ({}×{})", self.fb.0, self.fb.1),
                    "",
                )
            }
            Opt::Filter => (
                "Scaling filter",
                match self.filter {
                    FilterMode::Linear => "Smooth".into(),
                    FilterMode::Nearest => "Sharp pixels".into(),
                },
                "",
            ),
            Opt::TexFilter => (
                "Texture filter",
                match self.tex_filter {
                    TexFilter::Bilinear => "Smooth".into(),
                    TexFilter::N64 => "N64 original".into(),
                },
                "",
            ),
            Opt::Widescreen => {
                let staged = &self.widescreen[self.widescreen_sel];
                (
                    "Widescreen",
                    staged.clone().unwrap_or_else(|| "Off (4:3)".into()),
                    restart(choice_aspect(staged) != self.running.widescreen),
                )
            }
            Opt::FillScreen => (
                "Fill screen",
                if self.fill_screen { "On" } else { "Off" }.into(),
                restart(self.fill_screen != self.running.fill_screen),
            ),
            Opt::Fps if self.vsync => ("Frame rate", "Set by V-Sync".into(), ""),
            Opt::Fps => {
                let staged = self.fps[self.fps_sel];
                (
                    "Frame rate",
                    match staged {
                        opts::Fps::Monitor => match self.monitor_hz {
                            Some(hz) => format!("Match display ({hz} Hz)"),
                            None => "Match display".into(),
                        },
                        opts::Fps::Hz(n) => format!("{n} fps"),
                        opts::Fps::Uncapped => "Uncapped".into(),
                    },
                    "",
                )
            }
            Opt::Vsync => ("V-Sync", if self.vsync { "On" } else { "Off" }.into(), ""),
            Opt::DisplayMode => (
                "Display mode",
                mode_label(self.display_mode_choice()).into(),
                "",
            ),
            Opt::Resolution if self.display_mode_choice() != opts::DisplayMode::Exclusive => {
                ("Resolution", "Only for exclusive fullscreen".into(), "")
            }
            Opt::Resolution => (
                "Resolution",
                self.res_choice()
                    .map_or_else(|| self.res_desktop.clone(), res_label),
                "",
            ),
            Opt::Volume => (
                "Volume",
                match self.volume {
                    0 => "Muted".into(),
                    v => format!("{}%", v / 10),
                },
                "",
            ),
            Opt::OledDrift => (
                "HUD drift",
                if self.oled_drift { "On" } else { "Off" }.into(),
                "",
            ),
            Opt::OledBrightness => (
                "HUD brightness",
                format!("{}%", (self.oled_brightness * 100.0).round() as u32),
                "",
            ),
            Opt::ShowFps => (
                "Show FPS",
                if self.show_fps { "On" } else { "Off" }.into(),
                "",
            ),
            Opt::PauseBackground => (
                "Pause when in the background",
                if self.pause_background { "On" } else { "Off" }.into(),
                "",
            ),
            Opt::Ble => (
                "Switch 2 controllers (Bluetooth)",
                if self.ble { "On" } else { "Off" }.into(),
                "(experimental)",
            ),
        }
    }

    // --- navigation ---------------------------------------------------------

    /// Restart-only options whose staged value differs from the running one
    /// (row names; the banner list and the Save toast use the same).
    fn pending_restart(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.msaa != self.running.msaa {
            out.push("Anti-aliasing");
        }
        if choice_aspect(&self.widescreen[self.widescreen_sel]) != self.running.widescreen {
            out.push("Widescreen");
        }
        if self.fill_screen != self.running.fill_screen {
            out.push("Fill screen");
        }
        // Live rows staged but not confirmed yet: Save writes them, so a
        // restart applies them.
        if self.display_mode_choice() != self.running_mode {
            out.push("Display mode");
        }
        if self.res_choice() != self.running_res {
            out.push("Resolution");
        }
        out
    }

    /// The restart rows (Anti-aliasing, Widescreen, Fill screen) as rich text:
    /// the value turns orange while the staged value differs from the running
    /// one, and the weak "(restart)" suffix is always there. `None` for the
    /// other rows (plain text, built in `opt_row`).
    fn restart_row(&self, o: Opt, v: &egui::Visuals) -> Option<egui::WidgetText> {
        let (name, value, changed) = match o {
            Opt::Msaa => (
                "Anti-aliasing",
                msaa_value(self.msaa),
                self.msaa != self.running.msaa,
            ),
            Opt::Widescreen => (
                "Widescreen",
                self.widescreen[self.widescreen_sel]
                    .clone()
                    .unwrap_or_else(|| "Off (4:3)".into()),
                choice_aspect(&self.widescreen[self.widescreen_sel]) != self.running.widescreen,
            ),
            Opt::FillScreen => (
                "Fill screen",
                if self.fill_screen { "On" } else { "Off" }.into(),
                self.fill_screen != self.running.fill_screen,
            ),
            _ => return None,
        };
        Some(rich_row(
            name,
            &value,
            changed,
            "(restart)",
            // The plain rows' base color: the inactive widget's text stroke
            // (egui's `text_color()` is the noninteractive gray, which reads
            // dimmer than the selectable rows next to it).
            v.widgets.inactive.fg_stroke.color,
            v.weak_text_color(),
        ))
    }

    fn move_sel(&mut self, d: i32) {
        let n = self.rows().len() as i32;
        self.sel = (self.sel as i32 + d).rem_euclid(n) as usize;
        // Any navigation disarms the Quit row (U3).
        self.quit_armed = None;
        self.follow_sel = true;
    }

    /// A / Enter / click on the selected row: actions run, values cycle.
    fn confirm(&mut self) {
        // While the keep-resolution countdown runs, A confirms it first
        // ("A = keep"); the row's own action still runs (Save + keep is the
        // natural combination).
        self.revert_confirm();
        let rows = self.rows();
        // A click (mouse) can land on any row while a capture runs; only a
        // Bind row replaces the capture, everything else ends it first.
        let capturing = self.capture.is_some();
        let is_bind = matches!(rows[self.sel], Row::Bind(..));
        if capturing && !is_bind {
            self.end_capture();
        }
        // Confirming anything but Quit disarms the Quit row (U3).
        if rows[self.sel] != Row::Quit {
            self.quit_armed = None;
        }
        match rows[self.sel] {
            Row::Opt(o) => match o {
                Opt::DisplayMode => self.apply_display_mode = true,
                Opt::Resolution => self.apply_res = true,
                _ => self.change(1, true),
            },
            Row::Resume => self.show = false,
            Row::Controls => self.enter_page(Page::Controls, Row::Controls),
            Row::Graphics => self.enter_page(Page::Graphics, Row::Graphics),
            // U15: the save/data folder in the OS file manager.
            Row::OpenSaveFolder => crate::paths::open_in_file_manager(crate::paths::data_dir()),
            // U16: the restart banner's row: save, spawn, quit.
            Row::RestartNow => self.restart_now(),
            // Quit (U3): the first confirm arms "Press again to quit" for
            // 3 s, the second one inside the window quits.
            Row::Quit => {
                if self.quit_armed.is_some_and(|t| t.elapsed() < QUIT_ARM) {
                    self.quit_requested = true;
                    self.show = false;
                } else {
                    self.quit_armed = Some(Instant::now());
                }
            }
            // Reset to defaults: stage the built-in defaults and apply the
            // live ones (U2).
            Row::ResetGraphics => self.reset_graphics(),
            // The Back row (sub-pages only; on Main, B / Escape close).
            Row::Back => self.back(),
            // A Bind row: capture the next input for the slot. The row
            // names come straight from the live bindings, so the capture
            // binds what the list shows.
            Row::Bind(kind, i) => {
                let rows = match kind {
                    Kind::Keyboard => input::keyboard_binding_rows(),
                    Kind::Gamepad => input::gamepad_binding_rows(),
                };
                if let Some((name, _)) = rows.get(i) {
                    self.start_capture(kind, i, name);
                }
            }
            // A Reset row: the kind's defaults back.
            Row::Reset(kind) => match kind {
                Kind::Keyboard => input::reset_keyboard(),
                Kind::Gamepad => input::reset_gamepad(),
            },
        }
    }

    /// Steps the selected value by `d` (±1). `wrap`: cycle past the ends
    /// (confirm); otherwise stop there (left / right).
    fn change(&mut self, d: i32, wrap: bool) {
        let rows = self.rows();
        match rows[self.sel] {
            Row::Opt(o) => match o {
                Opt::Msaa => self.msaa = step(&MSAA_STEPS, self.msaa, d, wrap),
                Opt::Scale => self.scale = step(&SCALE_STEPS, self.scale, d, wrap),
                Opt::Filter => {
                    self.filter = match self.filter {
                        FilterMode::Linear => FilterMode::Nearest,
                        FilterMode::Nearest => FilterMode::Linear,
                    }
                }
                Opt::TexFilter => {
                    self.tex_filter = match self.tex_filter {
                        TexFilter::Bilinear => TexFilter::N64,
                        TexFilter::N64 => TexFilter::Bilinear,
                    }
                }
                Opt::Widescreen => {
                    let n = self.widescreen.len() as i32;
                    let old =
                        opts::fill_default(choice_aspect(&self.widescreen[self.widescreen_sel]));
                    self.widescreen_sel = (self.widescreen_sel as i32 + d).rem_euclid(n) as usize;
                    // An explicit choice now: saved as a value (S8).
                    self.widescreen_follow = false;
                    // Fill screen follows widescreen unless the user set it apart
                    // (else Widescreen Off would save a 4:3 fill_screen = true).
                    if self.fill_screen == old {
                        self.fill_screen = opts::fill_default(choice_aspect(
                            &self.widescreen[self.widescreen_sel],
                        ));
                    }
                }
                Opt::Fps if self.vsync => {} // the display paces the ticks
                Opt::Fps => {
                    let n = self.fps.len() as i32;
                    let next = self.fps_sel as i32 + d;
                    self.fps_sel = if wrap {
                        next.rem_euclid(n)
                    } else {
                        next.clamp(0, n - 1)
                    } as usize;
                }
                Opt::DisplayMode => {
                    let n = self.display_choices.len() as i32;
                    let next = self.display_mode_sel as i32 + d;
                    self.display_mode_sel = if wrap {
                        next.rem_euclid(n)
                    } else {
                        next.clamp(0, n - 1)
                    } as usize;
                }
                // The Resolution row is disabled outside an exclusive fullscreen.
                Opt::Resolution if self.display_mode_choice() != opts::DisplayMode::Exclusive => {}
                Opt::Resolution => {
                    let n = self.res_choices.len() as i32;
                    let next = self.res_sel as i32 + d;
                    self.res_sel = if wrap {
                        next.rem_euclid(n)
                    } else {
                        next.clamp(0, n - 1)
                    } as usize;
                }
                Opt::Volume => {
                    self.volume = step(&VOLUME_STEPS, self.volume, d, wrap);
                    audio::set_volume(self.volume as f32 / 1000.0);
                }
                Opt::Vsync => self.vsync = !self.vsync,
                Opt::FillScreen => self.fill_screen = !self.fill_screen,
                Opt::OledDrift => self.oled_drift = !self.oled_drift,
                Opt::ShowFps => self.show_fps = !self.show_fps,
                Opt::PauseBackground => self.pause_background = !self.pause_background,
                // Set by an env var, or no controller input at all: the row
                // is disabled and shows why (`ble_status_lines`).
                Opt::Ble if crate::ble::locked_by().is_some() => {}
                Opt::Ble => {
                    self.ble = !self.ble;
                    // Live, like Volume: the scanner starts / pads drop now.
                    crate::ble::set_enabled(self.ble);
                }
                Opt::OledBrightness => {
                    self.oled_brightness = step(
                        &config::OLED_BRIGHTNESS_STEPS,
                        self.oled_brightness,
                        d,
                        wrap,
                    );
                }
            },
            // The Controls / Back / Quit / "Open save folder" rows have
            // nothing to cycle.
            Row::Controls
            | Row::Back
            | Row::Bind(..)
            | Row::Reset(..)
            | Row::Quit
            | Row::OpenSaveFolder
            | Row::RestartNow => {}
            // Neither do Resume / the page-open rows / Reset to defaults.
            Row::Resume | Row::Graphics | Row::ResetGraphics => {}
        }
    }

    /// Stages the built-in defaults on the Graphics page (ResetGraphics, U2):
    /// the low-end preset is honoured; live ones (scale, filters, fps,
    /// V-Sync, display mode) apply right away, restart ones (MSAA,
    /// widescreen, fill screen) become pending as usual.
    fn reset_graphics(&mut self) {
        self.msaa = 1;
        // S8: the D3 default is the monitor's aspect (Off on a 4:3 one);
        // saved by removing the key, so the file keeps following the display.
        let widescreen = opts::widescreen_default();
        self.widescreen_sel = select_aspect(&mut self.widescreen, widescreen);
        self.widescreen_follow = true;
        self.fill_screen = opts::fill_default(widescreen);
        let fps_default = if opts::low_end() {
            opts::Fps::Hz(60)
        } else {
            opts::Fps::Monitor
        };
        if let Some(i) = self.fps.iter().position(|&f| f == fps_default) {
            self.fps_sel = i;
        }
        self.vsync = false;
        self.scale = if opts::low_end() { 0.75 } else { 1.0 };
        self.filter = FilterMode::Linear;
        self.tex_filter = TexFilter::Bilinear;
        // OLED care (O3): the built-in defaults.
        self.oled_drift = false;
        self.oled_brightness = 1.0;
        // U25: the built-in default (plain title).
        self.show_fps = false;
        // Display mode Window (the built-in default): applying it leaves an
        // exclusive fullscreen, so the resolution goes back to the desktop
        // choice too (staged; applying it would start the keep-it countdown).
        self.display_mode_sel = self
            .display_choices
            .iter()
            .position(|&m| m == opts::DisplayMode::Windowed)
            .unwrap_or(0);
        self.apply_display_mode = true;
        self.res_sel = self
            .res_choices
            .iter()
            .position(|r| r.is_none())
            .unwrap_or(0);
    }

    /// The settings that would be written now (the staged values): written
    /// by close() when they differ from the snapshot taken in `open` (D2).
    fn staged_settings(&self) -> config::SavedSettings {
        config::SavedSettings {
            msaa: Some(self.msaa),
            scale: Some(self.scale),
            scale_filter: Some(match self.filter {
                FilterMode::Linear => "linear",
                FilterMode::Nearest => "nearest",
            }),
            filter: Some(match self.tex_filter {
                TexFilter::Bilinear => "bilinear",
                TexFilter::N64 => "n64",
            }),
            // "" removes the key (S8: follow the monitor default).
            widescreen: Some(if self.widescreen_follow {
                String::new()
            } else {
                self.widescreen[self.widescreen_sel]
                    .clone()
                    .unwrap_or_else(|| "0".into())
            }),
            // The key is removed when it equals the default derived from the
            // staged widescreen choice (renderer.md "Fill screen"), so the
            // file keeps following widescreen.
            fill_screen: Some(save_fill(
                self.fill_screen,
                choice_aspect(&self.widescreen[self.widescreen_sel]),
            )),
            fps: Some(self.fps[self.fps_sel].to_config()),
            vsync: Some(self.vsync),
            display_mode: Some(self.display_mode_choice().as_str()),
            fullscreen_resolution: Some(self.res_choice().map(|r| r.to_config())),
            volume: Some(self.volume as f32 / 1000.0),
            oled_drift: Some(self.oled_drift),
            oled_brightness: Some(self.oled_brightness),
            // U25/U26: the live rows the window keeps (`apply_live`).
            show_fps: Some(self.show_fps),
            pause_in_background: Some(self.pause_background),
            // BLE pads (live). An env-set value can't change here, so it is
            // never saved (`changed_since`).
            ble: Some(self.ble),
            // The rebinding maps as they stand now (empty = defaults, so
            // the sub-table is dropped from the file).
            keyboard: Some(input::keyboard_overrides()),
            gamepad: Some(input::gamepad_overrides()),
        }
    }

    /// Pad navigation: the input thread routes the merged pad here while the
    /// overlay is open (d-pad / left stick move, A confirm, B or Start back).
    /// Skipped while a capture runs: the A that arms it must not confirm
    /// again, and a captured button must not navigate.
    fn pad_nav(&mut self) {
        if self.capture.is_some() {
            return;
        }
        let pad = input::overlay_pad();
        let prev = std::mem::replace(&mut self.prev_pad, pad);
        let edge = |bit: u16| pad.button & bit != 0 && prev.button & bit == 0;
        if edge(input::CONT_A) {
            self.confirm();
        }
        if edge(input::CONT_B) {
            // While the keep-resolution countdown runs, B reverts now
            // instead of closing (the timeout would never fire once the
            // overlay stops redrawing). On the Controls page, B goes back
            // to Main instead of closing.
            if self.revert.is_some() {
                self.pending_revert = self.revert_action();
            } else if self.page != Page::Main {
                // Any sub-page, like Escape (U2).
                self.back();
            } else {
                self.show = false; // back
            }
        }
        if edge(input::CONT_START) {
            self.show = false;
        }
        let h = nav_horizontal(pad);
        if h != 0 && h != nav_horizontal(prev) {
            self.change(h, false);
        }
        // Up/down move with hold repeat.
        let dir = nav_dir(pad);
        let t = Instant::now();
        match dir {
            Some(d) if self.held_dir == Some(d) => {
                if self.next_repeat.is_some_and(|n| t >= n) {
                    self.move_sel(d);
                    self.next_repeat = Some(t + REPEAT_RATE);
                }
            }
            Some(d) => {
                self.move_sel(d);
                self.next_repeat = Some(t + REPEAT_DELAY);
            }
            None => self.next_repeat = None,
        }
        self.held_dir = dir;
    }
}

/// egui input for one frame over the whole surface; `native_ppp` = the
/// window's physical px per point. The UI zoom (U1) lives in the egui
/// context (`set_zoom_factor`), so the screen rect is divided by the native
/// factor only. Shared with the first-run setup screen (firstrun.rs).
pub(crate) fn raw_input_zoom(
    gpu: &Gpu,
    native_ppp: f32,
    time: f64,
    events: Vec<egui::Event>,
) -> egui::RawInput {
    let (w, h) = (gpu.config.width, gpu.config.height);
    let mut raw = egui::RawInput {
        time: Some(time),
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(w as f32 / native_ppp, h as f32 / native_ppp),
        )),
        events,
        ..Default::default()
    };
    // Without this egui assumes 1 px per point: on a HiDPI display the UI
    // would paint into the top-left 1/ppp of the window while the pointer
    // (divided by ppp) hits elsewhere. `zoom_factor` supplies the rest.
    raw.viewports
        .entry(egui::ViewportId::ROOT)
        .or_default()
        .native_pixels_per_point = Some(native_ppp);
    raw
}

/// Paints one egui frame (`out` of `ctx.run`) onto `view` in its own render
/// pass: `load` = `Load` draws over the game frame already there, `Clear`
/// fills the background first. `renderer` is created on first use.
pub(crate) fn paint(
    gpu: &Gpu,
    ctx: &egui::Context,
    renderer: &mut Option<EguiRenderer>,
    out: egui::FullOutput,
    view: &wgpu::TextureView,
    load: wgpu::LoadOp<wgpu::Color>,
) {
    let prims = ctx.tessellate(out.shapes, out.pixels_per_point);
    let renderer = renderer.get_or_insert_with(|| {
        EguiRenderer::new(
            &gpu.device,
            gpu.config.format,
            egui_wgpu::RendererOptions::default(),
        )
    });
    for (id, delta) in &out.textures_delta.set {
        renderer.update_texture(&gpu.device, &gpu.queue, *id, delta);
    }
    let screen = egui_wgpu::ScreenDescriptor {
        size_in_pixels: [gpu.config.width, gpu.config.height],
        pixels_per_point: out.pixels_per_point,
    };
    let mut encoder = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("pw64 egui"),
        });
    renderer.update_buffers(&gpu.device, &gpu.queue, &mut encoder, &prims, &screen);
    let mut pass = encoder
        .begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("pw64 egui"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load,
                    store: wgpu::StoreOp::Store,
                },
                depth_slice: None,
            })],
            ..Default::default()
        })
        .forget_lifetime();
    renderer.render(&mut pass, &prims, &screen);
    drop(pass);
    gpu.queue.submit(Some(encoder.finish()));
    for id in &out.textures_delta.free {
        renderer.free_texture(id);
    }
}

/// Up/down on the pad (d-pad or stick): -1 up, 1 down.
fn nav_dir(p: input::Pad) -> Option<i32> {
    if p.button & input::CONT_UP != 0 || p.stick_y > NAV_STICK {
        Some(-1)
    } else if p.button & input::CONT_DOWN != 0 || p.stick_y < -NAV_STICK {
        Some(1)
    } else {
        None
    }
}

/// Left/right on the pad: -1, 1 or 0.
fn nav_horizontal(p: input::Pad) -> i32 {
    if p.button & input::CONT_LEFT != 0 || p.stick_x < -NAV_STICK {
        -1
    } else if p.button & input::CONT_RIGHT != 0 || p.stick_x > NAV_STICK {
        1
    } else {
        0
    }
}

/// The next step after `cur` in direction `d` (±1). `cur` between steps (a
/// hand-edited config value) moves to the neighbouring step. At the ends:
/// wrap around if `wrap`, else stay.
fn step<T: Copy + PartialOrd>(steps: &[T], cur: T, d: i32, wrap: bool) -> T {
    let next = if d > 0 {
        steps.iter().copied().find(|&s| s > cur)
    } else {
        steps.iter().rev().copied().find(|&s| s < cur)
    };
    match next {
        Some(s) => s,
        None if wrap && d > 0 => steps[0],
        None if wrap => steps[steps.len() - 1],
        None => cur,
    }
}

/// Aspect of one widescreen choice (`None` = off).
fn choice_aspect(c: &Option<String>) -> Option<f32> {
    c.as_deref().and_then(opts::parse_widescreen).flatten()
}

/// Rich row text with a coloured value and a weak suffix, built as a
/// `LayoutJob` because one label can't mix colors. Used by the restart rows
/// (`Overlay::restart_row`).
fn rich_row(
    name: &str,
    value: &str,
    changed: bool,
    suffix: &str,
    base: egui::Color32,
    weak: egui::Color32,
) -> egui::WidgetText {
    use egui::text::{LayoutJob, TextFormat};
    // Same size as the other rows (bug: this was 14.0, which shrank every
    // row that carried the "(restart)" suffix); only the suffix is weak.
    let fmt = |c: egui::Color32| TextFormat::simple(egui::FontId::proportional(16.0), c);
    let mut job = LayoutJob::default();
    job.append(&format!("{name}: "), 0.0, fmt(base));
    job.append(
        value,
        0.0,
        fmt(if changed { egui::Color32::ORANGE } else { base }),
    );
    if !suffix.is_empty() {
        job.append(&format!("  {suffix}"), 0.0, fmt(weak));
    }
    egui::WidgetText::from(job)
}

/// The `fill_screen` value to save: `Some(staged)` unless it equals the
/// default derived from the staged widescreen choice (then None, leaving the
/// key out so the file keeps following widescreen).
fn save_fill(staged: bool, staged_widescreen: Option<f32>) -> Option<bool> {
    (staged != opts::fill_default(staged_widescreen)).then_some(staged)
}

/// The D2 auto-save update: only the fields that differ from the snapshot
/// taken at open (None = leave the key as-is), so values the player never
/// touched (env-var overrides, the low-end preset, hand-edited keys) are not
/// pinned into `pw64.toml`. Fill screen is written whenever widescreen
/// changes too: its saved form depends on the widescreen choice.
fn changed_since(s: config::SavedSettings, snap: &config::SavedSettings) -> config::SavedSettings {
    fn keep<T: PartialEq>(v: Option<T>, old: &Option<T>) -> Option<T> {
        if v == *old { None } else { v }
    }
    let fill_changed = s.fill_screen != snap.fill_screen || s.widescreen != snap.widescreen;
    config::SavedSettings {
        msaa: keep(s.msaa, &snap.msaa),
        scale: keep(s.scale, &snap.scale),
        scale_filter: keep(s.scale_filter, &snap.scale_filter),
        filter: keep(s.filter, &snap.filter),
        widescreen: keep(s.widescreen, &snap.widescreen),
        fps: keep(s.fps, &snap.fps),
        vsync: keep(s.vsync, &snap.vsync),
        fill_screen: if fill_changed { s.fill_screen } else { None },
        display_mode: keep(s.display_mode, &snap.display_mode),
        fullscreen_resolution: keep(s.fullscreen_resolution, &snap.fullscreen_resolution),
        volume: keep(s.volume, &snap.volume),
        oled_drift: keep(s.oled_drift, &snap.oled_drift),
        oled_brightness: keep(s.oled_brightness, &snap.oled_brightness),
        show_fps: keep(s.show_fps, &snap.show_fps),
        pause_in_background: keep(s.pause_in_background, &snap.pause_in_background),
        ble: keep(s.ble, &snap.ble),
        keyboard: keep(s.keyboard, &snap.keyboard),
        gamepad: keep(s.gamepad, &snap.gamepad),
    }
}

/// The U16 restart command: this exe with the original command-line arguments
/// (`argv[0]` dropped), so a restart re-applies how the player started the
/// game. `Err` only when `current_exe` fails (exotic sandboxing).
fn restart_command() -> std::io::Result<std::process::Command> {
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.args(std::env::args_os().skip(1));
    Ok(cmd)
}

/// Widescreen choices — Off, 16:9, 21:9, plus the running aspect if it is a
/// custom one — and the index of the running one.
fn widescreen_choices(running: Option<f32>) -> (Vec<Option<String>>, usize) {
    let mut list = vec![None, Some("16:9".into()), Some("21:9".into())];
    let sel = select_aspect(&mut list, running);
    (list, sel)
}

/// The index of the choice with `aspect` in `list`, appending it as a custom
/// choice when missing (a 16:10 monitor default, a hand-edited ratio).
fn select_aspect(list: &mut Vec<Option<String>>, aspect: Option<f32>) -> usize {
    match list.iter().position(|c| choice_aspect(c) == aspect) {
        Some(i) => i,
        None => {
            // f32 Display round-trips, so saving it keeps the aspect exact.
            list.push(aspect.map(|a| a.to_string()));
            list.len() - 1
        }
    }
}

/// Frame-rate choices ([`FPS_STEPS`] plus the running rate if it's another
/// one) and the index of the running one.
fn fps_choices(running: opts::Fps) -> (Vec<opts::Fps>, usize) {
    let mut list = FPS_STEPS.to_vec();
    let sel = match list.iter().position(|&f| f == running) {
        Some(i) => i,
        None => {
            // Keep the list in rate order (Monitor first, Uncapped last).
            list.push(running);
            list.sort_by_key(|f| match f {
                opts::Fps::Monitor => 0,
                opts::Fps::Hz(n) => *n,
                opts::Fps::Uncapped => u32::MAX,
            });
            list.iter().position(|&f| f == running).unwrap()
        }
    };
    (list, sel)
}

/// The Display mode row's choices: Exclusive only when this window can do
/// it (not Wayland), so cycling skips it there.
fn display_mode_choices(exclusive_ok: bool) -> Vec<opts::DisplayMode> {
    if exclusive_ok {
        vec![
            opts::DisplayMode::Windowed,
            opts::DisplayMode::Borderless,
            opts::DisplayMode::Exclusive,
        ]
    } else {
        vec![opts::DisplayMode::Windowed, opts::DisplayMode::Borderless]
    }
}

/// The Resolution row's choices: the monitor's current mode first
/// (`None`, the "Desktop" choice), then every video mode as a
/// `(w, h, hz)` triple (Hz already rounded), deduped, highest first.
fn resolution_choices(mut modes: Vec<(u32, u32, u32)>) -> Vec<Option<opts::Res>> {
    use std::cmp::Reverse;
    modes.sort_unstable_by_key(|&(w, h, hz)| Reverse((w, h, hz)));
    modes.dedup();
    std::iter::once(None)
        .chain(
            modes
                .into_iter()
                .map(|(w, h, hz)| Some(opts::Res { w, h, hz: Some(hz) })),
        )
        .collect()
}

/// The Display mode row's label for one mode (D4 player-facing names).
fn mode_label(m: opts::DisplayMode) -> &'static str {
    match m {
        opts::DisplayMode::Windowed => "Window",
        opts::DisplayMode::Borderless => "Fullscreen",
        opts::DisplayMode::Exclusive => "Exclusive fullscreen",
    }
}

/// The Anti-aliasing row's value (U5): 1× is shown as "Off".
fn msaa_value(msaa: u32) -> String {
    match msaa {
        1 => "Off".into(),
        n => format!("{n}\u{d7}"),
    }
}

/// The weak status lines under the Switch 2 controllers row: why the row is
/// locked (`ble::locked_by`), else what BLE is doing, with the one thing a
/// beginner has to know (press Sync, don't pair in the OS).
fn ble_status_lines(locked_by: Option<&str>, status: &crate::ble::Status) -> Vec<String> {
    use crate::ble::Status;
    let pair_hint = if cfg!(windows) {
        "Don't pair them in Windows Bluetooth settings: just press Sync."
    } else {
        "Don't pair them in your system's Bluetooth settings: just press Sync."
    };
    let mut lines = Vec::new();
    match locked_by {
        Some("PW64_NO_INPUT") => {
            lines.push("Controller input is turned off (PW64_NO_INPUT).".into());
            return lines;
        }
        Some(var) => lines.push(format!("Set by {var}; change it there.")),
        None => {}
    }
    match status {
        Status::Off => {}
        Status::Starting => lines.push("Starting Bluetooth\u{2026}".into()),
        Status::NoAdapter => lines
            .push("No Bluetooth found on this PC. Turn Bluetooth on, or add an adapter.".into()),
        Status::Searching => {
            lines.push("Searching\u{2026} press the small Sync button on each controller.".into());
            lines.push(pair_hint.into());
        }
        Status::Connected(pads) => lines.push(format!("Connected: {}", pads.join(", "))),
    }
    lines
}

/// The weak help line for one row (U5), shown under the rows for whatever
/// the selection is on. One line per row, jargon-free.
fn help(row: Row) -> &'static str {
    match row {
        Row::Opt(o) => match o {
            Opt::DisplayMode => "Window or fullscreen. F11 or Alt+Enter also switches.",
            Opt::Resolution => {
                "Screen resolution in exclusive fullscreen. Confirm to try it: it switches \
                 back after 10 s unless you keep it."
            }
            Opt::Widescreen => {
                "Shows more of the world on wide screens. Menus stay 4:3. Off = original 4:3 \
                 look. Needs a restart."
            }
            Opt::FillScreen => {
                "Draws flights to the screen edges instead of with the original black bars. \
                 Needs a restart."
            }
            Opt::Fps => {
                "How smoothly the game runs. Match display is best; pick 60 if the game \
                 stutters."
            }
            Opt::Vsync => {
                "Paces frames to your display: removes stutter and tearing, adds a little \
                 input delay."
            }
            Opt::Scale => "Higher is sharper but slower. Below 100% helps slow PCs.",
            Opt::Filter => "How the rendered image is fitted to your screen when it isn't 100%.",
            Opt::TexFilter => {
                "Smooth is the modern look; N64 original copies the console's texture \
                 filtering."
            }
            Opt::Msaa => {
                "Smooths jagged edges. 4\u{d7} is a good choice; 8\u{d7} needs a strong \
                 graphics card. Needs a restart."
            }
            Opt::Volume => "Music and sound effects.",
            Opt::OledDrift => {
                "Slowly moves the flight HUD a few pixels so an OLED screen wears evenly. \
                 Leave off on other screens."
            }
            Opt::OledBrightness => {
                "Dims the flight HUD so an OLED screen wears less. Full brightness is the \
                 original look."
            }
            Opt::ShowFps => "Shows the frame rate in the window title.",
            Opt::PauseBackground => {
                "Pauses the game when you switch to another window or your last controller \
                 disconnects."
            }
            Opt::Ble => {
                "Play with Switch 2 Joy-Con or a Pro Controller 2 over Bluetooth. Experimental: \
                 may not work with every PC."
            }
        },
        Row::Bind(..) => "Press a key or button to bind it to this slot.",
        Row::Reset(..) => "Restores this section's default keys and buttons.",
        Row::Resume => "Back to the game.",
        Row::Graphics => "Fullscreen, widescreen, frame rate and picture quality.",
        Row::ResetGraphics => "Puts every option on this page back to its default.",
        Row::Controls => "See and change keyboard and controller buttons.",
        Row::Back => "Back to the previous page.",
        Row::Quit => "Closes Birdman64. Progress since the game last saved is lost.",
        Row::OpenSaveFolder => {
            "Your save (pw64.eep), settings and crash reports. Copy pw64.eep to back up \
             your progress."
        }
        Row::RestartNow => "Saves your changes and starts Birdman64 again so they take effect.",
    }
}

/// The Resolution row's label for one video mode.
fn res_label(r: opts::Res) -> String {
    match r.hz {
        Some(hz) => format!("{}×{} @ {} Hz", r.w, r.h, hz),
        None => format!("{}×{}", r.w, r.h),
    }
}

// The Controls page reads like a manual, not a config file: slots, keys and
// pad buttons in player-facing wording. Display only: config files keep the
// code names (`C_UP`, `KeyP`, `South`).

/// The Controls page's Bind-row order within a section (U9): the game's
/// controls first (A, B, Z, R, Start, C buttons, stick), the slots the game
/// never uses (L, D-pad) last. A stable sort, so same-rank rows keep the
/// slot-table order (Stick Up, Down, Left, Right).
fn slot_rank(name: &str) -> u8 {
    match name {
        "A" => 0,
        "B" => 1,
        "Z" => 2,
        "R" => 3,
        "START" => 4,
        "C_UP" => 5,
        "C_DOWN" => 6,
        "C_LEFT" => 7,
        "C_RIGHT" => 8,
        "STICK_UP" | "STICK_DOWN" | "STICK_LEFT" | "STICK_RIGHT" => 9,
        _ => 10, // L and the D-pad: not used by the game
    }
}

/// Indices of `rows` in Controls-page order ([`slot_rank`]).
fn ordered_slots(rows: &[(&'static str, String)]) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..rows.len()).collect();
    idx.sort_by_key(|&i| slot_rank(rows[i].0));
    idx
}

/// What the slot does in the game (U9; input.md "What each N64 input does"),
/// shown as a weak line under each Bind row. Code slot names, as in
/// `KEYBOARD_SLOTS` / `GAMEPAD_SLOTS`.
fn slot_action(slot: &str) -> &'static str {
    match slot {
        "A" => "Confirm · thrust, flap, jump, fire",
        "B" => "Back · gentle thrust, parachute",
        "Z" => "Hold to aim camera/missile, release to shoot",
        "R" => "Change camera view",
        "START" => "Pause · confirm",
        "C_UP" | "C_DOWN" | "C_LEFT" | "C_RIGHT" => "Look around",
        "STICK_UP" | "STICK_DOWN" | "STICK_LEFT" | "STICK_RIGHT" => "Steer · move in menus",
        "L" | "UP" | "DOWN" | "LEFT" | "RIGHT" => "Not used in this game",
        _ => "",
    }
}

/// A binding slot's player-facing label (`START` → "Start"). `A`, `B`, `Z`,
/// `L`, `R` read fine as is.
fn slot_label(code: &str) -> String {
    match code {
        "START" => "Start".into(),
        "C_UP" => "C Up".into(),
        "C_DOWN" => "C Down".into(),
        "C_LEFT" => "C Left".into(),
        "C_RIGHT" => "C Right".into(),
        "UP" => "D-Pad Up".into(),
        "DOWN" => "D-Pad Down".into(),
        "LEFT" => "D-Pad Left".into(),
        "RIGHT" => "D-Pad Right".into(),
        "STICK_UP" => "Stick Up".into(),
        "STICK_DOWN" => "Stick Down".into(),
        "STICK_LEFT" => "Stick Left".into(),
        "STICK_RIGHT" => "Stick Right".into(),
        other => other.to_string(),
    }
}

/// One key's player-facing name from its winit Debug name: strip the
/// `Key`/`Digit` prefix ("KeyP" → "P", "Digit5" → "5"), move a modifier's
/// Left/Right side to the front ("ShiftLeft" → "Left Shift"), else the raw
/// name with readable spacing ("ArrowUp" → "Arrow Up", "Numpad5" → "Num 5").
fn key_label(code: &str) -> String {
    let base = code
        .strip_suffix("Left")
        .or_else(|| code.strip_suffix("Right"));
    if let Some(base) = base
        && matches!(base, "Shift" | "Control" | "Alt" | "Super")
    {
        // Arrow keys end in Left/Right too: only modifiers move the side.
        let side = &code[base.len()..];
        let word = if base == "Control" { "Ctrl" } else { base };
        return format!("{side} {word}");
    }
    if let Some(d) = code.strip_prefix("Numpad")
        && d.len() == 1
        && d.chars().all(|c| c.is_ascii_digit())
    {
        return format!("Num {d}");
    }
    if let Some(w) = code
        .strip_prefix("Key")
        .or_else(|| code.strip_prefix("Digit"))
    {
        return w.to_string();
    }
    camel(code)
}

/// One pad button's player-facing name (U6): the active pad's family
/// labels via `input::button_name` (`Generic` = the Xbox names, D5); a
/// name that doesn't parse (gilrs extras) keeps readable spacing.
fn button_label(name: &str) -> String {
    let family = input::active_pad().map_or(input::PadFamily::Generic, |p| p.family);
    match input::parse_gamepad_button(name) {
        Some(b) => input::button_name(b, family).into(),
        None => camel(name),
    }
}

/// Labels every " / " part of a binding row's value ("KeyW / ArrowUp" →
/// "W / Arrow Up").
fn value_labels(value: &str, one: fn(&str) -> String) -> String {
    value.split(" / ").map(one).collect::<Vec<_>>().join(" / ")
}

/// The first-launch welcome card (U8): title + lines for
/// `Toast::show_card`. The keys and buttons come from the live bindings,
/// so rebinds show.
pub(crate) fn welcome_card() -> (String, Vec<String>) {
    let keyboard = |slot: &str| {
        input::keyboard_binding_rows()
            .iter()
            .find(|(name, _)| *name == slot)
            .map(|(_, value)| value_labels(value, key_label))
            .unwrap_or_else(|| "?".into())
    };
    let mut lines = Vec::new();
    // With a pad: name it and say which of its buttons is Z (U6 names).
    if let Some(pad) = input::active_pad() {
        let z = input::gamepad_binding_rows()
            .iter()
            .find(|(name, _)| *name == "Z")
            .map(|(_, value)| value_labels(value, button_label))
            .unwrap_or_else(|| "?".into());
        lines.push(format!(
            "Controller: {}. It works like an N64 controller: Z is {z}.",
            pad.name
        ));
    }
    lines.push(format!(
        "Keyboard: move with W A S D \u{b7} A = {} \u{b7} B = {} \u{b7} Z = {} \u{b7} Start = {}",
        keyboard("A"),
        keyboard("B"),
        keyboard("Z"),
        keyboard("START")
    ));
    lines.push("C buttons (camera): I J K L \u{b7} F11: fullscreen".to_string());
    // The settings triggers, as configured (the pad button named family-style).
    let family = input::active_pad().map_or(input::PadFamily::Generic, |p| p.family);
    let f10 = key_label(config::get().input.settings.key.as_deref().unwrap_or("F10"));
    lines.push(format!(
        "Esc, {f10} or {}: settings, controls and quit",
        input::button_name(pad_button(), family)
    ));
    ("Welcome to Birdman64".to_string(), lines)
}

/// Splits camel case for display: "ArrowUp" → "Arrow Up", "F10" stays.
fn camel(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    let mut chars = name.chars().peekable();
    while let Some(c) = chars.next() {
        out.push(c);
        if let Some(&next) = chars.peek()
            && c.is_ascii_lowercase()
            && next.is_ascii_uppercase()
        {
            out.push(' ');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths;

    #[test]
    fn key_names_are_player_facing() {
        // Prefixes stripped.
        assert_eq!(key_label("KeyP"), "P");
        assert_eq!(key_label("Digit5"), "5");
        // Modifiers: the side moves in front, Control abbreviates.
        assert_eq!(key_label("ShiftLeft"), "Left Shift");
        assert_eq!(key_label("ShiftRight"), "Right Shift");
        assert_eq!(key_label("ControlLeft"), "Left Ctrl");
        assert_eq!(key_label("ControlRight"), "Right Ctrl");
        assert_eq!(key_label("AltRight"), "Right Alt");
        // Raw-name fallback with readable spacing; digits don't split.
        assert_eq!(key_label("Enter"), "Enter");
        assert_eq!(key_label("Space"), "Space");
        assert_eq!(key_label("ArrowUp"), "Arrow Up");
        assert_eq!(key_label("ArrowLeft"), "Arrow Left");
        assert_eq!(key_label("PageUp"), "Page Up");
        assert_eq!(key_label("CapsLock"), "Caps Lock");
        assert_eq!(key_label("F10"), "F10");
        assert_eq!(key_label("Numpad5"), "Num 5");
        assert_eq!(key_label("NumpadEnter"), "Numpad Enter");
        assert_eq!(key_label("Backquote"), "Backquote");
    }

    /// The welcome card (U8): always the keyboard, C-button and settings
    /// lines; with the default bindings the keys read player-style.
    #[test]
    fn welcome_card_lines() {
        let (title, lines) = welcome_card();
        assert_eq!(title, "Welcome to Birdman64");
        let find = |start: &str| {
            lines
                .iter()
                .find(|l| l.starts_with(start))
                .unwrap_or_else(|| panic!("no line starting {start:?}: {lines:?}"))
        };
        // No pad in tests: the keyboard line carries the defaults.
        let kb = find("Keyboard: ");
        assert!(kb.contains("A = Space"), "{kb}");
        assert!(kb.contains("B = Left Shift"), "{kb}");
        assert!(kb.contains("Start = Enter"), "{kb}");
        assert_eq!(
            find("C buttons (camera): "),
            "C buttons (camera): I J K L \u{b7} F11: fullscreen"
        );
        assert!(find("Esc, ").ends_with(": settings, controls and quit"));
    }

    #[test]
    fn controls_rows_are_labelled() {
        // A slot's value lists its keys with " / ".
        assert_eq!(value_labels("KeyW / ArrowUp", key_label), "W / Arrow Up");
        assert_eq!(value_labels("(unbound)", key_label), "(unbound)");
        // Pad buttons are named for the active pad's family (Generic: the
        // Xbox names, D5).
        assert_eq!(value_labels("South / East", button_label), "A / B");
        assert_eq!(slot_label("C_UP"), "C Up");
        assert_eq!(slot_label("START"), "Start");
        assert_eq!(slot_label("A"), "A");
        assert_eq!(button_label("South"), "A");
        assert_eq!(button_label("LeftTrigger2"), "LT");
        assert_eq!(button_label("DPadUp"), "D-pad up");
    }

    /// Every slot of both tables has an action line, and the wording is the
    /// decided one (U9).
    #[test]
    fn every_slot_has_an_action() {
        for (_, slot, _) in input::KEYBOARD_SLOTS {
            assert!(!slot_action(slot).is_empty(), "{slot} has no action");
        }
        for (_, slot, _) in input::GAMEPAD_SLOTS {
            assert!(!slot_action(slot).is_empty(), "{slot} has no action");
        }
        assert_eq!(slot_action("A"), "Confirm · thrust, flap, jump, fire");
        assert_eq!(slot_action("B"), "Back · gentle thrust, parachute");
        assert_eq!(
            slot_action("Z"),
            "Hold to aim camera/missile, release to shoot"
        );
        assert_eq!(slot_action("R"), "Change camera view");
        assert_eq!(slot_action("START"), "Pause · confirm");
        assert_eq!(slot_action("C_LEFT"), "Look around");
        assert_eq!(slot_action("STICK_RIGHT"), "Steer · move in menus");
        assert_eq!(slot_action("L"), "Not used in this game");
        assert_eq!(slot_action("UP"), "Not used in this game");
    }

    #[test]
    fn fps_choices_select_running() {
        let (l, i) = fps_choices(opts::Fps::Monitor);
        assert_eq!((l.len(), i), (8, 0));
        let (l, i) = fps_choices(opts::Fps::Hz(180));
        assert_eq!(l.len(), 9);
        assert_eq!(l[i], opts::Fps::Hz(180));
        assert_eq!(l[i - 1], opts::Fps::Hz(165));
        assert_eq!(l[i + 1], opts::Fps::Hz(240));
    }

    #[test]
    fn step_wraps_or_clamps() {
        assert_eq!(step(&MSAA_STEPS, 1, 1, true), 4);
        assert_eq!(step(&MSAA_STEPS, 8, 1, true), 1);
        assert_eq!(step(&MSAA_STEPS, 8, 1, false), 8);
        assert_eq!(step(&MSAA_STEPS, 1, -1, false), 1);
        assert_eq!(step(&MSAA_STEPS, 1, -1, true), 8);
        // Off-grid values (hand-edited config) reach the neighbours; the old
        // exact-match table got stuck on them.
        assert_eq!(step(&SCALE_STEPS, 2.5, 1, false), 3.0);
        assert_eq!(step(&SCALE_STEPS, 2.5, -1, false), 2.0);
        assert_eq!(step(&VOLUME_STEPS, 350, 1, true), 400);
        assert_eq!(step(&VOLUME_STEPS, 1000, 1, true), 0);
    }

    #[test]
    fn widescreen_choices_select_running() {
        let (l, i) = widescreen_choices(None);
        assert_eq!((l.len(), i), (3, 0));
        let (l, i) = widescreen_choices(Some(16.0 / 9.0));
        assert_eq!(l[i].as_deref(), Some("16:9"));
        let (l, i) = widescreen_choices(Some(2.33));
        assert_eq!((l.len(), i), (4, 3));
        assert_eq!(choice_aspect(&l[i]), Some(2.33));
    }

    #[test]
    fn display_choices_skip_exclusive_when_unavailable() {
        use opts::DisplayMode::{Borderless, Exclusive, Windowed};
        assert_eq!(
            display_mode_choices(true),
            vec![Windowed, Borderless, Exclusive]
        );
        // No exclusive on Wayland: the cycle skips it.
        assert_eq!(display_mode_choices(false), vec![Windowed, Borderless]);
    }

    #[test]
    fn resolution_choices_desktop_first_highest_first() {
        let c = resolution_choices(vec![
            (1920, 1080, 60),
            (1280, 720, 60),
            (1920, 1080, 144),
            (1920, 1080, 60), // deduped
            (640, 480, 59),
        ]);
        assert_eq!(c[0], None, "the desktop choice first");
        let res = |w, h, hz| Some(opts::Res { w, h, hz: Some(hz) });
        assert_eq!(c[1], res(1920, 1080, 144));
        assert_eq!(c[2], res(1920, 1080, 60));
        assert_eq!(c[3], res(1280, 720, 60));
        assert_eq!(c[4], res(640, 480, 59));
        assert_eq!(c.len(), 5);
        // Empty monitor (none detected): just the desktop choice.
        assert_eq!(resolution_choices(Vec::new()), vec![None]);
    }

    #[test]
    fn exclusive_revert_state_machine() {
        let mut o = Overlay::new();
        let res = opts::Res {
            w: 640,
            h: 480,
            hz: Some(59),
        };
        // Not yet due: nothing happens.
        o.revert = Some(Revert {
            mode: opts::DisplayMode::Borderless,
            res: Some(res),
            deadline: Instant::now() + Duration::from_secs(10),
        });
        assert_eq!(o.revert_check(Instant::now()), None);
        // A = keep: the pending state is dropped.
        o.revert_confirm();
        assert!(o.revert.is_none());
        // B reverts right away: the previous mode + resolution.
        o.revert = Some(Revert {
            mode: opts::DisplayMode::Windowed,
            res: None,
            deadline: Instant::now() + Duration::from_secs(10),
        });
        assert_eq!(o.revert_action(), Some((opts::DisplayMode::Windowed, None)));
        // Past the deadline (checked each overlay frame): revert.
        o.revert = Some(Revert {
            mode: opts::DisplayMode::Borderless,
            res: Some(res),
            deadline: Instant::now(),
        });
        assert_eq!(
            o.revert_check(Instant::now() + Duration::from_secs(1)),
            Some((opts::DisplayMode::Borderless, Some(res)))
        );
    }

    #[test]
    fn fill_screen_saves_only_when_off_default() {
        // Default (follows widescreen) → the key is left out.
        assert_eq!(save_fill(true, Some(16.0 / 9.0)), None);
        assert_eq!(save_fill(false, None), None);
        // Diverging from the default → saved.
        assert_eq!(save_fill(false, Some(16.0 / 9.0)), Some(false));
        assert_eq!(save_fill(true, None), Some(true));
        // The row order keeps Fill screen right after Widescreen.
        let mut o = Overlay::new();
        o.page = Page::Graphics;
        let rows = o.rows();
        let i_w = rows
            .iter()
            .position(|r| *r == Row::Opt(Opt::Widescreen))
            .unwrap();
        assert_eq!(rows[i_w + 1], Row::Opt(Opt::FillScreen));
    }

    /// D2 auto-save writes only what changed since open; a widescreen
    /// change also writes fill screen (its saved form depends on it).
    #[test]
    fn auto_save_writes_only_changes() {
        let snap = Overlay::new().staged_settings();
        let mut s = snap.clone();
        s.volume = Some(0.35);
        let d = changed_since(s, &snap);
        assert_eq!(d.volume, Some(0.35));
        assert_eq!(
            d,
            config::SavedSettings {
                volume: Some(0.35),
                ..Default::default()
            },
            "nothing else is written"
        );
        let mut s = snap.clone();
        s.widescreen = Some("21:9".into());
        let d = changed_since(s, &snap);
        assert_eq!(d.widescreen.as_deref(), Some("21:9"));
        assert_eq!(d.fill_screen, snap.fill_screen, "fill follows widescreen");
    }

    /// U18 chain: values the player reset to the built-in defaults reach
    /// `config::apply` as removals, so a later default change (the low-end
    /// preset, the monitor's rate) still applies; only real choices are
    /// pinned.
    #[test]
    fn reset_defaults_are_removed_not_pinned() {
        let snap = config::SavedSettings {
            msaa: Some(4),
            scale: Some(2.0),
            scale_filter: Some("nearest"),
            filter: Some("n64"),
            widescreen: Some("16:9".into()),
            fps: Some("60".into()),
            vsync: Some(true),
            fill_screen: Some(Some(true)),
            display_mode: Some("exclusive"),
            fullscreen_resolution: Some(Some("640x480".into())),
            volume: Some(0.35),
            oled_drift: Some(true),
            oled_brightness: Some(0.7),
            show_fps: Some(true),
            pause_in_background: Some(false),
            ble: Some(true),
            keyboard: Some(
                [("A".to_string(), "KeyX".to_string())]
                    .into_iter()
                    .collect(),
            ),
            gamepad: Some(
                [("A".to_string(), "East".to_string())]
                    .into_iter()
                    .collect(),
            ),
        };
        // Everything back to the built-in defaults ("Reset to defaults").
        let staged = config::SavedSettings {
            msaa: Some(1),
            scale: Some(1.0),
            scale_filter: Some("linear"),
            filter: Some("bilinear"),
            widescreen: Some(String::new()),
            fps: Some("monitor".into()),
            vsync: Some(false),
            fill_screen: Some(None),
            display_mode: Some("windowed"),
            fullscreen_resolution: Some(None),
            volume: Some(1.0),
            oled_drift: Some(false),
            oled_brightness: Some(1.0),
            show_fps: Some(false),
            pause_in_background: Some(true),
            ble: Some(false),
            keyboard: Some(std::collections::BTreeMap::new()),
            gamepad: Some(std::collections::BTreeMap::new()),
        };
        let d = changed_since(staged, &snap);
        let mut table: toml::Table = toml::from_str(
            "[graphics]\nmsaa = 4\nscale = 2.0\nscale_filter = \"nearest\"\nfilter = \"n64\"\n\
             widescreen = \"16:9\"\nfps = 60\nvsync = true\nfill_screen = true\n\
             display_mode = \"exclusive\"\nfullscreen_resolution = \"640x480\"\nvolume = 0.35\n\
             show_fps = true\n\
             \n[oled]\ndrift = true\nbrightness = 0.7\n\
             \n[ui]\npause_in_background = false\n\
             \n[input.keyboard]\nA = \"KeyX\"\n\n[input.gamepad]\nA = \"East\"\n\
             \n[input.ble]\nenabled = true\n",
        )
        .unwrap();
        config::apply(&mut table, &d, false).unwrap();
        let g = &table["graphics"];
        for k in [
            "msaa",
            "scale",
            "scale_filter",
            "filter",
            "widescreen",
            "fps",
            "vsync",
            "fill_screen",
            "display_mode",
            "fullscreen_resolution",
            "volume",
            "show_fps",
        ] {
            assert!(g.get(k).is_none(), "{k}: the default removes the key");
        }
        assert!(table["oled"].get("drift").is_none());
        assert!(table["oled"].get("brightness").is_none());
        // U26: the pause setting's default is removed from [ui] too.
        assert!(table["ui"].get("pause_in_background").is_none());
        let i = &table["input"];
        assert!(i.get("keyboard").is_none());
        assert!(i.get("gamepad").is_none());
        // BLE off (the default) removes the key and its sub-table.
        assert!(i.get("ble").is_none());
    }

    /// A reverted exclusive resolution is re-staged, so the auto-save does
    /// not write the rejected mode.
    #[test]
    fn revert_restages_display_rows() {
        let mut o = Overlay::new();
        let res = opts::Res {
            w: 640,
            h: 480,
            hz: Some(60),
        };
        o.res_choices = vec![None, Some(res)];
        o.res_sel = 1;
        o.display_mode_sel = 2; // Exclusive
        o.restage_display(opts::DisplayMode::Windowed, None);
        assert_eq!(o.display_mode_choice(), opts::DisplayMode::Windowed);
        assert_eq!(o.res_choice(), None);
    }

    #[test]
    fn ui_zoom_scales_with_window_height() {
        // 720 px at the standard density: the old layout, no zoom.
        assert_eq!(ui_zoom(720, 1.0), 1.0);
        // 1440 px doubles the text height.
        assert_eq!(ui_zoom(1440, 1.0), 2.0);
        // The clamp keeps absurdly tall windows sane.
        assert_eq!(ui_zoom(4320, 1.0), 2.5);
        // HiDPI: the zoom works in egui points, the native scale is
        // separate (Windows at 200 %: 1440 px is only 720 points).
        assert_eq!(ui_zoom(1440, 2.0), 1.0);
    }

    #[test]
    fn rows_pages_and_controls_row() {
        let mut o = Overlay::new();
        // Main: Resume, the two sub-pages, Volume, the background-pause
        // row, Open save folder, Quit (U2, U3, U15, U26).
        let rows = o.rows();
        assert_eq!(rows.len(), 7);
        assert_eq!(rows[0], Row::Resume);
        assert_eq!(rows[1], Row::Controls);
        assert_eq!(rows[2], Row::Graphics);
        assert_eq!(rows[3], Row::Opt(Opt::Volume));
        assert_eq!(rows[4], Row::Opt(Opt::PauseBackground));
        assert_eq!(rows[5], Row::OpenSaveFolder);
        assert_eq!(rows[6], Row::Quit);
        // Confirm on Display & graphics opens the Graphics page: the
        // eleven graphics options (incl. Show FPS, U25), the two OLED care
        // rows, Reset to defaults, and Back (no restart-pending change yet,
        // so no banner row, U16).
        o.sel = 2;
        o.confirm();
        assert_eq!(o.page, Page::Graphics);
        let rows = o.rows();
        assert_eq!(rows.len(), 15);
        assert_eq!(rows[0], Row::Opt(Opt::DisplayMode));
        assert_eq!(rows[6], Row::Opt(Opt::ShowFps));
        assert_eq!(rows[10], Row::Opt(Opt::Msaa));
        assert_eq!(rows[11], Row::Opt(Opt::OledDrift));
        assert_eq!(rows[12], Row::Opt(Opt::OledBrightness));
        assert_eq!(rows[13], Row::ResetGraphics);
        assert_eq!(rows[14], Row::Back);
        // Stage a restart-only change: the banner's "Restart now" row (U16)
        // appears first, right under the banner.
        o.msaa = 4;
        let rows = o.rows();
        assert_eq!(rows.len(), 16);
        assert_eq!(rows[0], Row::RestartNow);
        // Back (B / Escape / the Back row) returns to Main, selection on
        // the row that opened it.
        o.back();
        assert_eq!(o.page, Page::Main);
        assert_eq!(o.rows()[o.sel], Row::Graphics);
        // Confirm on Controls opens the Controls page with the two binding
        // grids in the U9 order (game controls first, unused slots last:
        // 18 keyboard + reset, 11 gamepad + reset), the Switch 2 (BLE)
        // row and Back.
        o.sel = 1;
        o.confirm();
        assert_eq!(o.page, Page::Controls);
        let rows = o.rows();
        assert_eq!(rows.len(), 18 + 1 + 11 + 1 + 1 + 1);
        assert_eq!(rows[0], Row::Bind(Kind::Keyboard, 0));
        // The unused keyboard slots (L, the D-pad) moved last, after the
        // stick rows.
        assert_eq!(rows[9], Row::Bind(Kind::Keyboard, 14));
        assert_eq!(rows[13], Row::Bind(Kind::Keyboard, 3));
        assert_eq!(rows[17], Row::Bind(Kind::Keyboard, 13));
        assert_eq!(rows[18], Row::Reset(Kind::Keyboard));
        assert_eq!(rows[19], Row::Bind(Kind::Gamepad, 0));
        assert_eq!(rows[25], Row::Bind(Kind::Gamepad, 3));
        assert_eq!(rows[29], Row::Bind(Kind::Gamepad, 10));
        assert_eq!(rows[30], Row::Reset(Kind::Gamepad));
        assert_eq!(rows[31], Row::Opt(Opt::Ble));
        assert_eq!(rows[32], Row::Back);
        o.sel = 32;
        o.confirm();
        assert_eq!(o.page, Page::Main);
        assert_eq!(o.rows()[o.sel], Row::Controls);
    }

    /// The Switch 2 controllers row: reachable by navigation (it sits
    /// between the gamepad reset and Back), player-facing wording, and
    /// locked without the input thread (`ble::init` never runs in tests,
    /// like `PW64_NO_INPUT`): confirm/left/right change nothing and
    /// nothing gets saved.
    #[test]
    fn ble_row_navigation_and_lock() {
        let mut o = Overlay::new();
        o.page = Page::Controls;
        let rows = o.rows();
        let back = rows.iter().position(|&r| r == Row::Back).unwrap();
        o.sel = back;
        o.move_sel(-1);
        assert_eq!(o.rows()[o.sel], Row::Opt(Opt::Ble));
        o.move_sel(-1);
        assert_eq!(o.rows()[o.sel], Row::Reset(Kind::Gamepad));
        o.move_sel(1);
        let (name, value, hint) = o.row_text(Opt::Ble);
        assert_eq!(
            (name, value.as_str(), hint),
            ("Switch 2 controllers (Bluetooth)", "Off", "(experimental)")
        );
        let snap = o.staged_settings();
        o.confirm();
        o.change(1, false);
        assert!(!o.ble, "locked row doesn't toggle");
        assert!(!crate::ble::is_on());
        assert_eq!(changed_since(o.staged_settings(), &snap).ble, None);
        // Unlocked (simulated): a toggle is saved, toggling back is not.
        o.ble = true;
        assert_eq!(changed_since(o.staged_settings(), &snap).ble, Some(true));
    }

    /// The status lines under the BLE row.
    #[test]
    fn ble_status_lines_say_what_to_do() {
        use crate::ble::Status;
        assert!(ble_status_lines(None, &Status::Off).is_empty());
        let s = ble_status_lines(None, &Status::Searching);
        assert!(s[0].contains("Sync"), "{s:?}");
        assert!(s[1].contains("Don't pair"), "{s:?}");
        assert_eq!(
            ble_status_lines(
                None,
                &Status::Connected(vec!["Joy-Con 2 (L)", "Joy-Con 2 (R)"])
            ),
            vec!["Connected: Joy-Con 2 (L), Joy-Con 2 (R)".to_string()]
        );
        assert!(ble_status_lines(None, &Status::NoAdapter)[0].contains("No Bluetooth"));
        let s = ble_status_lines(Some("PW64_BLE"), &Status::Searching);
        assert!(s[0].contains("PW64_BLE") && s.len() == 3, "{s:?}");
        let s = ble_status_lines(Some("PW64_NO_INPUT"), &Status::Off);
        assert_eq!(s.len(), 1);
        assert!(s[0].contains("PW64_NO_INPUT"));
    }

    /// close() auto-save (D2) touches the process-wide data dir, so the two
    /// tests serialize on this (the first one to run claims it via
    /// `paths::set_data_dir`, which only works before the first
    /// `data_dir()` call).
    static DATA_DIR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn close_with_changes_saves() {
        let _g = DATA_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("pw64-test-save-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        paths::set_data_dir(tmp);
        let file = paths::data_dir().join("pw64.toml");
        let _ = std::fs::remove_file(&file);

        let mut o = Overlay::new();
        o.volume = 350; // anything but the staged value at open
        o.close();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(
            text.contains("volume = 0.35"),
            "the changed volume was written: {text}"
        );
    }

    #[test]
    fn close_without_changes_writes_nothing() {
        let _g = DATA_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("pw64-test-quiet-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        paths::set_data_dir(tmp);
        let file = paths::data_dir().join("pw64.toml");
        let before = std::fs::metadata(&file).ok().map(|m| m.modified().unwrap());

        let mut o = Overlay::new();
        // Mimics open(): the snapshot is what the screen shows right now.
        o.snapshot = o.staged_settings();
        o.close();
        let after = std::fs::metadata(&file).ok().map(|m| m.modified().unwrap());
        assert_eq!(
            before, after,
            "no staged change since open: pw64.toml untouched"
        );
    }

    /// The Controls capture state machine: a Bind row confirms into a
    /// capture, Escape cancels, a reserved key is rejected with a red line
    /// and the capture keeps waiting (no global binding state is touched:
    /// the bind itself is covered by `input.rs`).
    #[test]
    fn controls_capture_state_machine() {
        let mut o = Overlay::new();
        o.page = Page::Controls;
        // Confirm on the first keyboard Bind row (A) arms a capture.
        o.sel = 0;
        o.confirm();
        assert!(matches!(
            o.capture,
            Some(Capture {
                kind: Kind::Keyboard,
                slot: 0,
                ..
            })
        ));
        // A reserved key (F11; the settings key never reaches `on_key` in a
        // real run) is rejected with a red line, capture continues.
        o.on_key(KeyCode::F11, false, true);
        assert!(o.capture.is_some());
        assert!(o.capture_error.is_some());
        // Escape cancels.
        o.on_key(KeyCode::Escape, false, true);
        assert!(o.capture.is_none());
        assert!(o.capture_error.is_none());
        // A gamepad capture cancels too (disarming the pad capture).
        o.sel = 0;
        o.capture = Some(Capture {
            kind: Kind::Gamepad,
            slot: 3,
            name: "A",
            started: Instant::now(),
        });
        o.on_key(KeyCode::Escape, false, true);
        assert!(o.capture.is_none());
    }

    #[test]
    fn pad_nav_directions() {
        let p = |button, stick_x, stick_y| input::Pad {
            button,
            stick_x,
            stick_y,
        };
        assert_eq!(nav_dir(p(input::CONT_UP, 0, 0)), Some(-1));
        assert_eq!(nav_dir(p(0, 0, -60)), Some(1));
        assert_eq!(nav_dir(p(0, 0, 20)), None);
        assert_eq!(nav_horizontal(p(0, -60, 0)), -1);
        assert_eq!(nav_horizontal(p(input::CONT_RIGHT, 0, 0)), 1);
    }

    /// The Quit row (U3): the first confirm arms, any other row or
    /// navigation disarms, an expired arm re-arms on the next confirm, and
    /// a second confirm inside the 3 s window quits.
    #[test]
    fn quit_row_arms_disarms_and_quits() {
        let mut o = Overlay::new();
        o.page = Page::Main;
        let quit_i = o.rows().iter().position(|&r| r == Row::Quit).unwrap();
        // First confirm: armed, not quitting.
        o.sel = quit_i;
        o.confirm();
        assert!(!o.quit_requested && o.quit_armed.is_some());
        // Any other row's confirm disarms.
        o.sel = 0; // Resume
        o.confirm();
        assert!(o.quit_armed.is_none());
        // Navigation disarms too.
        o.sel = quit_i;
        o.confirm();
        assert!(o.quit_armed.is_some());
        o.move_sel(-1);
        assert!(o.quit_armed.is_none());
        // An arm past its 3 s window expires (checked per overlay frame);
        // the next confirm arms again instead of quitting.
        o.confirm();
        o.quit_armed = Some(Instant::now() - Duration::from_secs(4));
        o.quit_check();
        assert!(o.quit_armed.is_none());
        o.sel = quit_i;
        o.confirm();
        assert!(!o.quit_requested && o.quit_armed.is_some());
        // Second confirm inside the window: quit (the window closes; the
        // overlay object stays, the window takes the flag after close()).
        o.confirm();
        assert!(o.quit_requested);
        assert!(!o.show);
    }

    /// Every row of every page has a help line (U5). A restart-only change
    /// is staged so the conditional "Restart now" row (U16) is covered too.
    #[test]
    fn every_row_has_help() {
        let mut o = Overlay::new();
        o.msaa = 4;
        for page in [Page::Main, Page::Graphics, Page::Controls] {
            o.page = page;
            for row in o.rows() {
                assert!(!help(row).is_empty(), "{row:?} on {page:?} has no help");
            }
        }
    }

    /// The player-facing wording (U5): no jargon labels or values left.
    #[test]
    fn row_wording_is_player_facing() {
        let mut o = Overlay::new();
        o.page = Page::Graphics;
        let (name, value, _) = o.row_text(Opt::Msaa);
        assert_eq!((name, value.as_str()), ("Anti-aliasing", "Off"));
        let (name, value, _) = o.row_text(Opt::Filter);
        assert_eq!((name, value.as_str()), ("Scaling filter", "Smooth"));
        let (name, value, _) = o.row_text(Opt::Widescreen);
        assert_eq!((name, value.as_str()), ("Widescreen", "Off (4:3)"));
        let (name, value, _) = o.row_text(Opt::DisplayMode);
        assert_eq!((name, value.as_str()), ("Display mode", "Window"));
        let (name, value, _) = o.row_text(Opt::Resolution);
        assert_eq!(
            (name, value.as_str()),
            ("Resolution", "Only for exclusive fullscreen")
        );
        // The restart banner names the row the way the row does.
        o.msaa = 4;
        assert_eq!(o.pending_restart(), vec!["Anti-aliasing"]);
        // Frame rate names the tracked rate (unknown monitor: no Hz).
        let (name, value, _) = o.row_text(Opt::Fps);
        assert_eq!((name, value.as_str()), ("Frame rate", "Match display"));
    }

    /// The U16 restart command construction: this exe plus the original
    /// command-line arguments with `argv[0]` dropped (never spawned here:
    /// that would start a second copy of the process).
    #[test]
    fn restart_command_runs_this_exe_with_the_original_args() {
        let cmd = restart_command().unwrap();
        assert_eq!(cmd.get_program(), std::env::current_exe().unwrap());
        let args: Vec<_> = cmd.get_args().collect();
        let expected: Vec<_> = std::env::args_os().skip(1).collect();
        assert_eq!(args, expected);
    }

    /// A failed "Restart now" (U16): the banner text stays and the status
    /// line names the error; nothing is queued for the window, the overlay
    /// stays open.
    #[test]
    fn restart_now_failure_keeps_the_banner() {
        let mut o = Overlay::new();
        o.page = Page::Graphics;
        o.msaa = 4;
        o.snapshot = o.staged_settings(); // nothing to write either way
        o.show = true; // the overlay is open while the row runs
        // A command that can't run: spawn fails, nothing real starts.
        let bad = Ok(std::process::Command::new("pw64-no-such-exe"));
        o.restart_with(bad);
        assert!(
            o.restart_error
                .as_deref()
                .is_some_and(|e| e.starts_with("Couldn't restart: "))
        );
        assert!(!o.restart_requested);
        assert!(o.show, "a failed restart keeps the overlay open");
        // The banner still lists the pending change (its text stays).
        assert_eq!(o.pending_restart(), vec!["Anti-aliasing"]);
    }

    /// S7: restart-only rows stage what this session saved (restart still
    /// pending), else the running values.
    #[test]
    fn restart_rows_stage_the_saved_values() {
        let mut o = Overlay::new();
        o.stage_restart_rows();
        assert_eq!(o.msaa, o.running.msaa);
        assert_eq!(
            choice_aspect(&o.widescreen[o.widescreen_sel]),
            o.running.widescreen
        );
        assert_eq!(o.fill_screen, o.running.fill_screen);
        o.saved_restart = Some((8, Some(21.0 / 9.0), true));
        o.stage_restart_rows();
        assert_eq!(o.msaa, 8);
        assert_eq!(o.widescreen[o.widescreen_sel].as_deref(), Some("21:9"));
        assert!(o.fill_screen);
        assert!(!o.widescreen_follow);
    }

    /// S8: Reset stages the monitor-default widescreen and saves it by
    /// removing the key ("" in `SavedSettings`); an explicit choice after
    /// that saves a value again.
    #[test]
    fn reset_widescreen_follows_the_monitor() {
        let mut o = Overlay::new();
        o.reset_graphics();
        assert!(o.widescreen_follow);
        assert_eq!(
            choice_aspect(&o.widescreen[o.widescreen_sel]),
            opts::widescreen_default()
        );
        assert_eq!(o.staged_settings().widescreen.as_deref(), Some(""));
        o.widescreen_follow = false;
        assert_ne!(o.staged_settings().widescreen.as_deref(), Some(""));
    }

    #[test]
    fn select_aspect_finds_or_appends() {
        let (mut l, _) = widescreen_choices(None);
        assert_eq!(select_aspect(&mut l, Some(21.0 / 9.0)), 2);
        assert_eq!(select_aspect(&mut l, None), 0);
        let n = l.len();
        assert_eq!(select_aspect(&mut l, Some(1.6)), n);
        assert_eq!(choice_aspect(&l[n]), Some(1.6));
    }
}
