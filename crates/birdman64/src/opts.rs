//! Render-quality option parsing, shared by the windowed renderer
//! (`window.rs`) and the PNG dump renderer (`hle.rs`). Parsed per call site
//! at startup. Every option resolves in the same order: `PW64_*` env var →
//! `pw64.toml` (`config.rs`) → built-in default.

/// Precedence for every option: the env var wins over the config file, which
/// wins over the built-in default. (Env vars that are present but invalid
/// warn at their call site and then take this same fallback chain — a typo
/// on the command line shouldn't disable a good config value.)
pub(crate) fn precedence<T>(env: Option<T>, cfg: Option<T>, default: T) -> T {
    env.or(cfg).unwrap_or(default)
}

/// The built-in defaults for the settings keys, in the value spelling
/// `pw64.toml` and the settings screen use. `config::builtin_default` takes
/// its comparison values from here (a saved key equal to its default is
/// removed, U18), and each option below reads its default from the same
/// constant, so the two can't drift apart.
pub(crate) const MSAA_DEFAULT: u32 = 1;
pub(crate) const SCALE_DEFAULT: f32 = 1.0;
pub(crate) const SCALE_FILTER_DEFAULT: &str = "linear";
pub(crate) const TEX_FILTER_DEFAULT: &str = "bilinear";
pub(crate) const VSYNC_DEFAULT: bool = false;
pub(crate) const DISPLAY_MODE_DEFAULT: &str = "windowed";

/// Low-end preset defaults (L1), kept next to the plain ones so
/// `config::builtin_default` compares against exactly what the game runs.
pub(crate) const SCALE_LOW_END: f32 = 0.75;
pub(crate) const FPS_LOW_END_HZ: u32 = 60;

