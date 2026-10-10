//! Game window (native-build.md §9).
//!
//! winit owns the main thread (required on Windows/macOS); the game — the
//! thread-local OS core with all its coroutines — runs on a `pw64-game`
//! host thread started once the window and GPU exist. Each swap latch (VI
//! retrace at 60 fps, else the OS core's present tick at `PW64_FPS`, default
//! the monitor refresh; framerate.md) queues the game's framebuffer
//! operations (+ the newly shown framebuffer) in a bounded slot and wakes
//! the event loop (`UserEvent::Frame`); the main thread replays all of them,
//! in order, onto its own `pw64_gfx::Renderer`'s persistent framebuffer
//! targets (`renderer/fb.rs`) and blits the shown one into the surface (4:3
//! letterboxed; present mode per framerate.md's matrix (Fifo with V-Sync
//! or when the display rate isn't exceeded, Mailbox otherwise), so the game
//! thread never blocks on the swapchain, only on a full slot).
//! Close: `QUIT` → the game thread parks at its next retrace → exit without
//! dropping GPU state (NVIDIA teardown crash, native-build.md §8).

use crate::{PARKED, QUIT, config, input, opts, paths, settings};
use pw64_gfx::{FbOp, RenderOptions, Renderer, Widescreen};
use pw64_platform::os;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use winit::window::{Fullscreen, Icon, Window, WindowId};

/// A monitor-rate change to apply on the game thread (`Arc<Mutex<…>>`
/// shared window thread → [`crate::run_game`]'s retrace hook): outer
/// `None` = nothing pending, inner = the new present rate (`None` =
/// the 60 Hz VI path).
pub(crate) type PendingRate = Arc<Mutex<Option<Option<os::vi::PresentRate>>>>;

enum UserEvent {
    /// New framebuffer work is in the slot.
    Frame,
    /// `os::run` returned (deadlock): exit code + the thread dump taken on
    /// the game thread (P1: the OS core is thread-local; empty for code 0).
    Stopped(i32, String),
    /// The settings trigger was pressed on a pad (input thread).
    Settings,
    /// A controller toast (input thread: pad connected / disconnected, U7).
    Toast(String),
    /// U26: the last controller disconnected (input thread). The window
    /// opens the settings overlay (which pauses the game) if the player
    /// asked for that and the gates allow it.
    Pause,
    /// First-run build progress / completion (firstrun.rs worker).
    #[cfg(feature = "first-run")]
    Setup,
}

/// Proxy for the event loop (set in `run`; the input thread's settings
/// trigger reaches the window thread through it).
static PROXY: OnceLock<EventLoopProxy<UserEvent>> = OnceLock::new();

/// Called by the input thread when the settings pad button is pressed.
pub fn notify_settings() {
    if let Some(p) = PROXY.get() {
        let _ = p.send_event(UserEvent::Settings);
    }
}

/// Called by the input thread for a controller toast (U7): shows `text`
/// for 6 s in the window's bottom-left corner.
pub fn notify_toast(text: String) {
    if let Some(p) = PROXY.get() {
        let _ = p.send_event(UserEvent::Toast(text));
    }
}

/// Called by the input thread when the last controller disconnected (U26).
/// Same route as [`notify_toast`]: the decision (the player's setting, a
/// running game, no automation) is made on the window thread.
pub fn request_pause() {
    if let Some(p) = PROXY.get() {
        let _ = p.send_event(UserEvent::Pause);
    }
}

/// Framebuffer work queued by the game thread (`hle::FrameSink`).
#[derive(Default)]
struct Queued {
    /// In game order; every op must be replayed (the targets persist).
    ops: Vec<FbOp>,
    /// The latest newly shown framebuffer.
    show: Option<u32>,
    /// Newly shown frames since the last drain (fps counter).
    frames: u32,
}

/// The game → window queue. Bounded (`MAX_QUEUED_OPS`): when the window
/// thread falls behind, the game thread waits in the sink (see `start`).
#[derive(Default)]
struct SlotInner {
    q: Mutex<Queued>,
    /// Signalled by every drain.
    drained: Condvar,
}

type Slot = Arc<SlotInner>;

/// Framebuffer ops the window may lag behind before the game thread waits
/// for a drain. Ops can't be dropped (the targets persist, renderer.md
/// "Framebuffer persistence"), so the bound is backpressure: the OS core
/// stalls in the sink (its clock keeps running, so the game's dt grows —
/// graceful slowdown, like a slow N64 frame). Normally the window drains on
/// every `UserEvent::Frame` and the queue holds 1–3 ops; this only bites
/// when the window thread is blocked (FIFO present, modal move/resize loop)
/// or the GPU can't keep up with an uncapped rate.
const MAX_QUEUED_OPS: usize = 32;

pub(crate) struct Gpu {
    pub(crate) window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    pub(crate) device: wgpu::Device,
    pub(crate) queue: wgpu::Queue,
    pub(crate) config: wgpu::SurfaceConfiguration,
    /// `options.filter` is changed live by the settings overlay.
    pub(crate) renderer: Renderer,
    /// Supersampling factor and blit filter; the settings overlay changes
    /// both live.
    pub(crate) scale: f32,
    pub(crate) filter: wgpu::FilterMode,
    /// Live frame rate + V-Sync (the settings overlay changes both live);
    /// startup values from `opts::fps(true)` / `opts::vsync()`.
    pub(crate) fps: opts::Fps,
    pub(crate) vsync: bool,
    /// OLED care (O3, live): the HUD drifts and dims by these; startup
    /// values from `opts::oled_drift()` / `opts::oled_brightness()`.
    pub(crate) oled_drift: bool,
    pub(crate) oled_brightness: f32,
    /// The current display mode (`PW64_DISPLAY_MODE`, the settings screen's
    /// Display mode row); kept in sync by [`Self::set_display_mode`].
    pub(crate) display_mode: opts::DisplayMode,
    /// The fullscreen kind the last switch used (`Windowed` never lands
    /// here): F11 / Alt+Enter go back to this one instead of always
    /// borderless.
    fullscreen_kind: opts::DisplayMode,
    /// The video mode an exclusive fullscreen switches to
    /// (`PW64_FULLSCREEN_RES`, the settings screen's Resolution row;
    /// `None` = the monitor's current mode).
    pub(crate) fullscreen_res: Option<opts::Res>,
    /// Can this window do an exclusive fullscreen at all? (No on Wayland:
    /// compositors don't expose video-mode switches.)
    pub(crate) exclusive_ok: bool,
    /// The refresh rate of the video mode the last exclusive switch chose
    /// (millihertz), as the monitor's own reading can lag the switch.
    exclusive_mhz: Option<u32>,
    /// W1: exclusive fullscreen left (and the window minimised) on focus
    /// loss; re-applied on focus gain ([`Self::focus_changed`]).
    exclusive_suspended: bool,
    /// When the last display mode switch happened (W1 focus grace).
    mode_set: Instant,
    /// A message for the player from a display switch (W6 fallback); the
    /// window shows it as a toast.
    pub(crate) notice: Option<String>,
    /// The surface's supported present modes (`caps.present_modes`), for
    /// re-choosing the mode when fps/vsync change live.
    present_modes: Vec<wgpu::PresentMode>,
    /// The overlay changed fps/vsync: recompute the present rate + mode
    /// (`refresh_present`) before the next present.
    pub(crate) present_dirty: bool,
    /// `PW64_WIN_SHOT=<n1,n2..>`: read back the surface after rendering the
    /// nth presented frame (what the window shows, incl. scale blit).
    shots: Vec<u64>,
    /// U26: open the settings overlay when the window loses focus or the
    /// last controller disconnects (the settings screen's row, live).
    pub(crate) pause_background: bool,
    /// U25: the window title shows the frame rate (the settings screen's
    /// "Show FPS" row, live).
    pub(crate) show_fps: bool,
    /// U25: what the title currently shows (`set_title` runs on change
    /// only; the plain title is the default and the created one).
    title_fps: bool,
}

/// U26: open the settings overlay for a background event (the window lost
/// focus, the last controller disconnected)? Gates, pure so tests pin them:
/// the player's setting, a running game (not the setup screen), a closed
/// overlay (it must not toggle), and never automation (`is_interactive`
/// covers the scripted/headless runs).
fn should_auto_pause(
    enabled: bool,
    overlay_open: bool,
    game_running: bool,
    interactive: bool,
) -> bool {
    enabled && game_running && !overlay_open && interactive
}