/// Low-end preset (ga-polish L1): the adapter is a software rasteriser, so
/// the built-in defaults drop (60 fps, 75% render resolution, MSAA 1 — that
/// default already). Set once from `window::start`, before the first
/// `fps`/`scale` read; env vars and `pw64.toml` values sit on top of the
/// default in `precedence`, so explicit settings are never overridden.
static LOW_END: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Marks this process as running on a software rasteriser
/// (`low_end_adapter`).
pub fn set_low_end() {
    LOW_END.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Is the low-end preset active?
pub(crate) fn low_end() -> bool {
    LOW_END.load(std::sync::atomic::Ordering::Relaxed)
}

/// Is this adapter a software rasteriser? `DeviceType::Cpu` is the direct
/// report; llvmpipe, softpipe, SwiftShader and WARP (the "Microsoft Basic
/// Render Driver") report `Other` or an ordinary type but are recognisable
/// by name. Integrated GPUs get no preset: the defaults are already gentle
/// (MSAA 1, 100% render resolution) and `fps = monitor` is right there.
pub fn low_end_adapter(info: &wgpu::AdapterInfo) -> bool {
    if info.device_type == wgpu::DeviceType::Cpu {
        return true;
    }
    let name = info.name.to_ascii_lowercase();
    [
        "llvmpipe",
        "softpipe",
        "swiftshader",
        "microsoft basic render",
    ]
    .iter()
    .any(|s| name.contains(s))
}

/// Reads env var `name` through `parse` (value trimmed). Absent → `None`;
/// present but rejected by `parse` → warns and `None`, so the caller falls
/// through to the config value (`precedence`).
fn env_opt<T>(name: &str, hint: &str, parse: impl FnOnce(&str) -> Option<T>) -> Option<T> {
    let v = std::env::var(name).ok()?;
    let r = parse(v.trim());
    if r.is_none() {
        eprintln!("[pw64] {name}={v:?} not understood ({hint}); ignoring");
    }
    r
}

/// `PW64_MSAA=<1|4|8>` or `[graphics] msaa = 4`: MSAA sample count for the
/// windowed surface and the frame-dump renderer. Default 1.
pub fn msaa() -> u32 {
    let env = env_opt("PW64_MSAA", "use 1, 4 or 8", |v| {
        v.parse().ok().filter(|n| matches!(n, 1 | 4 | 8))
    });
    precedence(env, crate::config::get().graphics.msaa, MSAA_DEFAULT)
}

/// Does the adapter MSAA `n` samples in `format` and in the renderer's
/// depth-stencil format? (wgpu format-feature flags; the renderer's color and
/// depth targets both use this sample count. Depth format mirrors
/// `pw64_gfx::device_descriptor` + `renderer::depth_format`.)
pub fn format_supports_msaa(adapter: &wgpu::Adapter, format: wgpu::TextureFormat, n: u32) -> bool {
    let depth = if adapter
        .features()
        .contains(wgpu::Features::DEPTH32FLOAT_STENCIL8)
    {
        wgpu::TextureFormat::Depth32FloatStencil8
    } else {
        wgpu::TextureFormat::Depth24PlusStencil8
    };
    // wgpu validates against the adapter's flags only when the device has
    // TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES; otherwise the WebGPU
    // guarantee (1 and 4 samples) applies — 8 would panic at texture creation.
    let features = pw64_gfx::device_descriptor(adapter).required_features;
    let specific = features.contains(wgpu::Features::TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES);
    [format, depth].iter().all(|&f| {
        let flags = if specific {
            adapter.get_texture_format_features(f).flags
        } else {
            f.guaranteed_format_features(features).flags
        };
        flags.sample_count_supported(n)
    })
}

/// Picks the real MSAA sample count: `requested` if `format` supports it, else
/// the highest supported count below it (8 → 4 → 1). Prints the choice when it
/// differs from the request.
pub fn resolve_msaa(adapter: &wgpu::Adapter, format: wgpu::TextureFormat, requested: u32) -> u32 {
    let n = [8, 4, 1]
        .into_iter()
        .find(|&n| n <= requested && format_supports_msaa(adapter, format, n))
        .unwrap_or(1);
    if n != requested {
        eprintln!("[pw64] MSAA {requested}× not supported for {format:?} here; using {n}×");
    }
    n
}

/// `PW64_SCALE=<f>` or `[graphics] scale = 2.0`: render scale for the
/// windowed renderer — render at window_size × f and blit the result to the
/// surface (`fb_present`: down with the filter = supersampling, up =
/// upscale). Default 1 (the window size exactly); 0.5..1 is allowed
/// (blurry), useful for slow GPUs.
pub fn scale() -> f32 {
    let env = env_opt("PW64_SCALE", "a float ≥ 0.5", |v| {
        v.parse().ok().filter(|f: &f32| f.is_finite() && *f >= 0.5)
    });
    // Low-end preset (L1): 75% render resolution by default.
    let default = if low_end() {
        SCALE_LOW_END
    } else {
        SCALE_DEFAULT
    };
    precedence(env, crate::config::get().graphics.scale, default)
}

/// `"linear"` / `"nearest"` → blit filter (env var and config share this).
pub(crate) fn parse_filter(v: &str) -> Option<wgpu::FilterMode> {
    match v.trim().to_ascii_lowercase().as_str() {
        "linear" => Some(wgpu::FilterMode::Linear),
        "nearest" => Some(wgpu::FilterMode::Nearest),
        _ => None,
    }
}

/// `PW64_SCALE_FILTER=<linear|nearest>` or `[graphics] scale_filter`: filter
/// for the scale blit (default linear).
pub fn scale_filter() -> wgpu::FilterMode {
    let env = env_opt("PW64_SCALE_FILTER", "use linear or nearest", parse_filter);
    // Config values were validated at load.
    let cfg = crate::config::get()
        .graphics
        .scale_filter
        .as_deref()
        .and_then(parse_filter);
    precedence(env, cfg, parse_filter(SCALE_FILTER_DEFAULT).unwrap())
}

/// `"bilinear"` / `"n64"` → texture filter for bilinear tiles (env var and
/// config share this).
pub(crate) fn parse_tex_filter(v: &str) -> Option<pw64_gfx::TexFilter> {
    match v.trim().to_ascii_lowercase().as_str() {
        "bilinear" => Some(pw64_gfx::TexFilter::Bilinear),
        "n64" => Some(pw64_gfx::TexFilter::N64),
        _ => None,
    }
}

/// `PW64_FILTER=<bilinear|n64>` or `[graphics] filter`: how `G_TF_BILERP`
/// tiles are filtered — GPU bilinear (default) or the RDP's 3-point filter
/// (`pw64_gfx::TexFilter`). Window + PNG dumps; the settings screen changes
/// the window's live.
pub fn tex_filter() -> pw64_gfx::TexFilter {
    let env = env_opt("PW64_FILTER", "use bilinear or n64", parse_tex_filter);
    // Config values were validated at load.
    let cfg = crate::config::get()
        .graphics
        .filter
        .as_deref()
        .and_then(parse_tex_filter);
    precedence(env, cfg, parse_tex_filter(TEX_FILTER_DEFAULT).unwrap())
}

/// `PW64_WIDESCREEN` or `[graphics] widescreen`: output aspect for Hor+
/// widescreen. `1` → 16:9, `<w>:<h>` (or a plain ratio like `2.33`) → that
/// aspect; `0`/absent → the default (D3): the monitor's aspect when it is
/// wider than 4:3 (clamped to 21:9), else 4:3 (`None`). An invalid env value
/// warns and falls through to the config (`precedence`); `0` in the env
/// explicitly turns a configured widescreen off. Shared by the window, the
/// dumps and the C side (culling, `pw64_game::set_widescreen_aspect`).
pub fn widescreen() -> Option<f32> {
    let env = env_opt(
        "PW64_WIDESCREEN",
        "use 0, 1, 16:9, 21:9, … (wider than 4:3, ≤ 8)",
        parse_widescreen,
    );
    // Config values were validated at load.
    let cfg = crate::config::get()
        .graphics
        .widescreen
        .as_deref()
        .and_then(parse_widescreen);
    precedence(env, cfg, widescreen_default())
}

/// The monitor-aspect default behind `widescreen()` (D3). Unset (`None`):
/// the default is 4:3 — headless runs and PNG dumps never set it, so they
/// stay 4:3 unless `PW64_WIDESCREEN` says otherwise. The window sets it
/// from the monitor's size before the first `widescreen()` read.
static MONITOR_ASPECT: std::sync::Mutex<Option<f32>> = std::sync::Mutex::new(None);

/// Feeds the monitor's size into the [`widescreen`] default (D3). Call
/// before the first `widescreen()` read (the window's startup does).
pub(crate) fn set_widescreen_monitor_default(w: u32, h: u32) {
    *MONITOR_ASPECT.lock().unwrap() = monitor_widescreen_default(w, h);
}

/// The widescreen default for a monitor `w` x `h` px (D3): its aspect when
/// it is wider than 4:3, clamped to 21:9; a 4:3 or 5:4 monitor (and a
/// portrait one) keeps the original 4:3 look (`None`).
pub(crate) fn monitor_widescreen_default(w: u32, h: u32) -> Option<f32> {
    let a = w as f32 / h as f32;
    (a > 4.0 / 3.0).then(|| a.min(21.0 / 9.0))
}

/// The current monitor-aspect default (`None` before the window set it,
/// which reads as "stay 4:3").
pub(crate) fn widescreen_default() -> Option<f32> {
    *MONITOR_ASPECT.lock().unwrap()
}

/// One widescreen setting: `Some(None)` = explicitly off (`0` / empty),
/// `Some(Some(aspect))` = on, `None` = invalid.
pub(crate) fn parse_widescreen(v: &str) -> Option<Option<f32>> {
    match v.trim() {
        "" | "0" => Some(None),
        v => parse_aspect(v).map(Some),
    }
}

/// `PW64_FILL_SCREEN=<0|1>` or `[graphics] fill_screen = true|false`: the
/// game's own letterbox bars around world views go away (renderer.md "Fill
/// screen"). Default: on iff widescreen is on (a wider image without the
/// matching vertical un-crop looks wrong). Restart-only (the C side and the
/// renderer are built from it at boot).
pub fn fill_screen() -> bool {
    let env = env_opt("PW64_FILL_SCREEN", "use 0 or 1", parse_vsync);
    // Config values were validated at load.
    precedence(
        env,
        crate::config::get().graphics.fill_screen,
        fill_default(widescreen()),
    )
}

/// The default for `fill_screen` given the widescreen setting.
pub(crate) fn fill_default(widescreen: Option<f32>) -> bool {
    widescreen.is_some()
}

/// OLED care (O3). `PW64_OLED=<0|1>` or `[oled] drift`: the HUD drifts
/// (spreads burn-in). The env var presets the quick care mode: drift on
/// and the brightness at most 0.8 unless the config dimmed further.
pub fn oled_drift() -> bool {
    let env = env_opt("PW64_OLED", "use 0 or 1", parse_vsync);
    precedence(env, Some(crate::config::get().oled.drift), false)
}

/// `PW64_OLED=1` presets: drift on plus this brightness cap.
const OLED_ENV_BRIGHTNESS: f32 = 0.8;

/// `[oled] brightness` (the settings screen stages it live): the HUD
/// brightness multiplier, 1 = off.
pub fn oled_brightness() -> f32 {
    let cfg = crate::config::get().oled.brightness;
    // Only the env preset caps it: drift turned on in the settings screen
    // (`[oled] drift = true`) must keep the brightness the player chose.
    if env_opt("PW64_OLED", "use 0 or 1", parse_vsync) == Some(true) {
        cfg.min(OLED_ENV_BRIGHTNESS)
    } else {
        cfg
    }
}

/// The game's frame rate (framerate.md "Design"): the OS core's present
/// tick. VI retraces (audio, input scripts, timeouts) stay 60 Hz.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fps {
    /// The window's monitor refresh rate (headless: 60).
    Monitor,
    /// A fixed rate, 30..=1000 Hz.
    Hz(u32),
    /// As fast as frames finish (≥ 2 ms apart).
    Uncapped,
}

impl Fps {
    /// The `pw64.toml` / `PW64_FPS` spelling (`parse_fps` round-trips it).
    pub fn to_config(self) -> String {
        match self {
            Self::Monitor => "monitor".into(),
            Self::Hz(n) => n.to_string(),
            Self::Uncapped => "0".into(),
        }
    }
}

/// `monitor`, `0` / `uncapped`, or an integer rate 30..=1000.
pub(crate) fn parse_fps(v: &str) -> Option<Fps> {
    match v.trim().to_ascii_lowercase().as_str() {
        "monitor" => Some(Fps::Monitor),
        "0" | "uncapped" => Some(Fps::Uncapped),
        n => n
            .parse::<u32>()
            .ok()
            .filter(|n| (30..=1000).contains(n))
            .map(Fps::Hz),
    }
}

/// `PW64_FPS=<monitor|N|0>` or `[graphics] fps`: the game's frame rate.
/// Default: `monitor` in the window, 60 headless (scripted runs stay
/// comparable with older ones).
pub fn fps(windowed: bool) -> Fps {
    let env = env_opt(
        "PW64_FPS",
        "use monitor, 0 (uncapped) or 30..1000",
        parse_fps,
    );
    // Config values were validated at load.
    let cfg = crate::config::get()
        .graphics
        .fps
        .as_deref()
        .and_then(parse_fps);
    let default = if windowed { Fps::Monitor } else { Fps::Hz(60) };
    // Low-end preset (L1): 60 fps, whatever the window/headless split.
    let default = if low_end() {
        Fps::Hz(FPS_LOW_END_HZ)
    } else {
        default
    };
    precedence(env, cfg, default)
}