impl Gpu {
    /// Framebuffer target size: the output area of the surface (no
    /// letterbox bars) × `scale` (`PW64_SCALE` render scale: `fb_present`
    /// resamples it with `filter`), clamped to the device's max texture
    /// size keeping the aspect ratio. A change resamples the targets.
    fn fb_size(&self) -> (u32, u32) {
        self.fb_size_for_scale(self.scale)
    }

    /// The target size for one render scale on this surface's output area
    /// (the settings screen shows what a choice means in pixels).
    pub(crate) fn fb_size_for_scale(&self, scale: f32) -> (u32, u32) {
        let [_, _, w, h] = self
            .renderer
            .output_rect((self.config.width, self.config.height));
        Self::fb_size_for(scale, (w, h), self.device.limits().max_texture_dimension_2d)
    }

    /// One render scale's target size for the output area `(w, h)`, clamped
    /// to `max_tex`, aspect kept. Below 1 the game renders small and
    /// `fb_present` upscales with the Scale filter (blurry); above 1 it
    /// supersamples. Exact at 1.0 (the output size itself, no float
    /// rounding). Pure, so the settings screen can stage values.
    pub(crate) fn fb_size_for(scale: f32, out: (u32, u32), max_tex: u32) -> (u32, u32) {
        let (w, h) = (out.0.max(1), out.1.max(1));
        let max = max_tex as f32;
        let s = if scale == 1.0 {
            1.0
        } else {
            scale.clamp(0.5, max / w.max(h) as f32)
        };
        (
            ((w as f32 * s) as u32).clamp(1, max_tex),
            ((h as f32 * s) as u32).clamp(1, max_tex),
        )
    }

    /// Switches to `mode` (startup: the config; the settings screen's
    /// Display mode row; F11 / Alt+Enter via [`Self::toggle_fullscreen`]).
    /// A requested exclusive falls back to borderless when no video mode
    /// matches (logged in [`exclusive_mode`]); updates `display_mode` /
    /// `fullscreen_kind` to what actually happened.
    pub(crate) fn set_display_mode(&mut self, mode: opts::DisplayMode) {
        // Wayland has no exclusive fullscreen (a saved/env value can still
        // ask for it): borderless instead.
        let mode = match mode {
            opts::DisplayMode::Exclusive if !self.exclusive_ok => opts::DisplayMode::Borderless,
            m => m,
        };
        let fullscreen = match mode {
            opts::DisplayMode::Windowed => None,
            opts::DisplayMode::Borderless => Some(Fullscreen::Borderless(None)),
            opts::DisplayMode::Exclusive => {
                let monitor = self.window.current_monitor();
                match monitor
                    .as_ref()
                    .and_then(|m| Some((m, exclusive_mode(m, self.fullscreen_res)?)))
                {
                    // W6: winit can't report a mode switch the driver
                    // refuses, so test it first and fall back politely.
                    Some((m, v)) if !crate::sys::video_mode_supported(m, &v) => {
                        self.notice = Some(
                            "That resolution isn't supported, using borderless fullscreen.".into(),
                        );
                        Some(Fullscreen::Borderless(None))
                    }
                    Some((_, v)) => {
                        self.exclusive_mhz = Some(v.refresh_rate_millihertz());
                        Some(Fullscreen::Exclusive(v))
                    }
                    None => Some(Fullscreen::Borderless(None)),
                }
            }
        };
        self.exclusive_suspended = false;
        self.mode_set = Instant::now();
        if fullscreen.is_some() {
            // The kind the next F11 / Alt+Enter returns to (a fallback makes
            // a requested exclusive a borderless one).
            self.fullscreen_kind = if matches!(fullscreen, Some(Fullscreen::Exclusive(_))) {
                opts::DisplayMode::Exclusive
            } else {
                opts::DisplayMode::Borderless
            };
        }
        self.display_mode = if fullscreen.is_some() {
            self.fullscreen_kind
        } else {
            opts::DisplayMode::Windowed
        };
        self.window.set_fullscreen(fullscreen);
    }

    /// W1: winit's exclusive fullscreen is a topmost window at the game's
    /// video mode and does nothing on focus loss, so Alt+Tab / Win+L would
    /// leave the desktop at that mode behind a topmost window. Focus lost
    /// in exclusive: leave fullscreen (winit restores the desktop mode) and
    /// minimise; focus back: re-apply exclusive. Returns true when the
    /// display changed (the caller re-reads the present rate). Focus losses
    /// right after a mode switch are ignored: the switch itself can bounce
    /// the focus, and minimising then would strand the window.
    pub(crate) fn focus_changed(&mut self, focused: bool) -> bool {
        if focused {
            if !self.exclusive_suspended {
                return false;
            }
            self.window.set_minimized(false);
            self.set_display_mode(opts::DisplayMode::Exclusive);
            return true;
        }
        let exclusive = matches!(self.window.fullscreen(), Some(Fullscreen::Exclusive(_)));
        if !exclusive || self.mode_set.elapsed() < Duration::from_secs(1) {
            return false;
        }
        self.window.set_fullscreen(None);
        self.window.set_minimized(true);
        self.exclusive_suspended = true;
        true
    }

    /// Windowed ↔ the last fullscreen kind (F11, Alt+Enter).
    pub(crate) fn toggle_fullscreen(&mut self) {
        let mode = if self.window.fullscreen().is_some() {
            opts::DisplayMode::Windowed
        } else {
            self.fullscreen_kind
        };
        self.set_display_mode(mode);
    }

    /// Recomputes the present rate and mode from the live fps/vsync and the
    /// current monitor, and reconfigures the surface when the mode changed.
    /// Returns the rate for the `last_rate` / queue bookkeeping in
    /// `App::apply_present`. Every display change goes through here, so the
    /// mode follows the setting live (R-a, R-b, R-e).
    fn refresh_present(&mut self) -> Option<os::vi::PresentRate> {
        // Exclusive fullscreen: the monitor handle still reports the desktop
        // mode's rate, but the display now runs at the chosen video mode's.
        let mhz = match (self.display_mode, self.exclusive_mhz) {
            (opts::DisplayMode::Exclusive, Some(mhz)) => Some(mhz),
            _ => self
                .window
                .current_monitor()
                .and_then(|m| m.refresh_rate_millihertz()),
        };
        let rate = opts::present_rate(self.fps, mhz, self.vsync);
        let tearing = opts::allows_tearing(self.fps, mhz);
        let mode = opts::present_mode(self.vsync, rate.is_some(), tearing, &self.present_modes);
        if self.config.present_mode != mode {
            eprintln!(
                "[window] present mode {:?} → {mode:?}",
                self.config.present_mode
            );
            self.config.present_mode = mode;
            // wgpu's `Surface::configure` waits for the GPU to come idle and
            // panics (fatal error sink) when submissions are still in flight,
            // which this frame's own just-queued work always is. Drain first
            // (ms): the only device users are this thread (the game thread's
            // dump renderer is gated off when no dump env vars are set).
            let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
            self.surface.configure(&self.device, &self.config);
        }
        rate
    }
}

/// Picks the video mode for an exclusive fullscreen: the candidates at
/// `res`'s size (or the monitor's current size), preferring `res`'s rate
/// (else the monitor's current reading), then the nearest rate, then the
/// highest colour depth. `None`: no mode at that size, the caller falls
/// back to borderless (the user picks a supported `WxH@Hz`).
fn exclusive_mode(
    monitor: &winit::monitor::MonitorHandle,
    res: Option<opts::Res>,
) -> Option<winit::monitor::VideoModeHandle> {
    use std::cmp::Reverse;
    let (w, h) = res.map_or_else(
        || {
            let s = monitor.size();
            (s.width, s.height)
        },
        |r| (r.w, r.h),
    );
    // The rate to prefer, in millihertz (`res`'s exact Hz, else what the
    // monitor reports now). Compared unrounded: nearest wins anyway.
    let want = res
        .and_then(|r| r.hz)
        .map(|hz| hz * 1000)
        .or(monitor.refresh_rate_millihertz());
    let mut modes: Vec<_> = monitor
        .video_modes()
        .filter(|m| m.size() == winit::dpi::PhysicalSize::new(w, h))
        .collect();
    modes.sort_by_key(|m| {
        (
            want.map_or(0, |wmhz| {
                (m.refresh_rate_millihertz() as i64 - wmhz as i64).abs()
            }),
            Reverse(m.bit_depth()),
        )
    });
    let mode = modes.into_iter().next();
    if mode.is_none() {
        eprintln!("[window] no exclusive video mode at {w}x{h}; using borderless");
    }
    mode
}