/// `PW64_VSYNC=<0|1>` or `[graphics] vsync = true|false`: display-paced
/// present ticks (V-Sync, the matrix in framerate.md "Present tick"). Default
/// **off** (user decision 2026-09-29): V-Sync removes the monitor-rate beat's
/// micro-judder but adds input latency, so the `fps` cap stays the main
/// control. Restart-only (window surface mode + the OS core's tick source).
pub fn vsync() -> bool {
    let env = env_opt("PW64_VSYNC", "use 0 or 1", parse_vsync);
    // Config values were validated at load.
    precedence(
        env,
        Some(crate::config::get().graphics.vsync),
        VSYNC_DEFAULT,
    )
}

/// `1` = on, `0` = off.
pub(crate) fn parse_vsync(v: &str) -> Option<bool> {
    match v.trim() {
        "1" => Some(true),
        "0" => Some(false),
        _ => None,
    }
}

/// U25: the window title shows the frame rate. `[graphics] show_fps`
/// (settings screen; no env var), default off.
pub fn show_fps() -> bool {
    crate::config::get().graphics.show_fps
}

/// U26: open the settings overlay when the window loses focus or the last
/// controller disconnects. `[ui] pause_in_background` (settings screen; no
/// env var), default on.
pub fn pause_in_background() -> bool {
    crate::config::get().ui.pause_in_background
}

/// The OS core's present rate for `fps` (`monitor_mhz`: the window's
/// monitor refresh in millihertz, if known). ~60 Hz → `None`: the hardware
/// path exactly (swaps latch at VI retraces, the scheduler starts gfx there).
/// With `vsync` (window only; headless ignores it) the display paces the
/// ticks instead — the matrix in framerate.md "Present tick".
pub fn present_rate(
    fps: Fps,
    monitor_mhz: Option<u32>,
    vsync: bool,
) -> Option<pw64_platform::os::vi::PresentRate> {
    use pw64_platform::os::vi::PresentRate;
    if vsync {
        // Any `fps`: the display paces the ticks; its rate is only the
        // fallback for a silent window (headless, minimised, dumps).
        let mon = monitor_mhz.unwrap_or(60_000).clamp(30_000, 1_000_000);
        return Some(PresentRate::display_from_millihertz(mon));
    }
    let mhz = match fps {
        Fps::Uncapped => return Some(PresentRate::Uncapped),
        Fps::Hz(n) => n * 1000,
        // Unknown refresh → 60; clamp odd reports into the accepted range.
        Fps::Monitor => monitor_mhz.unwrap_or(60_000).clamp(30_000, 1_000_000),
    };
    // 59.94 Hz panels too: the old path is exact there, and a 0.06 Hz beat
    // against the 60 Hz VI would only drop a frame every ~17 s.
    if (59_500..=60_500).contains(&mhz) {
        None
    } else {
        Some(PresentRate::from_millihertz(mhz))
    }
}

/// May frames be presented with tearing? Only when the player asked for more
/// than the display shows (uncapped, or a fixed rate above the monitor's
/// refresh with a 1 Hz slack for the odd reports), where tearing is the expected
/// price of not blocking (as in any PC game with V-Sync off). An unknown
/// refresh rate never allows it (`Monitor` never asks for more than it shows).
pub fn allows_tearing(fps: Fps, monitor_mhz: Option<u32>) -> bool {
    match fps {
        Fps::Monitor => false,
        Fps::Uncapped => true,
        Fps::Hz(n) => monitor_mhz.is_some_and(|mhz| n * 1000 > mhz + 1000),
    }
}

/// Logged once: Mailbox (the mode every free-running display wants) is
/// unavailable on this surface, and what took its place.
static PRESENT_FALLBACK: std::sync::Once = std::sync::Once::new();

/// The window's present mode for `vsync` / a present rate (matrix in
/// framerate.md "Present tick"), given the surface's supported modes:
/// V-Sync → Fifo (no tearing, blocks the window thread per present);
/// otherwise with a present tick, Mailbox (newest frame, never blocks) if
/// supported, else Immediate but only when `allows_tearing` (the player
/// asked for more than the display shows), else Fifo; the 60 Hz VI path
/// keeps AutoVsync (Fifo) as before.
pub fn present_mode(
    vsync: bool,
    has_rate: bool,
    allow_tearing: bool,
    supported: &[wgpu::PresentMode],
) -> wgpu::PresentMode {
    if vsync {
        return wgpu::PresentMode::Fifo;
    }
    if has_rate {
        if supported.contains(&wgpu::PresentMode::Mailbox) {
            return wgpu::PresentMode::Mailbox;
        }
        let immediate = allow_tearing && supported.contains(&wgpu::PresentMode::Immediate);
        PRESENT_FALLBACK.call_once(|| {
            eprintln!(
                "[opts] Mailbox present mode not supported here; using {}",
                if immediate { "Immediate" } else { "Fifo" }
            );
        });
        return if immediate {
            wgpu::PresentMode::Immediate
        } else {
            wgpu::PresentMode::Fifo
        };
    }
    wgpu::PresentMode::AutoVsync
}

/// How the window shows fullscreen (`PW64_DISPLAY_MODE`, the settings
/// screen's Display mode row). `PartialOrd`/`Ord` give the settings row its
/// cycle order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DisplayMode {
    /// A normal window.
    Windowed,
    /// Fullscreen without switching the monitor's video mode.
    Borderless,
    /// Fullscreen at one of the monitor's video modes (`fullscreen_resolution`).
    Exclusive,
}

impl DisplayMode {
    /// The `pw64.toml` / `PW64_DISPLAY_MODE` spelling (`parse_display_mode`
    /// round-trips it).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Windowed => "windowed",
            Self::Borderless => "borderless",
            Self::Exclusive => "exclusive",
        }
    }
}

/// `"windowed"`, `"borderless"` or `"exclusive"` (case-insensitive).
pub(crate) fn parse_display_mode(v: &str) -> Option<DisplayMode> {
    match v.trim().to_ascii_lowercase().as_str() {
        "windowed" => Some(DisplayMode::Windowed),
        "borderless" => Some(DisplayMode::Borderless),
        "exclusive" => Some(DisplayMode::Exclusive),
        _ => None,
    }
}

/// `PW64_DISPLAY_MODE=windowed|borderless|exclusive` or `[graphics]
/// display_mode`: how the window goes fullscreen. Default Windowed.
pub fn display_mode() -> DisplayMode {
    let env = env_opt(
        "PW64_DISPLAY_MODE",
        "use windowed, borderless or exclusive",
        parse_display_mode,
    );
    // Config values were validated at load.
    let cfg = crate::config::get()
        .graphics
        .display_mode
        .as_deref()
        .and_then(parse_display_mode);
    precedence(env, cfg, parse_display_mode(DISPLAY_MODE_DEFAULT).unwrap())
}

/// One fullscreen resolution: `w`x`h` at `hz` Hz (`None`: the monitor's
/// current rate).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Res {
    pub w: u32,
    pub h: u32,
    pub hz: Option<u32>,
}

impl Res {
    /// The `pw64.toml` / `PW64_FULLSCREEN_RES` spelling (`parse_resolution`
    /// round-trips it).
    pub fn to_config(self) -> String {
        match self.hz {
            Some(hz) => format!("{}x{}@{hz}", self.w, self.h),
            None => format!("{}x{}", self.w, self.h),
        }
    }
}

/// `"WxH"` or `"WxH@Hz"` (case-insensitive `x`, whitespace tolerated). Zero
/// sizes/rates and anything else unparsable are rejected.
pub(crate) fn parse_resolution(v: &str) -> Option<Res> {
    let v = v.trim();
    let (size, hz) = match v.split_once('@') {
        Some((s, h)) => {
            let hz = h
                .trim()
                .parse::<u32>()
                .ok()
                .filter(|&hz| hz > 0 && hz <= 1000)?;
            (s, Some(hz))
        }
        None => (v, None),
    };
    let (w, h) = size.split_once(['x', 'X'])?;
    let w = w.trim().parse::<u32>().ok().filter(|&w| w > 0)?;
    let h = h.trim().parse::<u32>().ok().filter(|&h| h > 0)?;
    Some(Res { w, h, hz })
}