/// U19: the starting inner size, in logical px, for a window of the game
/// aspect `aspect` (width / height; 4/3 or the widescreen value) on a
/// monitor `monitor_px` physical pixels big with scale factor `scale`: the
/// largest one that fits 80% of the monitor's height and never wider than
/// the monitor (a portrait screen must not clip the window). No 960x720
/// floor: on a 1080p laptop at 150% (720 logical px high) or a 1366x768
/// screen a 720-high window plus its title bar runs under the taskbar and
/// off the screen (review of 1d74cdc). The caller centres it.
pub(crate) fn first_window_size(monitor_px: (u32, u32), scale: f64, aspect: f64) -> (f64, f64) {
    let scale = if scale > 0.0 { scale } else { 1.0 };
    let monitor_w = f64::from(monitor_px.0) / scale;
    let monitor_h = f64::from(monitor_px.1) / scale;
    // 80% of the height, or the full width / aspect when the monitor is
    // the narrow way round for the game aspect (portrait).
    let by_height = (0.8 * monitor_h).round();
    let by_width = (monitor_w / aspect).round();
    // 120: the window's min inner height (a broken monitor reading).
    let h = by_height.min(by_width).max(120.0);
    let w = (h * aspect).round();
    (w, h)
}

/// U24: how long the mouse must be still before the cursor hides in
/// fullscreen play.
const CURSOR_HIDE_AFTER: Duration = Duration::from_secs(2);

/// U24: hide the mouse cursor while playing fullscreen with the settings
/// overlay closed, no setup screen up, and the mouse idle (the caller
/// measures against [`CURSOR_HIDE_AFTER`]). Pure, so tests pin the rule.
fn should_hide_cursor(fullscreen: bool, overlay_open: bool, setup: bool, idle: bool) -> bool {
    fullscreen && !overlay_open && !setup && idle
}

struct App {
    proxy: EventLoopProxy<UserEvent>,
    slot: Slot,
    gpu: Option<Gpu>,
    /// The framebuffer the game last swapped to (None: nothing yet).
    shown: Option<u32>,
    modifiers: ModifiersState,
    fps_timer: Instant,
    frames: u32,
    /// Presented frames with new content, never reset (`PW64_WIN_SHOT`).
    presented: u64,
    /// Last cursor position, physical px (`CursorMoved`).
    cursor: (f32, f32),
    /// U24: when the mouse last moved (the idle check in `update_cursor`).
    cursor_moved: Instant,
    /// U24: what `set_cursor_visible` last set (the call runs on change
    /// only; `update_cursor` recomputes the wish every redraw).
    cursor_shown: bool,
    /// Settings overlay (None until first opened: no cost while closed).
    overlay: Option<settings::Overlay>,
    /// Monitor-rate changes for the game thread's retrace hook
    /// ([`PendingRate`]); written here, taken there.
    pending_rate: PendingRate,
    /// The present rate the last monitor-rate read produced (startup's
    /// included), so unchanged readings don't queue work.
    last_rate: Option<Option<os::vi::PresentRate>>,
    /// First run: no game module yet, show the setup screen at `start`.
    #[cfg(feature = "first-run")]
    setup_needed: bool,
    /// The setup screen while it is up (the game thread isn't running).
    #[cfg(feature = "first-run")]
    setup: Option<crate::firstrun::Screen>,
    /// Next setup-screen repaint (progress bar animation between reports).
    #[cfg(feature = "first-run")]
    setup_frame: Instant,
    /// No ROM yet and no file dialog on this system: the ROM screen while it
    /// is up (before the setup screen / game).
    rom_screen: Option<crate::rom_screen::Screen>,
    /// Toast notifications (bottom-left; the first-launch settings hint).
    toast: crate::toast::Toast,
    /// The first-launch hint has been handled this run (once, whatever the
    /// outcome: the config flag alone can't gate it, it is read once).
    hint_shown: bool,
    /// `PW64_SETTINGS_SHOT=<n>`: settings-screen automation (ga-polish S1).
    /// Once `presented` reaches n: open the overlay, feed the
    /// `PW64_SETTINGS_KEYS` sequence, capture `tmp/win_settings.png` after
    /// the overlay pass and quit.
    settings_shot: Option<u64>,
    /// Redraws until that capture (set by the trigger; the panel fades in
    /// over egui's animation time, so the first overlay frame is invisible).
    settings_shot_frame: u8,
}

/// Opens the window and runs the game in it. `setup`: the game module still
/// has to be built (first-run feature; always false otherwise): the window
/// shows the setup screen first. `ask_rom`: no ROM is loaded yet (no file
/// dialog on this system): the ROM screen comes before everything else.
pub fn run(setup: bool, ask_rom: bool) -> ! {
    #[cfg(not(feature = "first-run"))]
    let _ = setup;
    let el = match EventLoop::<UserEvent>::with_user_event().build() {
        Ok(el) => el,
        Err(e) => {
            eprintln!("error: no window system ({e}); set PW64_HEADLESS=1");
            std::process::exit(1);
        }
    };
    let _ = PROXY.set(el.create_proxy());
    let mut app = App {
        proxy: el.create_proxy(),
        slot: Arc::default(),
        gpu: None,
        shown: None,
        modifiers: ModifiersState::empty(),
        fps_timer: Instant::now(),
        frames: 0,
        presented: 0,
        cursor: (0.0, 0.0),
        cursor_moved: Instant::now(),
        cursor_shown: true,
        overlay: None,
        pending_rate: Arc::new(Mutex::new(None)),
        last_rate: None,
        #[cfg(feature = "first-run")]
        setup_needed: setup,
        #[cfg(feature = "first-run")]
        setup: None,
        #[cfg(feature = "first-run")]
        setup_frame: Instant::now(),
        rom_screen: ask_rom.then(crate::rom_screen::Screen::new),
        toast: crate::toast::Toast::new(),
        hint_shown: false,
        settings_shot: crate::hle::env_list("PW64_SETTINGS_SHOT").first().copied(),
        settings_shot_frame: 0,
    };
    if let Err(e) = el.run_app(&mut app) {
        eprintln!("error: event loop: {e}");
    }
    quit(0)
}

/// Title-bar/taskbar icon: the committed 64x64 RGBA PNG (generated by
/// `assets/icon/render.py`), decoded once. Windows takes the taskbar icon
/// from the exe resource too, but set it anyway (matches the window and
/// covers the gui-subsystem exe's taskbar group). `None` = decode failed:
/// cosmetic, never fail the window over it.
fn window_icon() -> Option<Icon> {
    let mut reader = png::Decoder::new(std::io::Cursor::new(include_bytes!(
        "../../../assets/icon/birdman64-64.png"
    )))
    .read_info()
    .ok()?;
    let size = reader.output_buffer_size()?;
    let mut buf = vec![0; size];
    let frame = reader.next_frame(&mut buf).ok()?;
    buf.truncate(frame.buffer_size());
    if frame.color_type != png::ColorType::Rgba || frame.bit_depth != png::BitDepth::Eight {
        return None;
    }
    Icon::from_rgba(buf, frame.width, frame.height).ok()
}

/// Stops the game thread at a retrace boundary, then exits the process
/// without dropping anything (GPU teardown note in `main`).
fn quit(code: i32) -> ! {
    // U14: a clean quit clears the running marker (a crash leaves it in
    // place, so the next start can say the last run didn't close well).
    let _ = std::fs::remove_file(paths::running_lock());
    crate::sys::keep_display_awake(false);
    QUIT.store(true, Ordering::Release);
    let t = Instant::now();
    while !PARKED.load(Ordering::Acquire) && t.elapsed() < Duration::from_secs(1) {
        std::thread::sleep(Duration::from_millis(2));
    }
    std::process::exit(code)
}

/// The `PW64_SETTINGS_KEYS` sequence (comma-separated key names via
/// `input::parse_key`), fed to the overlay by the settings-shot automation.
/// Unknown names are skipped with a note.
fn settings_shot_keys() -> Vec<KeyCode> {
    std::env::var("PW64_SETTINGS_KEYS")
        .unwrap_or_default()
        .split(',')
        .filter_map(|name| {
            let name = name.trim();
            if name.is_empty() {
                return None;
            }
            match input::parse_key(name) {
                Some(code) => Some(code),
                None => {
                    eprintln!("[window] PW64_SETTINGS_KEYS: unknown key {name:?}");
                    None
                }
            }
        })
        .collect()
}