/// `PW64_FULLSCREEN_RES=WxH[@Hz]` or `[graphics] fullscreen_resolution`: the
/// video mode an exclusive fullscreen uses (`None`: the monitor's current
/// mode). Only read when the display mode is exclusive.
pub fn fullscreen_resolution() -> Option<Res> {
    let env = env_opt(
        "PW64_FULLSCREEN_RES",
        "use WxH or WxH@Hz (no zeros)",
        parse_resolution,
    );
    // Config values were validated at load. `T = Option<Res>`: the default
    // itself is "no explicit mode" (the monitor's current one), so both the
    // env and the config values get wrapped in another `Some`.
    let cfg = crate::config::get()
        .graphics
        .fullscreen_resolution
        .as_deref()
        .and_then(parse_resolution);
    precedence(env.map(Some), cfg.map(Some), None)
}

/// `PW64_NO_AUDIO` or `[graphics] no_audio = true`: no output device (HLE
/// and AI timing still run).
pub fn no_audio() -> bool {
    std::env::var_os("PW64_NO_AUDIO").is_some() || crate::config::get().graphics.no_audio
}

fn parse_aspect(v: &str) -> Option<f32> {
    let v = v.trim();
    let a = match v.split_once(':') {
        Some((w, h)) => w.trim().parse::<f32>().ok()? / h.trim().parse::<f32>().ok()?,
        None if v == "1" => 16.0 / 9.0,
        None => v.parse::<f32>().ok()?,
    };
    (a.is_finite() && a > 4.0 / 3.0 + 1e-4 && a <= 8.0).then_some(a)
}

#[cfg(test)]
mod tests {
    use super::{
        Fps, fill_default, monitor_widescreen_default, parse_aspect, parse_filter, parse_fps,
        parse_tex_filter, parse_vsync, parse_widescreen, precedence, present_mode, present_rate,
    };
    use pw64_platform::os::vi::PresentRate;

    #[test]
    fn fps_parsing_and_rate() {
        assert_eq!(parse_fps(" Monitor "), Some(Fps::Monitor));
        assert_eq!(parse_fps("0"), Some(Fps::Uncapped));
        assert_eq!(parse_fps("uncapped"), Some(Fps::Uncapped));
        assert_eq!(parse_fps("144"), Some(Fps::Hz(144)));
        assert_eq!(parse_fps("29"), None);
        assert_eq!(parse_fps("1001"), None);
        assert_eq!(parse_fps("fast"), None);
        for f in [Fps::Monitor, Fps::Hz(144), Fps::Uncapped] {
            assert_eq!(parse_fps(&f.to_config()), Some(f));
        }
        // ~60 Hz keeps the VI path; others get a present tick.
        assert_eq!(present_rate(Fps::Hz(60), None, false), None);
        assert_eq!(present_rate(Fps::Monitor, None, false), None);
        assert_eq!(present_rate(Fps::Monitor, Some(59_940), false), None);
        assert_eq!(
            present_rate(Fps::Monitor, Some(144_000), false),
            Some(PresentRate::from_millihertz(144_000))
        );
        assert_eq!(
            present_rate(Fps::Hz(30), Some(144_000), false),
            Some(PresentRate::from_millihertz(30_000))
        );
        assert_eq!(
            present_rate(Fps::Uncapped, None, false),
            Some(PresentRate::Uncapped)
        );
    }

    /// The vsync × fps matrix (framerate.md "Present tick"), both the tick
    /// source and the window's present-mode choice.
    #[test]
    fn vsync_matrix() {
        use wgpu::PresentMode as Pm;
        let all_modes: Vec<wgpu::PresentMode> = [
            Pm::Fifo,
            Pm::Mailbox,
            Pm::Immediate,
            Pm::AutoVsync,
            Pm::AutoNoVsync,
        ]
        .into_iter()
        .collect();
        let only_fifo = vec![wgpu::PresentMode::Fifo];
        let fallback = PresentRate::display_from_millihertz(144_000);
        // vsync on: every fps paces on the display, fallback = monitor
        // (unknown monitor → 60 Hz).
        for f in [
            Fps::Monitor,
            Fps::Hz(30),
            Fps::Hz(60),
            Fps::Hz(144),
            Fps::Uncapped,
        ] {
            assert_eq!(present_rate(f, Some(144_000), true), Some(fallback));
            assert_eq!(
                present_rate(f, None, true),
                Some(PresentRate::display_from_millihertz(60_000)),
                "unknown monitor → 60 Hz fallback interval"
            );
            assert_eq!(
                present_mode(true, true, false, &all_modes),
                wgpu::PresentMode::Fifo
            );
        }
        // vsync off, present tick: Mailbox, else Immediate (only when
        // tearing is allowed), else Fifo.
        assert_eq!(
            present_mode(false, true, false, &all_modes),
            wgpu::PresentMode::Mailbox
        );
        assert_eq!(
            present_mode(false, true, true, &all_modes),
            wgpu::PresentMode::Mailbox
        );
        assert_eq!(
            present_mode(false, true, false, &only_fifo),
            wgpu::PresentMode::Fifo
        );
        let no_mailbox = vec![Pm::Immediate, Pm::Fifo];
        assert_eq!(
            present_mode(false, true, true, &no_mailbox),
            wgpu::PresentMode::Immediate
        );
        assert_eq!(
            present_mode(false, true, false, &no_mailbox),
            wgpu::PresentMode::Fifo
        );
        // vsync off, no tick (the 60 Hz VI path): AutoVsync as before.
        assert_eq!(
            present_mode(false, false, true, &all_modes),
            wgpu::PresentMode::AutoVsync
        );
    }

    /// Tearing is allowed only when the player asked for more than the
    /// display shows (uncapped, or fixed > refresh + 1 Hz).
    #[test]
    fn tearing_needs_more_than_the_display() {
        use super::allows_tearing;
        assert!(!allows_tearing(Fps::Monitor, Some(144_000)));
        assert!(!allows_tearing(Fps::Monitor, None));
        assert!(allows_tearing(Fps::Uncapped, None));
        assert!(allows_tearing(Fps::Uncapped, Some(60_000)));
        assert!(allows_tearing(Fps::Hz(144), Some(60_000)));
        // The +1 Hz slack: 61 fps on a 60 Hz display still counts as more.
        assert!(allows_tearing(Fps::Hz(62), Some(60_000)));
        assert!(!allows_tearing(Fps::Hz(60), Some(60_000)));
        assert!(!allows_tearing(Fps::Hz(60), Some(59_940)));
        assert!(!allows_tearing(Fps::Hz(60), Some(120_000)));
        // Unknown display: no tearing (falls back to Fifo).
        assert!(!allows_tearing(Fps::Hz(144), None));
    }

    #[test]
    fn vsync_parsing() {
        assert_eq!(parse_vsync("1"), Some(true));
        assert_eq!(parse_vsync(" 0 "), Some(false));
        assert_eq!(parse_vsync("true"), None);
        assert_eq!(parse_vsync("yes"), None);
    }

    #[test]
    fn widescreen_setting_parsing() {
        // "0"/empty = explicitly off (an env "0" overrides a config value);
        // invalid = None (falls through to the config).
        assert_eq!(parse_widescreen("0"), Some(None));
        assert_eq!(parse_widescreen(" "), Some(None));
        assert_eq!(parse_widescreen("1"), Some(Some(16.0 / 9.0)));
        assert_eq!(parse_widescreen("wide"), None);
        assert_eq!(parse_widescreen("4:3"), None);
        assert_eq!(parse_filter(" nearest"), Some(wgpu::FilterMode::Nearest));
        assert_eq!(parse_filter("Linear"), Some(wgpu::FilterMode::Linear));
        assert_eq!(parse_filter("smooth"), None);
        assert_eq!(parse_tex_filter(" N64"), Some(pw64_gfx::TexFilter::N64));
        assert_eq!(
            parse_tex_filter("bilinear"),
            Some(pw64_gfx::TexFilter::Bilinear)
        );
        assert_eq!(parse_tex_filter("3point"), None);
    }

    #[test]
    fn widescreen_aspect_parsing() {
        assert_eq!(parse_aspect("1"), Some(16.0 / 9.0));
        assert_eq!(parse_aspect(" 21:9 "), Some(21.0 / 9.0));
        assert_eq!(parse_aspect("2.0"), Some(2.0));
        assert_eq!(parse_aspect("0"), None);
        assert_eq!(parse_aspect("4:3"), None);
        assert_eq!(parse_aspect("16:0"), None);
        assert_eq!(parse_aspect("wide"), None);
    }

    /// The monitor-aspect default (D3): wider than 4:3 → the monitor's
    /// aspect (clamped to 21:9); 4:3 / 5:4 / portrait monitors stay 4:3.
    #[test]
    fn widescreen_monitor_default_aspect_rule() {
        assert_eq!(monitor_widescreen_default(1920, 1440), None, "4:3 monitor");
        assert_eq!(monitor_widescreen_default(1280, 1024), None, "5:4 monitor");
        assert_eq!(
            monitor_widescreen_default(1080, 1920),
            None,
            "portrait monitor"
        );
        let a = monitor_widescreen_default(1920, 1080).unwrap();
        assert!((a - 16.0 / 9.0).abs() < 1e-6, "16:9 monitor: own aspect");
        // Wider than 21:9 is clamped (3440x1440 is 2.39).
        let a = monitor_widescreen_default(3440, 1440).unwrap();
        assert!((a - 21.0 / 9.0).abs() < 1e-6, "ultrawide clamped to 21:9");
    }