/// Queues a changed present rate for the game thread's retrace hook (the
/// kernel state is only reachable there). Unchanged readings do nothing.
fn queue_rate(
    pending_rate: &PendingRate,
    last_rate: &mut Option<Option<os::vi::PresentRate>>,
    rate: Option<os::vi::PresentRate>,
) {
    if *last_rate != Some(rate) {
        eprintln!("[window] present rate → present tick {rate:?}");
        *pending_rate.lock().unwrap() = Some(rate);
        *last_rate = Some(rate);
    }
}

impl App {
    fn start(&mut self, el: &ActiveEventLoop) {
        // D3: the default widescreen is this monitor's aspect (wider than
        // 4:3, clamped to 21:9). Must be set before the first
        // `opts::widescreen()` read below, which sizes the window. Headless
        // runs never set it: their dumps stay 4:3 unless PW64_WIDESCREEN.
        if let Some(m) = el.primary_monitor() {
            let s = m.size();
            opts::set_widescreen_monitor_default(s.width, s.height);
        }
        let monitor = el.primary_monitor();
        // `PW64_WIDESCREEN`: open at the game aspect for this run (4:3, or
        // the widescreen value); the renderer letter/pillarboxes on resize.
        let widescreen = opts::widescreen();
        let aspect = widescreen.map_or(4.0 / 3.0, f64::from);
        // U19: the largest game-aspect window that fits 80% of the
        // monitor's height (960x720 when there is no monitor reading).
        let (width, height) = monitor.as_ref().map_or((960.0, 720.0), |m| {
            let s = m.size();
            first_window_size((s.width, s.height), m.scale_factor(), aspect)
        });
        eprintln!(
            "[window] start size {width}x{height} logical (aspect {aspect:.3}, monitor {})",
            monitor.as_ref().map_or("?".into(), |m| format!(
                "{}x{} @ {}%",
                m.size().width,
                m.size().height,
                (m.scale_factor() * 100.0).round()
            ))
        );
        let window = Arc::new(
            el.create_window(
                Window::default_attributes()
                    .with_title("Birdman64")
                    .with_window_icon(window_icon())
                    .with_inner_size(winit::dpi::LogicalSize::new(width, height))
                    .with_min_inner_size(winit::dpi::LogicalSize::new(160.0, 120.0)),
            )
            .unwrap_or_else(|e| {
                crate::fatal(&format!("Birdman64 could not create its window. ({e})"))
            }),
        );
        // U19: centre on the monitor the size came from (winit's default
        // placement is OS-dependent). The window's outer size is known only
        // after creation, so this has to follow it (one frame at worst).
        if let Some(m) = &monitor {
            let (mp, ms) = (m.position(), m.size());
            let outer = window.outer_size();
            window.set_outer_position(winit::dpi::PhysicalPosition::new(
                mp.x + ((ms.width.saturating_sub(outer.width)) / 2) as i32,
                mp.y + ((ms.height.saturating_sub(outer.height)) / 2) as i32,
            ));
        }
        // Dev switch (L1): force the software adapter (WARP on DX12) to test
        // the low-end preset.
        let force_fallback = std::env::var_os("PW64_FALLBACK_ADAPTER").is_some();
        let instance = wgpu::Instance::default();
        let surface = instance.create_surface(window.clone()).unwrap_or_else(|e| {
            crate::fatal(&format!(
                "Birdman64 could not set up graphics for its window. Updating your \
                 graphics driver may help. ({e})"
            ))
        });
        let adapter = pw64_gfx::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: force_fallback,
        }));
        let adapter = match adapter {
            Ok(a) => a,
            Err(e) => crate::fatal(&format!(
                "No compatible graphics adapter was found. Birdman64 needs a GPU with \
                 Vulkan, DirectX 12 or OpenGL support. Updating your graphics driver may \
                 help. ({e})"
            )),
        };
        // Low-end preset (L1): a software rasteriser gets gentler defaults
        // (60 fps, 75% render resolution); explicit settings still win.
        // Must run before the first `opts::fps` / `opts::scale` read.
        let info = adapter.get_info();
        if opts::low_end_adapter(&info) {
            opts::set_low_end();
            eprintln!(
                "[window] software GPU ({}): defaults 60 fps, 75% render resolution",
                info.name
            );
            self.toast.show(
                "No graphics driver found, so Birdman64 uses gentler settings (60 fps, \
                 75% resolution). Installing your graphics driver gives a smoother game.",
                10,
            );
        }
        let (device, queue) =
            pw64_gfx::block_on(adapter.request_device(&pw64_gfx::device_descriptor(&adapter)))
                .unwrap_or_else(|e| {
                    crate::fatal(&format!(
                        "Birdman64 could not set up the graphics device ({}). Updating your \
                 graphics driver may help. ({e})",
                        adapter.get_info().name
                    ))
                });
        let caps = surface.get_capabilities(&adapter);
        // The N64 outputs gamma-space colors: prefer a non-sRGB format.
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let size = window.inner_size();
        // COPY_SRC for the `PW64_WIN_SHOT` / `PW64_SETTINGS_SHOT` readback,
        // only if requested and the surface supports it (configure panics
        // otherwise).
        let mut shots = crate::hle::env_list("PW64_WIN_SHOT");
        let mut usage = wgpu::TextureUsages::RENDER_ATTACHMENT;
        if !shots.is_empty() || std::env::var_os("PW64_SETTINGS_SHOT").is_some() {
            if caps.usages.contains(wgpu::TextureUsages::COPY_SRC) {
                usage |= wgpu::TextureUsages::COPY_SRC;
            } else {
                eprintln!("[window] surface can't be read back; PW64_WIN_SHOT ignored");
                shots.clear();
            }
        }
        // Game frame rate (`PW64_FPS`, default: this monitor's refresh) and
        // V-Sync (`PW64_VSYNC`, default off — matrix in framerate.md).
        let vsync = opts::vsync();
        let fps = opts::fps(true);
        let monitor = window
            .current_monitor()
            .and_then(|m| m.refresh_rate_millihertz());
        let rate = opts::present_rate(fps, monitor, vsync);
        // Present mode (opts::present_mode): V-Sync → Fifo; else with a
        // present tick Mailbox (the newest frame goes out at the next vblank
        // and the window thread never blocks, so a free-running tick at the
        // monitor rate only drops/repeats a frame once per beat period,
        // minutes for a rate read from the monitor).
        // Immediate only when the player asked for more than the display
        // shows (allows_tearing); otherwise Fifo/AutoVsync (the 60 Hz VI
        // path) block the window thread per present; the op-queue bound then
        // throttles the game.
        let tearing = opts::allows_tearing(fps, monitor);
        let present_mode = opts::present_mode(vsync, rate.is_some(), tearing, &caps.present_modes);
        eprintln!(
            "[window] fps {fps:?} (monitor {} Hz) vsync {} → present tick {rate:?}, {present_mode:?}",
            monitor.map_or("?".into(), |m| format!("{:.3}", m as f64 / 1000.0)),
            if vsync { "on" } else { "off" }
        );
        let config = wgpu::SurfaceConfiguration {
            usage,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode,
            desired_maximum_frame_latency: 2,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
        };
        surface.configure(&device, &config);
        let msaa = opts::resolve_msaa(&adapter, format, opts::msaa());
        let renderer = Renderer::new(
            &device,
            &queue,
            format,
            RenderOptions {
                msaa,
                widescreen: widescreen.map_or(Widescreen::Off, Widescreen::Aspect),
                filter: opts::tex_filter(),
                fill_view: opts::fill_screen(),
                oled: Default::default(),
            },
        );
        let (scale, filter) = (opts::scale(), opts::scale_filter());
        // Display mode (`PW64_DISPLAY_MODE`) and the exclusive video mode
        // (`PW64_FULLSCREEN_RES`); exclusive is unavailable on Wayland
        // (compositors don't expose video-mode switches).
        let display_mode = opts::display_mode();
        let fullscreen_res = opts::fullscreen_resolution();
        #[cfg(target_os = "linux")]
        let exclusive_ok = !winit::platform::wayland::ActiveEventLoopExtWayland::is_wayland(el);
        #[cfg(not(target_os = "linux"))]
        let exclusive_ok = true;
        eprintln!(
            "[window] GPU: {}, surface {format:?}, msaa {msaa}×, scale {scale}×, display mode {}",
            adapter.get_info().name,
            display_mode.as_str()
        );

        self.last_rate = Some(rate);
        self.gpu = Some(Gpu {
            window,
            surface,
            device,
            queue,
            config,
            renderer,
            scale,
            filter,
            fps,
            vsync,
            oled_drift: opts::oled_drift(),
            oled_brightness: opts::oled_brightness(),
            display_mode,
            fullscreen_res,
            fullscreen_kind: match display_mode {
                opts::DisplayMode::Windowed => opts::DisplayMode::Borderless,
                m => m,
            },
            exclusive_ok,
            exclusive_mhz: None,
            exclusive_suspended: false,
            mode_set: Instant::now(),
            notice: None,
            present_modes: caps.present_modes,
            present_dirty: false,
            shots,
            pause_background: opts::pause_in_background(),
            show_fps: opts::show_fps(),
            title_fps: false,
        });
        // The configured display mode applies right away (the Resized event
        // reconfigures the surface for the new size), then the present rate
        // (a fullscreen switch may move the window to another monitor).
        self.gpu.as_mut().unwrap().set_display_mode(display_mode);
        self.apply_present();
        // The ROM screen first (rom_screen.rs); `after_rom` once it has one.
        if self.rom_screen.is_some() {
            self.gpu.as_ref().unwrap().window.request_redraw();
            return;
        }
        self.after_rom();
    }

    /// A ROM is installed: the setup screen (first run) or the game.
    fn after_rom(&mut self) {
        // First run (firstrun.rs): the setup screen builds the game module;
        // the game thread starts once it is loaded (`UserEvent::Setup`).
        #[cfg(feature = "first-run")]
        if self.setup_needed {
            let proxy = self.proxy.clone();
            self.setup = Some(crate::firstrun::Screen::start(move || {
                let _ = proxy.send_event(UserEvent::Setup);
            }));
            return;
        }
        self.spawn_game();
    }

    /// Starts the game thread: it shares the device (for PNG dumps) and
    /// posts frames into the slot.
    fn spawn_game(&mut self) {
        // U14: a running.lock left by the previous run means that one never
        // got to a clean quit (crash, power loss): say so once, over the
        // first frames, before this run's marker is created below.
        let stale_lock = paths::running_lock().is_file();
        let Some(g) = &self.gpu else { return };
        let (slot, proxy) = (self.slot.clone(), self.proxy.clone());
        let stop_proxy = self.proxy.clone();
        let pending_rate = self.pending_rate.clone();
        // The latest reading (a monitor change during first-run setup
        // included).
        let rate = self.last_rate.flatten();
        let gpu = (g.device.clone(), g.queue.clone());
        let r = std::thread::Builder::new()
            .name("pw64-game".into())
            .spawn(move || {
                let sink: crate::hle::FrameSink = Box::new(move |ops, show| {
                    let mut q = slot.q.lock().unwrap();
                    // Backpressure (MAX_QUEUED_OPS): wait for the window to
                    // drain; never while quitting (the window thread is then
                    // waiting for us to park, not draining).
                    while q.ops.len() >= MAX_QUEUED_OPS && !QUIT.load(Ordering::Acquire) {
                        q = slot
                            .drained
                            .wait_timeout(q, Duration::from_millis(50))
                            .unwrap()
                            .0;
                    }
                    q.ops.extend(ops);
                    if show.is_some() {
                        q.show = show;
                        q.frames += 1;
                    }
                    drop(q);
                    let _ = proxy.send_event(UserEvent::Frame);
                });
                let stop = crate::run_game(Some(gpu), Some(sink), rate, pending_rate);
                let code = crate::exit_code(stop);
                let dump = if code != 0 {
                    os::dump_threads()
                } else {
                    String::new()
                };
                let _ = stop_proxy.send_event(UserEvent::Stopped(code, dump));
                // Don't let TLS destructors drop the OS core / GPU state.
                PARKED.store(true, Ordering::Release);
                loop {
                    std::thread::park();
                }
            });
        if let Err(e) = r {
            eprintln!("error: could not start the game thread: {e}");
            std::process::exit(1);
        }
        // The game thread is up: this run is live (U14). Failure to create
        // the marker is logged only: the toast then just shows every start.
        if let Err(e) = std::fs::File::create(paths::running_lock()) {
            eprintln!("[window] running.lock: {e}");
        }
        if stale_lock {
            self.toast.show(
                "Birdman64 didn't close properly last time. If it crashed, \
                 crash.log in the save folder helps us fix it.",
                10,
            );
        }
        // S4: a save (or settings file) in the folder that wasn't chosen.
        if let Some(msg) = paths::startup_notice() {
            eprintln!("[paths] {msg}");
            self.toast.show(&msg, 15);
        }
        // Hand-edited pw64.toml values the loader had to skip.
        let ignored = config::ignored_keys();
        if !ignored.is_empty() {
            self.toast.show(
                &format!(
                    "Some settings in pw64.toml were ignored: {}",
                    ignored.join(", ")
                ),
                10,
            );
        }
        // W4: gamepad play is no keyboard/mouse activity to Windows; keep
        // the display on while the game runs (cleared in `quit`).
        crate::sys::keep_display_awake(true);
    }

    /// First run: the build worker reported. Done → load the module and
    /// start the game in this window; a failure stays on the error screen.
    #[cfg(feature = "first-run")]
    fn setup_event(&mut self) {
        let Some(s) = &mut self.setup else { return };
        if let Some(path) = s.poll() {
            match crate::firstrun::load(&path) {
                Ok(()) => {
                    self.setup = None;
                    self.spawn_game();
                }
                Err(f) => s.fail(f),
            }
        }
        if let Some(g) = &self.gpu {
            g.window.request_redraw();
        }
    }

    /// Draws the ROM screen instead of a game frame; on a loaded ROM it
    /// closes and the setup screen or game follows.
    fn redraw_rom_screen(&mut self) {
        let (Some(g), Some(s)) = (&mut self.gpu, &mut self.rom_screen) else {
            return;
        };
        let tex = match g.surface.get_current_texture() {
            Ok(t) => t,
            Err(wgpu::SurfaceError::Timeout) => return,
            Err(_) => {
                g.surface.configure(&g.device, &g.config);
                return;
            }
        };
        let view = tex.texture.create_view(&Default::default());
        let (next, shot) = s.render(g, &view);
        if shot && g.config.usage.contains(wgpu::TextureUsages::COPY_SRC) {
            crate::hle::shot_png(&g.device, &g.queue, &tex.texture, "tmp/win_setup_rom.png");
        }
        g.window.pre_present_notify();
        tex.present();
        match next {
            crate::rom_screen::Next::Stay => {}
            // Nothing is running yet.
            crate::rom_screen::Next::Quit => std::process::exit(0),
            crate::rom_screen::Next::Loaded => {
                self.rom_screen = None;
                self.after_rom();
                if let Some(g) = &self.gpu {
                    g.window.request_redraw();
                }
            }
        }
    }

    /// First run: draws the setup screen instead of a game frame. Returns
    /// false when the player chose Quit.
    #[cfg(feature = "first-run")]
    fn redraw_setup(&mut self) -> bool {
        let (Some(g), Some(s)) = (&mut self.gpu, &mut self.setup) else {
            return true;
        };
        let tex = match g.surface.get_current_texture() {
            Ok(t) => t,
            Err(wgpu::SurfaceError::Timeout) => return true,
            Err(_) => {
                g.surface.configure(&g.device, &g.config);
                return true;
            }
        };
        let view = tex.texture.create_view(&Default::default());
        let (keep, shot) = s.render(g, &view);
        // Dev check of the screen (`PW64_WIN_SHOT` set: surface readable):
        // one capture per step, `tmp/win_setup_<step>.png`.
        if let Some(tag) = shot.filter(|_| g.config.usage.contains(wgpu::TextureUsages::COPY_SRC)) {
            crate::hle::shot_png(
                &g.device,
                &g.queue,
                &tex.texture,
                &format!("tmp/win_setup_{tag}.png"),
            );
        }
        g.window.pre_present_notify();
        tex.present();
        keep
    }

    /// Replays the queued framebuffer operations onto the renderer's
    /// persistent targets (all of them, in order: skipping one would lose
    /// what it drew, renderer/fb.rs) at the current target size. Runs on
    /// every `UserEvent::Frame`, so the queue stays short even while no
    /// redraws happen (minimized window).
    fn drain(&mut self) {
        let q = std::mem::take(&mut *self.slot.q.lock().unwrap());
        self.slot.drained.notify_all();
        let Some(g) = &mut self.gpu else { return };
        // Minimised (Windows sends Resized(0×0) → a 1×1 surface): keep the
        // target size, or resampling to 1×1 and back would wipe screens the
        // game draws only once (photo album, fades).
        if g.config.width > 1 && g.config.height > 1 {
            let size = g.fb_size();
            g.renderer.set_fb_size(size);
        }
        // OLED care (O3): the HUD drifts (a slow circle, spreading the wear)
        // and dims per the settings; the overlay stages new values into the
        // fields above live. PNG dumps use their own renderer and stay
        // unaffected.
        g.renderer.options.oled = pw64_gfx::renderer::Oled {
            brightness: g.oled_brightness,
            drift: if g.oled_drift {
                pw64_gfx::renderer::oled_drift(self.fps_timer.elapsed().as_secs_f64())
            } else {
                [0.0; 2]
            },
        };
        for op in &q.ops {
            g.renderer.fb_apply(op);
        }
        if q.show.is_some() {
            self.shown = q.show;
        }
        if q.frames > 0 {
            self.frames += q.frames;
            self.presented += 1;
            self.first_frame_hint();
        }
    }

    /// First-launch welcome card (U8, replaces the S2 hint): once the first
    /// game frame is presented, say hello and how to play. Shown once per
    /// machine (the flag is saved to `pw64.toml` `[ui]`), and never when an
    /// input script is driving (the card would end up in every scripted
    /// screenshot).
    fn first_frame_hint(&mut self) {
        if self.hint_shown {
            return;
        }
        self.hint_shown = true;
        if std::env::var_os("PW64_INPUT_SCRIPT").is_some()
            || std::env::var_os("PW64_SETTINGS_SHOT").is_some()
            || config::get().ui.settings_hint_shown
        {
            return;
        }
        let (title, lines) = settings::welcome_card();
        self.toast.show_card(&title, &lines, 18);
        if let Err(e) = config::save_ui_flag() {
            // Logged only: the card then shows again next launch.
            eprintln!("[config] settings hint flag: {e}");
        }
    }

    fn redraw(&mut self) {
        // U24: the cursor wish is checked here (per redraw, no timer).
        self.update_cursor();
        self.drain();
        // S1 settings-shot trigger: open the overlay and feed the scripted
        // keys before the render below, so the capture lands after the
        // overlay pass (`PW64_WIN_SHOT` shots are taken before the overlay;
        // this one is about the overlay itself). Runs before `self.gpu` is
        // borrowed: `toggle_settings` takes `&mut self`.
        let mut settings_shot = false;
        if let Some(n) = self.settings_shot {
            if self.presented >= n {
                self.settings_shot = None;
                self.toggle_settings();
                if let Some(o) = &mut self.overlay {
                    for code in settings_shot_keys() {
                        o.on_key(code, false, true);
                    }
                }
                // The panel fades in over egui's animation time: the very
                // first overlay frame paints at opacity 0 (invisible), so
                // capture a few dozen redraws later, once it is opaque.
                self.settings_shot_frame = 48;
            }
        } else if self.settings_shot_frame > 0 {
            self.settings_shot_frame -= 1;
            settings_shot = self.settings_shot_frame == 0;
            // Redraws only continue while the overlay is open; if the keys
            // closed it, capture what is on screen right away.
            settings_shot |= self.settings_shot_frame > 0 && !settings::is_open();
        }
        let Some(g) = &mut self.gpu else { return };
        let tex = match g.surface.get_current_texture() {
            Ok(t) => t,
            Err(e) => {
                if !matches!(e, wgpu::SurfaceError::Timeout) {
                    g.surface.configure(&g.device, &g.config);
                }
                // W2: while the overlay is open the game is paused (no
                // Frame events), so only this keeps the overlay alive.
                if settings::is_open() {
                    g.window.request_redraw();
                }
                return;
            }
        };
        let view = tex.texture.create_view(&Default::default());
        // W6: a display switch fell back (unsupported exclusive mode).
        if let Some(msg) = g.notice.take() {
            self.toast.show(&msg, 8);
        }
        // Save-file problems reported by the platform layer (game thread).
        for msg in pw64_platform::headless::take_toasts() {
            self.toast.show(&msg, 10);
        }
        // The shown framebuffer target, letterboxed (downsampled with the
        // `PW64_SCALE` filter when supersampled; 1:1 otherwise).
        g.renderer.fb_present(self.shown, &view, g.filter);
        // A Save with restart-only options changed queued a toast naming
        // them (S5): take it BEFORE the toast pass below, so it shows on
        // the very frame the Save closed the overlay (and in the
        // `PW64_SETTINGS_SHOT` capture, which runs on that same frame).
        if let Some(msg) = self.overlay.as_mut().and_then(|o| o.take_toast()) {
            self.toast.show(&msg, 8);
        }
        // Toasts (S2): over the game frame, before the shot readback (shots
        // show them) and before the overlay.
        self.toast.render(g, &view);
        // Once per frame number (redraws without a new frame don't re-shoot;
        // a number skipped by a multi-frame drain shoots at the next one).
        if let Some(i) = g.shots.iter().position(|&n| n <= self.presented) {
            let n = g.shots.swap_remove(i);
            let path = format!("tmp/win_{n:05}.png");
            crate::hle::shot_png(&g.device, &g.queue, &tex.texture, &path);
        }
        // The overlay draws over the game frame (own pass on the surface);
        // closed, it costs nothing. While open it keeps requesting redraws:
        // the game is paused, so no Frame events arrive to drive the loop.
        let overlay_closed = match (settings::is_open(), &mut self.overlay) {
            (true, Some(o)) => !o.render(g, &view),
            _ => false,
        };
        if settings_shot {
            // The capture must show the overlay: after the pass above
            // (before present is fine, the texture still holds the frame).
            // Copyable only if the surface allowed COPY_SRC (see `new`).
            if g.config.usage.contains(wgpu::TextureUsages::COPY_SRC) {
                crate::hle::shot_png(&g.device, &g.queue, &tex.texture, "tmp/win_settings.png");
            } else {
                eprintln!(
                    "[window] surface can't be read back; PW64_SETTINGS_SHOT capture skipped"
                );
            }
            quit(0);
        }
        g.window.pre_present_notify();
        tex.present();
        // A frame went out: the OS core's Display pacing (V-Sync) ticks on
        // this counter (crates/pw64-platform/src/os/vi.rs).
        os::vi::VBLANK.fetch_add(1, Ordering::Relaxed);
        // The overlay changed fps/vsync (or toggled fullscreen): re-read the
        // present rate/mode (R-a). Only after `present`, and only once the
        // acquired texture is DROPPED: wgpu rejects a reconfigure while the
        // surface output exists (`SurfaceOutput must be dropped…`, saw it
        // with Mailbox → Fifo). `present()` consumes the texture (its
        // output goes away with it); the view still holds a reference.
        drop(view);
        if g.present_dirty {
            g.present_dirty = false;
            let rate = g.refresh_present();
            queue_rate(&self.pending_rate, &mut self.last_rate, rate);
        }
        let elapsed = self.fps_timer.elapsed();
        if elapsed >= Duration::from_secs(1) {
            // Per second of wall time: redraws can be further apart than 1 s
            // (a slow GPU), and the raw count then overstates the rate.
            let fps = f64::from(self.frames) / elapsed.as_secs_f64();
            // U25: the fps suffix only while "Show FPS" is on; the title is
            // written on change only (show_fps flips live in the overlay).
            if g.show_fps {
                g.window.set_title(&format!("Birdman64 ({fps:.0} fps)"));
                g.title_fps = true;
            } else if g.title_fps {
                g.window.set_title("Birdman64");
                g.title_fps = false;
            }
            self.frames = 0;
            self.fps_timer = Instant::now();
            // W8: the monitor's refresh rate can change without a move
            // (display settings, a VRR/HDR toggle): re-read it about once a
            // second (the next frame applies it; unchanged readings queue
            // nothing and reconfigure nothing).
            g.present_dirty = true;
        }
        if overlay_closed {
            self.close_settings();
            // U3: a quit confirmed on the overlay's Quit row. The D2
            // auto-save inside close() has written the staged settings by
            // now, so exiting here is a clean exit (exit code 0).
            if self.overlay.as_mut().is_some_and(|o| o.take_quit()) {
                quit(0);
            }
            // U16: "Restart now" on the Graphics page's restart banner. The
            // overlay has saved (D2) and spawned the new copy of this exe;
            // exiting here hands over to it.
            if self.overlay.as_mut().is_some_and(|o| o.take_restart()) {
                quit(0);
            }
        }
    }

    /// U24: applies the cursor wish ([`should_hide_cursor`]); called from
    /// the redraw path, so no timer or thread is involved. The
    /// `set_cursor_visible` call only happens on a change.
    fn update_cursor(&mut self) {
        let Some(g) = &self.gpu else { return };
        let fullscreen = g.window.fullscreen().is_some();
        let idle = self.cursor_moved.elapsed() >= CURSOR_HIDE_AFTER;
        let hide = should_hide_cursor(fullscreen, settings::is_open(), !self.game_started(), idle);
        if hide == self.cursor_shown {
            g.window.set_cursor_visible(!hide);
            self.cursor_shown = !hide;
        }
    }

    /// U24: show the cursor at once (mouse move, the overlay opening,
    /// focus loss). `set_cursor_visible` only on a change.
    fn show_cursor(&mut self) {
        if let Some(g) = &self.gpu
            && !self.cursor_shown
        {
            g.window.set_cursor_visible(true);
            self.cursor_shown = true;
        }
    }

    /// False while the ROM screen or the first-run setup screen is up (no
    /// game thread yet).
    fn game_started(&self) -> bool {
        #[cfg(feature = "first-run")]
        let setup = self.setup.is_some();
        #[cfg(not(feature = "first-run"))]
        let setup = false;
        self.rom_screen.is_none() && !setup
    }

    /// Opens/closes the settings overlay: pause the OS core (threads sit in
    /// their waits; the clock freezes) and hand the pads to the overlay.
    fn toggle_settings(&mut self) {
        // First-run setup screen: no game to pause yet.
        if !self.game_started() {
            return;
        }
        if settings::is_open() {
            self.close_settings();
            return;
        }
        self.open_settings();
    }

    /// Opens the overlay (U26's `auto_pause` uses it too; Esc and the pad
    /// trigger go through `toggle_settings`): the OS core pauses, the pads
    /// move to the overlay. The cursor comes back at once (U24).
    fn open_settings(&mut self) {
        // The cursor comes back at once (U24; before the gpu borrow below).
        self.show_cursor();
        // No window yet: nothing could draw the overlay, and a paused game
        // with no visible way out would look hung.
        let Some(g) = &self.gpu else { return };
        // Nothing sticks while the overlay owns the input.
        input::release_all_keys();
        os::set_paused(true);
        self.overlay
            .get_or_insert_with(settings::Overlay::new)
            .open(g);
        g.window.request_redraw();
    }

    /// U26: a background event (window focus lost, last controller
    /// disconnected) opens the settings overlay, which pauses the game.
    /// The player resumes with Esc / B / Resume; the overlay stays open
    /// when focus returns.
    fn auto_pause(&mut self) {
        let enabled = self.gpu.as_ref().is_some_and(|g| g.pause_background);
        if !should_auto_pause(
            enabled,
            settings::is_open(),
            self.game_started(),
            crate::rom_setup::is_interactive(),
        ) {
            return;
        }
        self.open_settings();
    }

    /// W2: input reached the open overlay. It normally keeps redrawing by
    /// itself, but a dropped surface frame (Timeout / Lost) ends that chain;
    /// any input restarts it.
    fn request_overlay_redraw(&self) {
        if let Some(g) = &self.gpu {
            g.window.request_redraw();
        }
    }

    fn close_settings(&mut self) {
        if let Some(o) = &mut self.overlay {
            o.close(); // flips the flag + hands the pads back
        }
        os::set_paused(false);
    }

    /// Re-reads the present rate + mode after a monitor change or a live
    /// fps/vsync change (R-a, R-b, R-e): every display change goes through
    /// [`apply_present`] so the kernel's tick source and the surface's
    /// present mode follow without a restart.
    fn apply_present(&mut self) {
        let Some(g) = &mut self.gpu else { return };
        let rate = g.refresh_present();
        queue_rate(&self.pending_rate, &mut self.last_rate, rate);
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.gpu.is_none() {
            self.start(el);
        }
    }

    fn user_event(&mut self, _: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Frame => {
                self.drain();
                if let Some(g) = &self.gpu {
                    g.window.request_redraw();
                }
            }
            UserEvent::Stopped(code, dump) => {
                // U14: a non-zero stop is an OS core deadlock. Report it
                // like a crash (append to crash.log + the report box,
                // interactive only) before exiting.
                if code != 0 {
                    crate::report_deadlock(&dump);
                }
                eprintln!("[window] game stopped; exiting");
                quit(code);
            }
            UserEvent::Settings => self.toggle_settings(),
            UserEvent::Pause => self.auto_pause(),
            UserEvent::Toast(text) => self.toast.show(&text, 6),
            #[cfg(feature = "first-run")]
            UserEvent::Setup => self.setup_event(),
        }
    }

    /// First run: repaint the setup screen ~10×/s between progress reports
    /// (the bar animates; a download reports nothing for seconds).
    #[cfg(feature = "first-run")]
    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        use winit::event_loop::ControlFlow;
        if self.setup.is_none() {
            el.set_control_flow(ControlFlow::Wait);
            return;
        }
        let now = Instant::now();
        if now >= self.setup_frame {
            self.setup_frame = now + Duration::from_millis(100);
            if let Some(g) = &self.gpu {
                g.window.request_redraw();
            }
        }
        el.set_control_flow(ControlFlow::WaitUntil(self.setup_frame));
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                // No game thread to park during setup (the build worker and
                // its compiler processes simply end with the process).
                if !self.game_started() {
                    PARKED.store(true, Ordering::Release);
                }
                // S3/P8: the X / Alt+F4 with the overlay open is a close
                // path too: auto-save the menu changes (D2) and unpause, so
                // the game thread reaches a park point (the pause loop
                // parks on QUIT as well) and `quit` needn't time out.
                if settings::is_open() {
                    self.close_settings();
                }
                el.exit()
            }
            WindowEvent::Resized(s) => {
                if let Some(g) = &mut self.gpu {
                    g.config.width = s.width.max(1);
                    g.config.height = s.height.max(1);
                    g.surface.configure(&g.device, &g.config);
                    g.window.request_redraw();
                }
            }
            WindowEvent::Moved(_) => self.apply_present(),
            WindowEvent::ScaleFactorChanged { .. } => self.apply_present(),
            WindowEvent::Focused(focused) => {
                // W7: pads drive neither the game nor the settings trigger
                // while another window has the focus.
                input::set_focused(focused);
                if !focused {
                    input::release_all_keys();
                    // U24: focus loss shows the cursor (the next mouse move
                    // would anyway, but the pointer may sit over another
                    // window for a long time).
                    self.show_cursor();
                    // U26: pausing when the window goes to the background is
                    // the player's choice (auto_pause gates it). Not within
                    // 1 s of a display mode switch (window creation, F11,
                    // exclusive re-entry): those bounce the focus (W1's
                    // rule in `focus_changed`).
                    if self
                        .gpu
                        .as_ref()
                        .is_some_and(|g| g.mode_set.elapsed() >= Duration::from_secs(1))
                    {
                        self.auto_pause();
                    }
                }
                // W1: leave / re-enter exclusive fullscreen with the focus.
                if self.gpu.as_mut().is_some_and(|g| g.focus_changed(focused)) {
                    self.apply_present();
                }
            }
            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
            WindowEvent::CursorMoved { position, .. } => {
                let pos = (position.x as f32, position.y as f32);
                // U24: any mouse movement shows the cursor again at once.
                // Only a real move: Windows can post a synthetic same-spot
                // WM_MOUSEMOVE when the cursor visibility changes, which
                // would un-hide it right away every time.
                if pos != self.cursor {
                    self.cursor_moved = Instant::now();
                    self.show_cursor();
                }
                self.cursor = pos;
                if let (Some(s), Some(g)) = (&mut self.rom_screen, &self.gpu) {
                    s.pointer_moved(self.cursor.0, self.cursor.1, g.window.scale_factor() as f32);
                    g.window.request_redraw();
                }
                #[cfg(feature = "first-run")]
                if let (Some(s), Some(g)) = (&mut self.setup, &self.gpu) {
                    s.pointer_moved(self.cursor.0, self.cursor.1, g.window.scale_factor() as f32);
                }
                if let (true, Some(o)) = (settings::is_open(), &mut self.overlay) {
                    o.pointer_moved(self.cursor.0, self.cursor.1);
                    self.request_overlay_redraw();
                }
            }
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } => {
                if let (Some(s), Some(g)) = (&mut self.rom_screen, &self.gpu) {
                    let (x, y) = self.cursor;
                    let pressed = state == ElementState::Pressed;
                    s.pointer_button(x, y, g.window.scale_factor() as f32, pressed);
                    g.window.request_redraw();
                }
                #[cfg(feature = "first-run")]
                if let (Some(s), Some(g)) = (&mut self.setup, &self.gpu) {
                    let (x, y) = self.cursor;
                    let pressed = state == ElementState::Pressed;
                    s.pointer_button(x, y, g.window.scale_factor() as f32, pressed);
                    g.window.request_redraw();
                }
                if let (true, Some(o)) = (settings::is_open(), &mut self.overlay) {
                    let pressed = state == ElementState::Pressed;
                    o.pointer_button(self.cursor.0, self.cursor.1, pressed);
                    self.request_overlay_redraw();
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let PhysicalKey::Code(code) = event.physical_key else {
                    return;
                };
                let pressed = event.state == ElementState::Pressed;
                let alt_enter = self.modifiers.alt_key()
                    && matches!(code, KeyCode::Enter | KeyCode::NumpadEnter);
                if pressed && !event.repeat && (code == KeyCode::F11 || alt_enter) {
                    if let Some(g) = &mut self.gpu {
                        g.toggle_fullscreen();
                    }
                    self.apply_present();
                    return;
                }
                // The ROM screen takes the keyboard (Enter / Esc).
                if let Some(s) = &mut self.rom_screen {
                    if pressed && !event.repeat {
                        s.on_key(code);
                        if let Some(g) = &self.gpu {
                            g.window.request_redraw();
                        }
                    }
                    return;
                }
                // First run: the setup screen takes the keyboard (Enter /
                // Esc on its error screen); no game is running yet.
                #[cfg(feature = "first-run")]
                if let Some(s) = &mut self.setup {
                    if pressed && !event.repeat {
                        s.on_key(code);
                    }
                    return;
                }
                // U4/D6: Escape opens settings (the key every PC player
                // tries), unless the player bound Escape to a game slot in
                // pw64.toml. While the overlay is open, Escape keeps its
                // overlay meaning (back / close) further down. The bound
                // check is the specified `input::key_is_bound` (a key is
                // bound iff some slot maps it); switch the call to
                // `input::key_is_bound` once that helper lands in input.rs.
                if pressed
                    && !event.repeat
                    && code == KeyCode::Escape
                    && !settings::is_open()
                    && input::key_mapping(KeyCode::Escape).is_none()
                {
                    self.toggle_settings();
                    return;
                }
                // The settings trigger: the overlay owns it (open or closed).
                if pressed && !event.repeat && code == settings::key() {
                    self.toggle_settings();
                    return;
                }
                // While the overlay is open it consumes the keyboard; the
                // game (paused) gets nothing.
                if settings::is_open() {
                    if let Some(o) = &mut self.overlay {
                        o.on_key(code, event.repeat, pressed);
                    }
                    self.request_overlay_redraw();
                    return;
                }
                if !alt_enter {
                    input::on_key(code, pressed);
                }
            }
            // A file dropped onto the window: the ROM screen's way in.
            WindowEvent::DroppedFile(path) => {
                if let (Some(s), Some(g)) = (&mut self.rom_screen, &self.gpu) {
                    s.dropped(path);
                    g.window.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => {
                if self.rom_screen.is_some() {
                    self.redraw_rom_screen();
                    return;
                }
                #[cfg(feature = "first-run")]
                if self.setup.is_some() {
                    if !self.redraw_setup() {
                        // Quit on the error screen: nothing is running.
                        std::process::exit(1);
                    }
                    return;
                }
                self.redraw()
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Gpu, first_window_size, should_auto_pause, should_hide_cursor};

    /// U24: the cursor hides only while playing fullscreen with the
    /// overlay closed and the mouse idle; the setup screen keeps it.
    #[test]
    fn cursor_hides_fullscreen_idle_only() {
        assert!(should_hide_cursor(true, false, false, true));
        // Not fullscreen (windowed, or borderless lost to a focus bounce).
        assert!(!should_hide_cursor(false, false, false, true));
        // Overlay open (the mouse is used there).
        assert!(!should_hide_cursor(true, true, false, true));
        // Setup screen (first run).
        assert!(!should_hide_cursor(true, false, true, true));
        // The mouse just moved: it stays visible for the grace period.
        assert!(!should_hide_cursor(true, false, false, false));
    }

    /// U26: the background-pause gates (the player's setting, a running
    /// game, a closed overlay, no automation).
    #[test]
    fn auto_pause_gates() {
        assert!(should_auto_pause(true, false, true, true));
        assert!(
            !should_auto_pause(false, false, true, true),
            "the row is off"
        );
        assert!(
            !should_auto_pause(true, true, true, true),
            "the overlay is already open"
        );
        assert!(
            !should_auto_pause(true, false, false, true),
            "the setup screen is up"
        );
        assert!(
            !should_auto_pause(true, false, true, false),
            "never in automation"
        );
    }

    /// The render-scale target sizes (S6): exact at 1.0, upscaled below 1
    /// (blurry blit), supersampled above 1, clamped to the max texture size
    /// with the aspect kept.
    #[test]
    fn fb_size_for() {
        // Exact at 1.0 whatever the limit (no float rounding).
        assert_eq!(Gpu::fb_size_for(1.0, (960, 720), 8192), (960, 720));
        // Below 1: render small, `fb_present` upscales with the filter.
        assert_eq!(Gpu::fb_size_for(0.5, (960, 720), 8192), (480, 360));
        // The 0.5 floor (a hand-edited 0.25 config value clamps).
        assert_eq!(Gpu::fb_size_for(0.25, (960, 720), 8192), (480, 360));
        // Above 1: supersample.
        assert_eq!(Gpu::fb_size_for(2.0, (960, 720), 8192), (1920, 1440));
        // Clamped to the max texture size, aspect kept (960·3, 720·3).
        assert_eq!(Gpu::fb_size_for(4.0, (960, 720), 2880), (2880, 2160));
    }

    /// U19: the starting window is the largest game-aspect window that
    /// fits 80% of the monitor's height (and its width).
    #[test]
    fn first_window_fits_80_percent_of_the_height() {
        // 1080p at 100%: 864 logical high, 4:3 wide.
        assert_eq!(
            first_window_size((1920, 1080), 1.0, 4.0 / 3.0),
            (1152.0, 864.0)
        );
        // 4K at 150%: 0.8 * 2160 / 1.5 = 1152 high.
        assert_eq!(
            first_window_size((3840, 2160), 1.5, 4.0 / 3.0),
            (1536.0, 1152.0)
        );
        // Widescreen aspect keeps the same height rule.
        assert_eq!(
            first_window_size((1920, 1080), 1.0, 16.0 / 9.0),
            (1536.0, 864.0)
        );
        // Small screens stay on screen (no 960x720 floor): 1366x768, and
        // the common 1080p laptop at 150% (720 logical px high).
        assert_eq!(
            first_window_size((1366, 768), 1.0, 4.0 / 3.0),
            (819.0, 614.0)
        );
        assert_eq!(
            first_window_size((1920, 1080), 1.5, 4.0 / 3.0),
            (768.0, 576.0)
        );
        // High DPI (200%): physical px halve.
        assert_eq!(
            first_window_size((1920, 1080), 2.0, 4.0 / 3.0),
            (576.0, 432.0)
        );
        // Portrait monitor: the game aspect must fit the width instead
        // (the window must not spill off the screen).
        assert_eq!(
            first_window_size((1080, 1920), 1.0, 4.0 / 3.0),
            (1080.0, 810.0)
        );
        // A broken scale factor (0) falls back to 1.
        assert_eq!(
            first_window_size((1920, 1080), 0.0, 4.0 / 3.0),
            (1152.0, 864.0)
        );
    }
}