    #[test]
    fn display_mode_parsing() {
        use super::{DisplayMode, parse_display_mode};
        assert_eq!(parse_display_mode("windowed"), Some(DisplayMode::Windowed));
        assert_eq!(
            parse_display_mode(" Borderless "),
            Some(DisplayMode::Borderless)
        );
        assert_eq!(
            parse_display_mode("EXCLUSIVE"),
            Some(DisplayMode::Exclusive)
        );
        assert_eq!(parse_display_mode("full"), None);
        assert_eq!(parse_display_mode(""), None);
        // The config spelling round-trips in the row's cycle order.
        let all = [
            DisplayMode::Windowed,
            DisplayMode::Borderless,
            DisplayMode::Exclusive,
        ];
        for m in all {
            assert_eq!(parse_display_mode(m.as_str()), Some(m));
            assert_eq!(parse_display_mode(m.as_str()).unwrap().as_str(), m.as_str());
        }
        assert!(all[0] < all[1] && all[1] < all[2]);
    }

    #[test]
    fn resolution_parsing() {
        use super::{Res, parse_resolution};
        assert_eq!(
            parse_resolution("1920x1080"),
            Some(Res {
                w: 1920,
                h: 1080,
                hz: None
            })
        );
        assert_eq!(
            parse_resolution(" 1280x720@60 "),
            Some(Res {
                w: 1280,
                h: 720,
                hz: Some(60)
            })
        );
        assert_eq!(
            parse_resolution("640X480"),
            Some(Res {
                w: 640,
                h: 480,
                hz: None
            })
        );
        // Junk and zeros are rejected.
        assert_eq!(parse_resolution("wide"), None);
        assert_eq!(parse_resolution("0x480"), None);
        assert_eq!(parse_resolution("640x0"), None);
        assert_eq!(parse_resolution("640x480@0"), None);
        assert_eq!(parse_resolution("640x480@1001"), None);
        assert_eq!(parse_resolution("640"), None);
        assert_eq!(parse_resolution("x480"), None);
        // The config spelling round-trips.
        for r in [
            Res {
                w: 1920,
                h: 1080,
                hz: None,
            },
            Res {
                w: 640,
                h: 480,
                hz: Some(59),
            },
        ] {
            assert_eq!(parse_resolution(&r.to_config()), Some(r));
        }
        // The rate cap matches the fps cap.
        assert_eq!(parse_resolution("640x480@1000").unwrap().hz, Some(1000));
    }

    #[test]
    fn fill_screen_env_parsing_and_default() {
        // PW64_FILL_SCREEN uses the same 0/1 spelling as PW64_VSYNC.
        assert_eq!(parse_vsync("1"), Some(true));
        assert_eq!(parse_vsync("0"), Some(false));
        // Default follows widescreen (renderer.md "Fill screen").
        assert!(fill_default(Some(16.0 / 9.0)));
        assert!(!fill_default(None));
        // Precedence: an explicit value beats the default either way.
        assert!(!precedence(Some(false), None, true));
        assert!(precedence(Some(true), Some(false), true));
        assert!(precedence(None, Some(true), false));
        assert!(!precedence(None, None, false));
    }

    /// Low-end preset detection (L1): synthetic `AdapterInfo`s, no adapter
    /// needed (the name/type rule is what runs on real hardware).
    #[test]
    fn low_end_adapter_detection() {
        use super::low_end_adapter;
        use wgpu::{AdapterInfo, Backend, DeviceType};
        let info = |name: &str, ty: DeviceType| AdapterInfo {
            name: name.into(),
            vendor: 0,
            device: 0,
            device_type: ty,
            driver: String::new(),
            driver_info: String::new(),
            backend: Backend::Vulkan,
        };
        // The direct report and every software rasteriser's name.
        assert!(low_end_adapter(&info("GDI Generic", DeviceType::Cpu)));
        assert!(low_end_adapter(&info(
            "llvmpipe (LLVM 17.0.6, 256 bits)",
            DeviceType::Other
        )));
        assert!(low_end_adapter(&info("softpipe", DeviceType::Other)));
        assert!(low_end_adapter(&info(
            "SwiftShader Device",
            DeviceType::IntegratedGpu
        )));
        assert!(low_end_adapter(&info(
            "Microsoft Basic Render Driver",
            DeviceType::Other
        )));
        assert!(low_end_adapter(&info(
            "microsoft basic render driver",
            DeviceType::VirtualGpu
        )));
        // Integrated GPUs and real cards get no preset.
        assert!(!low_end_adapter(&info(
            "Intel(R) UHD Graphics 620",
            DeviceType::IntegratedGpu
        )));
        assert!(!low_end_adapter(&info(
            "NVIDIA GeForce RTX 3080",
            DeviceType::DiscreteGpu
        )));
        assert!(!low_end_adapter(&info(
            "AMD Radeon 780M",
            DeviceType::Other
        )));
    }
}
