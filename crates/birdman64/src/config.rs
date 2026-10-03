//! `pw64.toml`: optional config file in the data dir (`paths::data_dir`:
//! the cwd for developer builds, the exe dir for a portable install, the
//! per-user directory otherwise; see `paths.rs`).
//!
//! Precedence per option: the `PW64_*` env var wins over the config file
//! (see `opts::precedence`), which wins over the built-in default. A missing
//! file is fine; a file that is not valid TOML at all falls back to the
//! defaults with a warning, and one bad value only loses that key (the
//! skipped keys are collected for a toast via `ignored_keys`) — never crash
//! the game over a config. Parsing happens once at startup
//! (first `get()`); afterwards the parsed values are read from the `OnceLock`
//! without allocating.
//!
//! ```toml
//! [graphics]
//! msaa = 4                # 1, 4 or 8 (`PW64_MSAA`)
//! scale = 2.0              # render scale ≥ 0.5 (`PW64_SCALE`)
//! scale_filter = "nearest" # or "linear" (`PW64_SCALE_FILTER`)
//! filter = "n64"           # texture filter: "bilinear" or "n64" 3-point (`PW64_FILTER`)
//! widescreen = "16:9"      # 1, "w:h", a ratio like "2.33", or "0" off (`PW64_WIDESCREEN`)
//! fps = "monitor"          # "monitor", 30..1000, or 0 = uncapped (`PW64_FPS`)
//!
//! vsync = false            # V-Sync: display-paced ticks (`PW64_VSYNC`)
//! fill_screen = true       # no letterbox bars (`PW64_FILL_SCREEN`; default: follows widescreen)
//! display_mode = "windowed" # "windowed", "borderless" or "exclusive" (`PW64_DISPLAY_MODE`)
//! fullscreen_resolution = "1920x1080@60" # exclusive video mode WxH[@Hz] (`PW64_FULLSCREEN_RES`)
//! no_audio = true          # no output device (`PW64_NO_AUDIO`)
//! volume = 0.8             # output volume 0..1 (settings screen)
//! show_fps = false         # frame rate in the window title (settings screen)
//!
//! [input]                  # settings-screen triggers (default: F10 / Select)
//! settings_key = "F10"
//! settings_pad_button = "Select"
//!
//! [input.keyboard]         # N64 slot → key; unlisted slots keep their defaults
//! A = "Space"
//! B = "LShift"
//! START = "Enter"
//!
//! [input.gamepad]          # N64 slot → gilrs button
//! Z = "LeftTrigger2"
//!
//! [input.ble]              # Switch 2 pads over Bluetooth LE (`PW64_BLE`)
//! enabled = true
//!
//! [oled]                   # OLED care (settings screen): HUD drift + dim
//! drift = false            # `PW64_OLED`
//! brightness = 1.0         # 1.0, 0.85, 0.8, 0.75 or 0.7
//!
//! [rom]                    # remembered ROM pick (first-run dialog)
//! path = "C:/roms/pw64.z64"
//!
//! [ui]                     # remembered UI state
//! settings_hint_shown = true
//! pause_in_background = true # pause when the window loses focus or the last pad disconnects
//! ```

use crate::opts;
use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::sync::OnceLock;

/// `pw64.toml`, in the data dir (`paths::data_dir`; developer builds keep
/// it in the cwd; see `paths.rs` for the rule order).
fn path() -> std::path::PathBuf {
    crate::paths::data_dir().join("pw64.toml")
}

/// Validated settings; the maps are passed to `input` which knows how to
/// parse key / button names against winit / gilrs.
#[derive(Debug, Default)]
pub struct Config {
    pub graphics: Graphics,
    pub input: InputMaps,
    /// `[oled]`: OLED care for the flight HUD (drift + brightness).
    pub oled: Oled,
    pub rom: Rom,
    pub ui: Ui,
    /// Keys skipped at load (wrong type or invalid value; see `ignored_keys`).
    pub ignored: Vec<String>,
}

/// `[oled]`: OLED care (O3). The window applies it every frame; the
/// settings screen stages new values live.
#[derive(Debug)]
pub struct Oled {
    /// The HUD slowly drifts in a circle, spreading the wear.
    pub drift: bool,
    /// HUD brightness multiplier, 1 = off.
    pub brightness: f32,
}

impl Default for Oled {
    fn default() -> Self {
        Self {
            drift: false,
            // 1 = no dimming (a derived Default would give 0: invisible HUD).
            brightness: 1.0,
        }
    }
}

/// The valid HUD brightness steps (O3): the settings screen cycles these.
pub const OLED_BRIGHTNESS_STEPS: [f32; 5] = [1.0, 0.85, 0.8, 0.75, 0.7];

/// `[ui]`: window UI state the game remembers between runs.
#[derive(Debug)]
pub struct Ui {
    /// The first-launch settings hint ("F10 / Select: Settings") has been
    /// shown once; it stays hidden afterwards.
    pub settings_hint_shown: bool,
    /// U26: open the settings overlay (pausing the game) when the window
    /// loses focus or the last controller disconnects.
    pub pause_in_background: bool,
}

impl Default for Ui {
    fn default() -> Self {
        Self {
            settings_hint_shown: false,
            // On: switching to another window pausing the game is what a
            // player expects; the settings screen can turn it off.
            pause_in_background: true,
        }
    }
}

/// `[rom]`: remembered first-run ROM pick (`rom_setup`).
#[derive(Debug, Default)]
pub struct Rom {
    /// Absolute path of the ROM file, or none until a picker ran.
    pub path: Option<String>,
}

#[derive(Debug)]
pub struct Graphics {
    /// None = keep the built-in default (MSAA 1).
    pub msaa: Option<u32>,
    pub scale: Option<f32>,
    pub scale_filter: Option<String>,
    /// Texture filter, "bilinear" / "n64" (`opts::parse_tex_filter`).
    pub filter: Option<String>,
    pub widescreen: Option<String>,
    /// Validated `fps` value as text (`opts::parse_fps`).
    pub fps: Option<String>,
    /// Display-paced present ticks (V-Sync); default off.
    pub vsync: bool,
    /// No letterbox bars around world views (`PW64_FILL_SCREEN`); None =
    /// default follows `widescreen` (`opts::fill_default`).
    pub fill_screen: Option<bool>,
    /// How the window goes fullscreen, "windowed" / "borderless" /
    /// "exclusive" (`opts::parse_display_mode`).
    pub display_mode: Option<String>,
    /// The exclusive video mode, "WxH[@Hz]" (`opts::parse_resolution`).
    pub fullscreen_resolution: Option<String>,
    /// U25: the window title shows the frame rate; default off.
    pub show_fps: bool,
    pub no_audio: bool,
    /// Output volume 0..1; default 1.0 (a derived Default would give 0).
    pub volume: f32,
}

impl Default for Graphics {
    fn default() -> Self {
        Self {
            msaa: None,
            scale: None,
            scale_filter: None,
            filter: None,
            widescreen: None,
            fps: None,
            vsync: false,
            fill_screen: None,
            display_mode: None,
            fullscreen_resolution: None,
            show_fps: false,
            no_audio: false,
            volume: 1.0,
        }
    }
}

#[derive(Debug, Default)]
pub struct InputMaps {
    /// Settings-screen triggers; `None` = defaults (F10 / Select).
    pub settings: Settings,
    pub keyboard: HashMap<String, String>,
    pub gamepad: HashMap<String, String>,
    /// `[input.ble] enabled` — Switch 2 pads over BLE; `ble::enabled()` resolves
    /// it against the `PW64_BLE` env var and the default (off).
    pub ble: Option<bool>,
}

#[derive(Debug, Default)]
pub struct Settings {
    /// Keyboard key that opens the settings screen, e.g. `"F10"`.
    pub key: Option<String>,
    /// gilrs button that opens it, e.g. `"Select"`.
    pub pad_button: Option<String>,
}

/// Parses one config file's text; the loader wraps this with the data-dir
/// file and error fallbacks. Separated out so tests can check malformed input
/// without touching globals. Only text that is not valid TOML at all is an
/// error: one bad value drops just that key (S1 review).
fn parse(text: &str) -> Result<Config, toml::de::Error> {
    let table: toml::Table = toml::from_str(text)?;
    Ok(from_table(&table))
}

/// Loads `pw64.toml` from the data dir. Absent file → defaults,
/// silently (no config is the normal case); a malformed file warns and
/// defaults rather than taking the game down.
fn load() -> Config {
    match std::fs::read_to_string(path()) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
        Err(e) => {
            eprintln!("[config] can't read pw64.toml ({e}); using defaults");
            Config::default()
        }
        Ok(text) => match parse(&text) {
            Ok(cfg) => cfg,
            Err(e) => {
                eprintln!("[config] pw64.toml: {e}; using defaults");
                Config::default()
            }
        },
    }
}

/// Warns and records one skipped key (S1): the value's type or content made
/// it unusable. `ignored_keys` surfaces the names so the window can toast
/// them instead of failing silently.
fn ignore(ignored: &mut Vec<String>, key: &str, value: impl std::fmt::Display, hint: &str) {
    eprintln!("[config] pw64.toml: {key} = {value} ({hint}); ignored");
    ignored.push(key.to_string());
}

/// Booleans, leniently: `true`/`false` or the integer 0/1 a hand-edited
/// file is likely to contain (`vsync = 1` must not void the file).
fn coerce_bool(v: &toml::Value) -> Option<bool> {
    match v {
        toml::Value::Boolean(b) => Some(*b),
        toml::Value::Integer(n) if *n == 0 => Some(false),
        toml::Value::Integer(n) if *n == 1 => Some(true),
        toml::Value::String(s) => match s.trim() {
            "true" | "1" => Some(true),
            "false" | "0" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// Numbers, leniently: an integer or a numeric string for a float field
/// (`msaa = "4"`, `scale = "2"`).
fn coerce_f32(v: &toml::Value) -> Option<f32> {
    match v {
        toml::Value::Float(f) => Some(*f as f32),
        toml::Value::Integer(n) => Some(*n as f32),
        toml::Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// `msaa`, leniently: an integer, a numeric string, or an integral float.
fn coerce_u32(v: &toml::Value) -> Option<u32> {
    match v {
        toml::Value::Integer(n) => u32::try_from(*n).ok(),
        toml::Value::Float(f) if f.fract() == 0.0 && (0.0..=u32::MAX as f64).contains(f) => {
            Some(*f as u32)
        }
        v => v.as_str().and_then(|s| s.trim().parse().ok()),
    }
}

/// `widescreen`/`fps` accept a bare number (`widescreen = 1`) as well as a
/// string: reduced to the text the `opts` parser validates.
fn as_text(v: &toml::Value) -> Option<String> {
    match v {
        toml::Value::String(s) => Some(s.clone()),
        toml::Value::Integer(n) => Some(n.to_string()),
        toml::Value::Float(f) => Some(f.to_string()),
        _ => None,
    }
}

/// One text key (exactly a TOML string); a wrong type is ignored. Same
/// shape for every caller, so the skip path stays uniform.
fn string_of(t: &toml::Table, key: &str, path: &str, ignored: &mut Vec<String>) -> Option<String> {
    match t.get(key) {
        Some(v) => match v.as_str() {
            Some(s) => Some(s.to_string()),
            None => {
                ignore(ignored, path, v, "must be text");
                None
            }
        },
        None => None,
    }
}

/// One bool key, leniently; a wrong type is ignored. `d` is the value when
/// the key is absent (and what an invalid value falls back to).
fn bool_of(t: &toml::Table, key: &str, path: &str, ignored: &mut Vec<String>, d: bool) -> bool {
    match t.get(key) {
        Some(v) => coerce_bool(v).unwrap_or_else(|| {
            ignore(ignored, path, v, "use true or false");
            d
        }),
        None => d,
    }
}

/// One bool key that distinguishes absent from invalid.
fn opt_bool_of(t: &toml::Table, key: &str, path: &str, ignored: &mut Vec<String>) -> Option<bool> {
    match t.get(key) {
        Some(v) => match coerce_bool(v) {
            Some(b) => Some(b),
            None => {
                ignore(ignored, path, v, "use true or false");
                None
            }
        },
        None => None,
    }
}

/// One validated text key: `valid` is the opts parser the value must pass.
fn checked_string(
    t: &toml::Table,
    key: &str,
    path: &str,
    ignored: &mut Vec<String>,
    hint: &str,
    valid: fn(&str) -> bool,
) -> Option<String> {
    match string_of(t, key, path, ignored) {
        Some(s) if valid(&s) => Some(s),
        Some(s) => {
            ignore(ignored, path, s, hint);
            None
        }
        None => None,
    }
}

/// Reads one parsed `pw64.toml` table into validated settings, one key at a
/// time (S1 review): a value with the wrong type or an invalid value is
/// warned, skipped and collected in the result's `ignored`, instead of
/// serde's typed struct throwing away the whole file (one `vsync = 1` used
/// to lose every setting, the bindings and the remembered ROM). Unknown
/// keys survive saves and are ignored here.
fn from_table(table: &toml::Table) -> Config {
    let mut cfg = Config::default();
    let mut ignored: Vec<String> = Vec::new();

    if let Some(v) = table.get("graphics") {
        match v.as_table() {
            Some(g) => {
                cfg.graphics.msaa = match g.get("msaa") {
                    Some(v) => match coerce_u32(v) {
                        Some(n @ (1 | 4 | 8)) => Some(n),
                        _ => {
                            ignore(&mut ignored, "graphics.msaa", v, "use 1, 4 or 8");
                            None
                        }
                    },
                    None => None,
                };
                cfg.graphics.scale = match g.get("scale") {
                    Some(v) => match coerce_f32(v) {
                        Some(f) if f.is_finite() && f >= 0.5 => Some(f),
                        _ => {
                            ignore(
                                &mut ignored,
                                "graphics.scale",
                                v,
                                "use a finite value >= 0.5",
                            );
                            None
                        }
                    },
                    None => None,
                };
                cfg.graphics.scale_filter = checked_string(
                    g,
                    "scale_filter",
                    "graphics.scale_filter",
                    &mut ignored,
                    "use linear or nearest",
                    |s| opts::parse_filter(s).is_some(),
                );
                cfg.graphics.filter = checked_string(
                    g,
                    "filter",
                    "graphics.filter",
                    &mut ignored,
                    "use bilinear or n64",
                    |s| opts::parse_tex_filter(s).is_some(),
                );
                cfg.graphics.widescreen = match g.get("widescreen") {
                    Some(v) => match as_text(v).filter(|s| opts::parse_widescreen(s).is_some()) {
                        Some(s) => Some(s),
                        _ => {
                            ignore(
                                &mut ignored,
                                "graphics.widescreen",
                                v,
                                "use 0, 1, \"16:9\", …",
                            );
                            None
                        }
                    },
                    None => None,
                };
                cfg.graphics.fps = match g.get("fps") {
                    Some(v) => match as_text(v).filter(|s| opts::parse_fps(s).is_some()) {
                        Some(s) => Some(s),
                        _ => {
                            ignore(
                                &mut ignored,
                                "graphics.fps",
                                v,
                                "use \"monitor\", 30..1000 or 0 = uncapped",
                            );
                            None
                        }
                    },
                    None => None,
                };
                cfg.graphics.vsync = bool_of(g, "vsync", "graphics.vsync", &mut ignored, false);
                cfg.graphics.fill_screen =
                    opt_bool_of(g, "fill_screen", "graphics.fill_screen", &mut ignored);
                cfg.graphics.display_mode = checked_string(
                    g,
                    "display_mode",
                    "graphics.display_mode",
                    &mut ignored,
                    "use windowed, borderless or exclusive",
                    |s| opts::parse_display_mode(s).is_some(),
                );
                cfg.graphics.fullscreen_resolution = checked_string(
                    g,
                    "fullscreen_resolution",
                    "graphics.fullscreen_resolution",
                    &mut ignored,
                    "use WxH or WxH@Hz, no zeros",
                    |s| opts::parse_resolution(s).is_some(),
                );
                cfg.graphics.no_audio =
                    bool_of(g, "no_audio", "graphics.no_audio", &mut ignored, false);
                cfg.graphics.show_fps =
                    bool_of(g, "show_fps", "graphics.show_fps", &mut ignored, false);
                cfg.graphics.volume = match g.get("volume") {
                    Some(v) => match coerce_f32(v) {
                        Some(f) if (0.0..=1.0).contains(&f) => f,
                        _ => {
                            ignore(&mut ignored, "graphics.volume", v, "use 0.0..1.0");
                            1.0
                        }
                    },
                    None => 1.0,
                };
            }
            None => ignore(&mut ignored, "graphics", v, "must be a [graphics] table"),
        }
    }

    if let Some(v) = table.get("input") {
        match v.as_table() {
            Some(i) => {
                cfg.input.settings.key =
                    string_of(i, "settings_key", "input.settings_key", &mut ignored);
                cfg.input.settings.pad_button = string_of(
                    i,
                    "settings_pad_button",
                    "input.settings_pad_button",
                    &mut ignored,
                );
                for (section, target) in
                    [("keyboard", "input.keyboard"), ("gamepad", "input.gamepad")]
                {
                    if let Some(m) = i.get(section) {
                        match m.as_table() {
                            Some(bindings) => {
                                // One slot name → one key/button name; an
                                // entry with a non-string value is skipped
                                // on its own, the rest survive.
                                let mut slots = HashMap::new();
                                for (slot, val) in bindings {
                                    match val.as_str() {
                                        Some(s) => {
                                            slots.insert(slot.clone(), s.to_string());
                                        }
                                        None => ignore(
                                            &mut ignored,
                                            &format!("{target}.{slot}"),
                                            val,
                                            "must be a name in quotes",
                                        ),
                                    }
                                }
                                if section == "keyboard" {
                                    cfg.input.keyboard = slots;
                                } else {
                                    cfg.input.gamepad = slots;
                                }
                            }
                            None => {
                                ignore(&mut ignored, target, m, "must be a table of slot = name");
                            }
                        }
                    }
                }
                if let Some(b) = i.get("ble") {
                    match b.as_table() {
                        Some(b) => {
                            cfg.input.ble =
                                opt_bool_of(b, "enabled", "input.ble.enabled", &mut ignored);
                        }
                        None => {
                            ignore(&mut ignored, "input.ble", b, "must be an [input.ble] table")
                        }
                    }
                }
            }
            None => ignore(&mut ignored, "input", v, "must be an [input] table"),
        }
    }

    if let Some(v) = table.get("oled") {
        match v.as_table() {
            Some(o) => {
                cfg.oled.drift = bool_of(o, "drift", "oled.drift", &mut ignored, false);
                cfg.oled.brightness = match o.get("brightness") {
                    Some(v) => match coerce_f32(v) {
                        Some(f) if OLED_BRIGHTNESS_STEPS.contains(&f) => f,
                        _ => {
                            ignore(
                                &mut ignored,
                                "oled.brightness",
                                v,
                                "use 1.0, 0.85, 0.8, 0.75 or 0.7",
                            );
                            1.0
                        }
                    },
                    None => 1.0,
                };
            }
            None => ignore(&mut ignored, "oled", v, "must be an [oled] table"),
        }
    }

    if let Some(v) = table.get("rom") {
        match v.as_table() {
            Some(r) => cfg.rom.path = string_of(r, "path", "rom.path", &mut ignored),
            None => ignore(&mut ignored, "rom", v, "must be a [rom] table"),
        }
    }

    if let Some(v) = table.get("ui") {
        match v.as_table() {
            Some(u) => {
                cfg.ui.settings_hint_shown = bool_of(
                    u,
                    "settings_hint_shown",
                    "ui.settings_hint_shown",
                    &mut ignored,
                    false,
                );
                cfg.ui.pause_in_background = bool_of(
                    u,
                    "pause_in_background",
                    "ui.pause_in_background",
                    &mut ignored,
                    true,
                );
            }
            None => ignore(&mut ignored, "ui", v, "must be a [ui] table"),
        }
    }

    cfg.ignored = ignored;
    cfg
}

/// Parsed `pw64.toml`. The process-wide settings, loaded on first use.
pub fn get() -> &'static Config {
    static CONFIG: OnceLock<Config> = OnceLock::new();
    CONFIG.get_or_init(load)
}

/// Keys from `pw64.toml` that were skipped at load because a value had the
/// wrong type or was invalid (S1 review), e.g. `["graphics.msaa"]`. For the
/// window to toast ("some settings in pw64.toml were ignored") instead of
/// failing silently; empty until a file with problems was read.
pub fn ignored_keys() -> Vec<String> {
    get().ignored.clone()
}

/// Settings the screen can write back to `pw64.toml` (auto-saved on close,
/// D2). `PartialEq` snapshots what the screen showed at open.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct SavedSettings {
    pub msaa: Option<u32>,
    pub scale: Option<f32>,
    /// None = leave as-is (the key is not touched).
    pub scale_filter: Option<&'static str>,
    /// Texture filter `"bilinear"` / `"n64"`; None = leave as-is.
    pub filter: Option<&'static str>,
    /// `"0"` for off, `"16:9"`, …; None = leave as-is; `""` = remove the
    /// key (the default follows the monitor's aspect).
    pub widescreen: Option<String>,
    /// `opts::Fps::to_config` text (`"monitor"`, `"144"`, `"0"`); None =
    /// leave as-is. Numbers are written as integers.
    pub fps: Option<String>,
    /// V-Sync (None = leave as-is).
    pub vsync: Option<bool>,
    /// Fill screen (None = leave as-is, `Some(None)` = remove the key: the
    /// default follows widescreen).
    pub fill_screen: Option<Option<bool>>,
    /// How the window goes fullscreen ("windowed" / "borderless" /
    /// "exclusive"); None = leave as-is.
    pub display_mode: Option<&'static str>,
    /// The exclusive video mode ("WxH[@Hz]"): None = leave as-is,
    /// `Some(None)` = remove the key (back to the monitor's current mode).
    pub fullscreen_resolution: Option<Option<String>>,
    /// Output volume 0..1 (None = leave as-is).
    pub volume: Option<f32>,
    /// OLED care (O3): the HUD drifts (None = leave as-is).
    pub oled_drift: Option<bool>,
    /// OLED care HUD brightness, 1 = off (None = leave as-is).
    pub oled_brightness: Option<f32>,
    /// Rebound keyboard bindings (N64 slot → key name): None = leave as-is,
    /// `Some(map)` replaces the `[input.keyboard]` sub-table, `Some(empty)`
    /// removes it (back to the built-in defaults). Other `[input]` keys
    /// (settings triggers, `[input.ble]`) survive.
    pub keyboard: Option<BTreeMap<String, String>>,
    /// Same for the gamepad map (`[input.gamepad]`, gilrs button names).
    pub gamepad: Option<BTreeMap<String, String>>,
    /// U25: the frame rate in the window title (None = leave as-is).
    pub show_fps: Option<bool>,
    /// U26: open the overlay when the window loses focus / the last pad
    /// disconnects (`[ui] pause_in_background`; None = leave as-is).
    pub pause_in_background: Option<bool>,
    /// Switch 2 pads over BLE (`[input.ble] enabled`; None = leave as-is).
    pub ble: Option<bool>,
}

/// Writes the screen's settings into `pw64.toml`. The file is round-tripped
/// through `toml::Table`, so keys the screen doesn't know survive; comments
/// (if any) don't. Written atomically (temp file + rename). A malformed file
/// is never overwritten — fix it by hand first.
pub fn save_settings(u: SavedSettings) -> Result<(), String> {
    let mut table = read_table()?;
    apply(&mut table, &u, opts::low_end())?;
    write_table(&table)
}

/// Remembers the first-run ROM pick as `rom.path` (absolute; `rom_setup`).
/// Same file discipline as `save_settings`: unknown keys survive, and a
/// malformed or unreadable file is never overwritten.
pub fn save_rom_path(path: &std::path::Path) -> Result<(), String> {
    let table = read_table()?;
    let table = with_rom_path(table, path)?;
    write_table(&table)
}

/// Marks the first-launch settings hint as shown (`[ui]
/// settings_hint_shown = true`). Same file discipline as `save_settings`:
/// unknown keys survive, and a malformed or unreadable file is never
/// overwritten (the hint then shows again next launch, which is fine).
pub fn save_ui_flag() -> Result<(), String> {
    let table = read_table()?;
    let table = with_ui_flag(table)?;
    write_table(&table)
}

/// Reads `pw64.toml` into a round-trip table: only a missing file starts from
/// scratch — any other read error (locked, not UTF-8, …) or malformed TOML
/// must not turn into "overwrite with defaults".
fn read_table() -> Result<toml::Table, String> {
    let text = match std::fs::read_to_string(path()) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("can't read pw64.toml ({e}); not overwriting it")),
    };
    toml::from_str(&text).map_err(|e| format!("pw64.toml is malformed ({e}); not overwriting it"))
}

/// Atomically writes a round-trip table back to `pw64.toml` (temp file +
/// rename; same directory, so the rename is atomic on Windows and Unix).
fn write_table(table: &toml::Table) -> Result<(), String> {
    let out = toml::to_string_pretty(table).map_err(|e| e.to_string())?;
    let path = path();
    let mut tmp = path.clone().into_os_string();
    tmp.push(".new");
    let write = || -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(out.as_bytes())?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, &path)
    };
    write().map_err(|e| format!("writing pw64.toml: {e}"))?;
    eprintln!("[config] saved {}", path.display());
    Ok(())
}

/// Inserts `rom.path` into a round-trip table (separate from the file I/O for
/// tests). A non-table `[rom]` is an error, not a silent "saved".
fn with_rom_path(mut table: toml::Table, path: &std::path::Path) -> Result<toml::Table, String> {
    let rom = table
        .entry("rom")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    let toml::Value::Table(rom) = rom else {
        return Err("[rom] is not a table in pw64.toml; not saved".to_string());
    };
    rom.insert("path".into(), path.display().to_string().into());
    Ok(table)
}

/// Sets `ui.settings_hint_shown` in a round-trip table (separate from the
/// file I/O for tests). A non-table `[ui]` is an error, not a silent "saved".
fn with_ui_flag(mut table: toml::Table) -> Result<toml::Table, String> {
    let ui = table
        .entry("ui")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    let toml::Value::Table(ui) = ui else {
        return Err("[ui] is not a table in pw64.toml; not saved".to_string());
    };
    ui.insert("settings_hint_shown".into(), true.into());
    Ok(table)
}

/// The built-in default of one settings key, in the exact `toml::Value`
/// shape [`apply`] writes; `None` where the key has no fixed default (its
/// absent behaviour is handled by the `Some(None)` / `""` update sentinels,
/// or follows another key). The input binding maps compare as maps instead
/// ([`builtin_input_map`]). Every value comes from the same constant or
/// function the option is *read* with, so a saved default and the built-in
/// default cannot drift apart.
///
/// Pure and free of process state on purpose (U18): the low-end preset is
/// taken in as a flag, not read from `opts`, so tests can pin both
/// variants. When it is active its gentler values are the *effective*
/// defaults (they are what the game actually runs), so a Reset on that
/// machine removes the keys and the preset keeps applying until a real
/// graphics driver is present, which restores the plain defaults.
fn builtin_default(section: &str, key: &str, low_end: bool) -> Option<toml::Value> {
    // The same three-decimal rounding `apply` writes with, so the
    // comparison is exact.
    let float = |v: f32| toml::Value::Float((f64::from(v) * 1000.0).round() / 1000.0);
    match (section, key) {
        ("graphics", "msaa") => Some(opts::MSAA_DEFAULT.into()),
        ("graphics", "scale") => Some(float(if low_end {
            opts::SCALE_LOW_END
        } else {
            opts::SCALE_DEFAULT
        })),
        ("graphics", "scale_filter") => Some(opts::SCALE_FILTER_DEFAULT.into()),
        ("graphics", "filter") => Some(opts::TEX_FILTER_DEFAULT.into()),
        // "monitor" is the window default (`opts::fps(true)`; the settings
        // screen only saves in the window); with the low-end preset it is
        // 60. A monitor swap or a new graphics driver must keep applying
        // to a file without the key. Written as a number or text exactly
        // as `apply` writes it.
        ("graphics", "fps") => {
            let s = if low_end {
                opts::Fps::Hz(opts::FPS_LOW_END_HZ).to_config()
            } else {
                opts::Fps::Monitor.to_config()
            };
            Some(match s.parse::<i64>() {
                Ok(n) => n.into(),
                Err(_) => s.into(),
            })
        }
        ("graphics", "vsync") => Some(opts::VSYNC_DEFAULT.into()),
        ("graphics", "display_mode") => Some(opts::DISPLAY_MODE_DEFAULT.into()),
        ("graphics", "volume") => Some(float(Graphics::default().volume)),
        // U25: the title shows the frame rate only when the player asked.
        ("graphics", "show_fps") => Some(Graphics::default().show_fps.into()),
        // Dynamic defaults: the default is "follow something" (the
        // monitor's aspect for widescreen, widescreen for fill_screen, the
        // monitor's current video mode), not a value, so they are removed
        // by the `""` / `Some(None)` sentinels in `apply`, never compared.
        ("graphics", "widescreen" | "fill_screen" | "fullscreen_resolution") => None,
        ("oled", "drift") => Some(Oled::default().drift.into()),
        ("oled", "brightness") => Some(float(Oled::default().brightness)),
        // U26: background pausing is on until the player turns it off.
        ("ui", "pause_in_background") => Some(Ui::default().pause_in_background.into()),
        // Switch 2 pads over BLE: off unless the player switches them on.
        ("input.ble", "enabled") => Some(crate::ble::DEFAULT.into()),
        _ => None,
    }
}

/// The full built-in binding map for one `[input.*]` section (U18), in the
/// spelling the settings screen saves (`input::bind_key` / `bind_button`
/// write `format!("{k:?}")`). A saved map equal to it means nothing is
/// rebound, so the sub-table is removed like an empty map. Multi-key slots
/// (Z, B on a pad, the stick) bind their first default here; an exactly
/// equal map is "every slot back to its primary default", which is what
/// resetting the Controls page means, and removing the table restores the
/// extra defaults too.
fn builtin_input_map(section: &str) -> BTreeMap<String, String> {
    match section {
        "keyboard" => crate::input::KEYBOARD_SLOTS
            .iter()
            .map(|(_, slot, keys)| (slot.to_string(), format!("{:?}", keys[0])))
            .collect(),
        "gamepad" => crate::input::GAMEPAD_SLOTS
            .iter()
            .map(|(_, slot, buttons)| (slot.to_string(), format!("{:?}", buttons[0])))
            .collect(),
        _ => BTreeMap::new(),
    }
}

/// Applies one update in place (separate from the file I/O for tests;
/// `pub(crate)` so the settings screen's tests can drive it directly).
/// `low_end` is the live low-end preset flag (`opts::low_end()`, passed by
/// [`save_settings`]): with it active the preset's fps/scale defaults are
/// the effective defaults and are removed too.
pub(crate) fn apply(
    table: &mut toml::Table,
    u: &SavedSettings,
    low_end: bool,
) -> Result<(), String> {
    let graphics = table
        .entry("graphics")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    let toml::Value::Table(graphics) = graphics else {
        return Err("[graphics] is not a table in pw64.toml; not saved".to_string());
    };
    // f32 → f64 would write 0.3499999940395355 for 0.35; the UI steps are
    // coarse, so three decimals keep the file readable.
    let float = |v: f32| toml::Value::Float((f64::from(v) * 1000.0).round() / 1000.0);
    {
        // U18: a value equal to the built-in default is removed instead of
        // written, so a later default change (the low-end preset, the
        // monitor's rate, a code change) keeps applying; keys the player
        // actually chose stay in the file.
        let mut put = |k: &str, v: toml::Value| {
            if builtin_default("graphics", k, low_end).as_ref() == Some(&v) {
                graphics.remove(k);
            } else {
                graphics.insert(k.into(), v);
            }
        };
        if let Some(v) = u.msaa {
            put("msaa", v.into());
        }
        if let Some(v) = u.scale {
            put("scale", float(v));
        }
        if let Some(v) = u.scale_filter {
            put("scale_filter", v.into());
        }
        if let Some(v) = u.filter {
            put("filter", v.into());
        }
        if let Some(v) = u.widescreen.as_ref().filter(|v| !v.is_empty()) {
            put("widescreen", v.clone().into());
        }
        if let Some(v) = &u.fps {
            match v.parse::<i64>() {
                Ok(n) => put("fps", n.into()),
                Err(_) => put("fps", v.clone().into()),
            }
        }
        if let Some(v) = u.volume {
            put("volume", float(v));
        }
        if let Some(v) = u.vsync {
            put("vsync", v.into());
        }
        if let Some(v) = u.display_mode {
            put("display_mode", v.into());
        }
        if let Some(Some(v)) = u.fill_screen {
            put("fill_screen", v.into());
        }
        if let Some(Some(v)) = &u.fullscreen_resolution {
            put("fullscreen_resolution", v.clone().into());
        }
        if let Some(v) = u.show_fps {
            put("show_fps", v.into());
        }
    }
    // Back to the defaults: drop keys saved earlier (the fill_screen
    // default follows widescreen; the fullscreen_resolution default is the
    // monitor's current mode).
    if u.fill_screen == Some(None) {
        graphics.remove("fill_screen");
    }
    // `Some("")`: the widescreen default (the monitor's aspect, D3/S8).
    if u.widescreen.as_deref() == Some("") {
        graphics.remove("widescreen");
    }
    if u.fullscreen_resolution == Some(None) {
        graphics.remove("fullscreen_resolution");
    }
    // U26: the pause-in-the-background setting lives in `[ui]`.
    if let Some(v) = u.pause_in_background {
        let ui = table
            .entry("ui")
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        let toml::Value::Table(ui) = ui else {
            return Err("[ui] is not a table in pw64.toml; not saved".to_string());
        };
        // Same rule as [graphics] (U18): the default is removed, not
        // written.
        if builtin_default("ui", "pause_in_background", low_end) == Some(v.into()) {
            ui.remove("pause_in_background");
        } else {
            ui.insert("pause_in_background".into(), v.into());
        }
    }
    // OLED care (O3): its own table.
    if u.oled_drift.is_some() || u.oled_brightness.is_some() {
        let oled = table
            .entry("oled")
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        let toml::Value::Table(oled) = oled else {
            return Err("[oled] is not a table in pw64.toml; not saved".to_string());
        };
        // Same rule as [graphics] (U18): the default is removed, not
        // written.
        let mut oput = |k: &str, v: toml::Value| {
            if builtin_default("oled", k, low_end).as_ref() == Some(&v) {
                oled.remove(k);
            } else {
                oled.insert(k.into(), v);
            }
        };
        if let Some(v) = u.oled_drift {
            oput("drift", v.into());
        }
        if let Some(v) = u.oled_brightness {
            oput("brightness", float(v));
        }
    }
    // Input maps (R1): a Save that includes bindings replaces the
    // sub-tables wholesale; an empty map means "back to the defaults" and
    // removes the sub-table. A map equal to the full built-in default
    // means the same (nothing is rebound; U18), so it is removed too, and
    // a later default change keeps applying. Everything else under
    // `[input]` (the settings triggers) and `[input.ble]` survives
    // untouched.
    if u.keyboard.is_some() || u.gamepad.is_some() {
        let input = table
            .entry("input")
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        let toml::Value::Table(input) = input else {
            return Err("[input] is not a table in pw64.toml; not saved".to_string());
        };
        for (key, map) in [("keyboard", &u.keyboard), ("gamepad", &u.gamepad)] {
            match map {
                Some(m) if m.is_empty() || *m == builtin_input_map(key) => {
                    input.remove(key);
                }
                Some(m) => {
                    let t: toml::Table = m
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone().into()))
                        .collect();
                    input.insert(key.into(), t.into());
                }
                None => {}
            }
        }
    }
    // Switch 2 pads over BLE (`[input.ble] enabled`): same U18 rule; the
    // sub-table goes once it is empty, the rest of `[input]` survives. A
    // default on a file without the tables creates nothing.
    if let Some(v) = u.ble {
        let is_default = builtin_default("input.ble", "enabled", low_end) == Some(v.into());
        if !is_default || table.contains_key("input") {
            let input = table
                .entry("input")
                .or_insert_with(|| toml::Value::Table(toml::Table::new()));
            let toml::Value::Table(input) = input else {
                return Err("[input] is not a table in pw64.toml; not saved".to_string());
            };
            if !is_default || input.contains_key("ble") {
                let ble = input
                    .entry("ble")
                    .or_insert_with(|| toml::Value::Table(toml::Table::new()));
                let toml::Value::Table(ble) = ble else {
                    return Err("[input.ble] is not a table in pw64.toml; not saved".to_string());
                };
                if is_default {
                    ble.remove("enabled");
                } else {
                    ble.insert("enabled".into(), v.into());
                }
                if ble.is_empty() {
                    input.remove("ble");
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opts;

    /// [`super::apply`] with the low-end preset off, the common test case
    /// (the preset variant is exercised in `low_end_preset_defaults_removed`).
    fn apply(t: &mut toml::Table, u: &SavedSettings) -> Result<(), String> {
        super::apply(t, u, false)
    }

    #[test]
    fn parses_valid_toml() {
        let c = parse(
            "[graphics]\nmsaa = 4\nscale = 2.0\nscale_filter = \"nearest\"\nfilter = \"N64\"\n\
             no_audio = true\nwidescreen = \"16:9\"\nvsync = true\nfill_screen = true\n\n[input.keyboard]\nA = \"KeyX\"\n\
             \n[input.gamepad]\nZ = \"LeftTrigger2\"\n\n[input.ble]\nenabled = true\n",
        )
        .unwrap();
        assert_eq!(c.graphics.msaa, Some(4));
        assert_eq!(c.graphics.scale, Some(2.0));
        assert_eq!(c.graphics.scale_filter.as_deref(), Some("nearest"));
        assert_eq!(c.graphics.filter.as_deref(), Some("N64"));
        assert_eq!(c.graphics.widescreen.as_deref(), Some("16:9"));
        assert!(c.graphics.no_audio);
        assert!(c.graphics.vsync, "vsync = true parsed");
        assert_eq!(
            c.graphics.fill_screen,
            Some(true),
            "fill_screen = true parsed"
        );
        // Absent → off (the default).
        let d = parse("[graphics]\nmsaa = 4\n").unwrap();
        assert!(!d.graphics.vsync);
        assert_eq!(d.graphics.fill_screen, None);
        assert_eq!(c.input.keyboard.get("A").map(String::as_str), Some("KeyX"));
        assert_eq!(
            c.input.gamepad.get("Z").map(String::as_str),
            Some("LeftTrigger2")
        );
        assert!(c.input.ble.unwrap());
        // OLED care keys absent → defaults.
        assert!(!d.oled.drift);
        assert_eq!(d.oled.brightness, 1.0);
        // OLED care keys parse (O3); an unknown brightness is ignored.
        let o = parse("[oled]\ndrift = true\nbrightness = 0.75\n").unwrap();
        assert!(o.oled.drift);
        assert_eq!(o.oled.brightness, 0.75);
        let bad = parse("[oled]\ndrift = true\nbrightness = 0.9\n").unwrap();
        assert_eq!(bad.oled.brightness, 1.0, "unknown step ignored");
    }

    /// The `oled` keys the settings screen saves: written into their own
    /// table, float-stepped to three decimals, unrelated keys survive.
    #[test]
    fn oled_keys_round_trip() {
        let mut table: toml::Table =
            toml::from_str("[graphics]\nmsaa = 4\n[oled]\ndrift = false\n").unwrap();
        apply(
            &mut table,
            &SavedSettings {
                oled_drift: Some(true),
                oled_brightness: Some(0.8),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(table["oled"]["drift"].as_bool(), Some(true));
        assert_eq!(table["oled"]["brightness"].as_float(), Some(0.8));
        // Unrelated keys survive.
        assert_eq!(table["graphics"]["msaa"].as_integer(), Some(4));
        let text = toml::to_string_pretty(&table).unwrap();
        let c = parse(&text).unwrap();
        assert!(c.oled.drift);
        assert_eq!(c.oled.brightness, 0.8, "the written step parses back");
    }

    /// `[input.ble] enabled` (settings Controls row): On is written and
    /// parses back; Off (the default) removes the key and the then-empty
    /// sub-table, while the rest of `[input]` survives.
    #[test]
    fn ble_enabled_round_trips() {
        let mut table: toml::Table =
            toml::from_str("[input]\nsettings_key = \"F9\"\n\n[input.keyboard]\nA = \"KeyX\"\n")
                .unwrap();
        let on = SavedSettings {
            ble: Some(true),
            ..Default::default()
        };
        apply(&mut table, &on).unwrap();
        assert_eq!(table["input"]["ble"]["enabled"].as_bool(), Some(true));
        let c = parse(&toml::to_string_pretty(&table).unwrap()).unwrap();
        assert_eq!(c.input.ble, Some(true));
        assert_eq!(c.input.keyboard.get("A").map(String::as_str), Some("KeyX"));
        let off = SavedSettings {
            ble: Some(false),
            ..Default::default()
        };
        apply(&mut table, &off).unwrap();
        assert!(table["input"].get("ble").is_none(), "default removed");
        assert_eq!(table["input"]["settings_key"].as_str(), Some("F9"));
        assert!(table["input"].get("keyboard").is_some());
        // Unknown keys in [input.ble] keep the sub-table.
        let mut t: toml::Table =
            toml::from_str("[input.ble]\nenabled = true\nfuture = 1\n").unwrap();
        apply(&mut t, &off).unwrap();
        assert_eq!(t["input"]["ble"]["future"].as_integer(), Some(1));
        assert!(t["input"]["ble"].get("enabled").is_none());
        // Off on a file without [input]: nothing written.
        let mut t = toml::Table::new();
        apply(&mut t, &off).unwrap();
        assert!(t.get("input").is_none());
        // A non-table [input.ble] is an error, not a silent "saved".
        let mut t: toml::Table = toml::from_str("[input]\nble = 3\n").unwrap();
        assert!(apply(&mut t, &on).is_err());
    }

    /// U18: every settings key whose saved value equals the built-in
    /// default is removed from `pw64.toml` instead of pinned, so a later
    /// default change (the low-end preset, the monitor's rate, a code
    /// change) keeps applying; a non-default value is written.
    #[test]
    fn default_values_removed_non_defaults_written() {
        let upd = |f: fn(&mut SavedSettings)| {
            let mut s = SavedSettings::default();
            f(&mut s);
            s
        };
        // (key, the built-in default as an update, a player choice)
        let cases: Vec<(&str, SavedSettings, SavedSettings)> = vec![
            ("msaa", upd(|s| s.msaa = Some(1)), upd(|s| s.msaa = Some(4))),
            (
                "scale",
                upd(|s| s.scale = Some(1.0)),
                upd(|s| s.scale = Some(2.0)),
            ),
            (
                "scale_filter",
                upd(|s| s.scale_filter = Some("linear")),
                upd(|s| s.scale_filter = Some("nearest")),
            ),
            (
                "filter",
                upd(|s| s.filter = Some("bilinear")),
                upd(|s| s.filter = Some("n64")),
            ),
            (
                "fps",
                upd(|s| s.fps = Some("monitor".into())),
                upd(|s| s.fps = Some("60".into())),
            ),
            (
                "vsync",
                upd(|s| s.vsync = Some(false)),
                upd(|s| s.vsync = Some(true)),
            ),
            (
                "display_mode",
                upd(|s| s.display_mode = Some("windowed")),
                upd(|s| s.display_mode = Some("borderless")),
            ),
            (
                "volume",
                upd(|s| s.volume = Some(1.0)),
                upd(|s| s.volume = Some(0.35)),
            ),
            // The dynamic-default sentinels still remove (they never
            // write); a real value writes.
            (
                "widescreen",
                upd(|s| s.widescreen = Some(String::new())),
                upd(|s| s.widescreen = Some("16:9".into())),
            ),
            (
                "fill_screen",
                upd(|s| s.fill_screen = Some(None)),
                upd(|s| s.fill_screen = Some(Some(true))),
            ),
            (
                "fullscreen_resolution",
                upd(|s| s.fullscreen_resolution = Some(None)),
                upd(|s| s.fullscreen_resolution = Some(Some("640x480".into()))),
            ),
        ];
        for (key, default_upd, choice_upd) in cases {
            let mut t = toml::Table::new();
            apply(&mut t, &choice_upd).unwrap();
            assert!(
                t["graphics"].get(key).is_some(),
                "{key}: a choice is written"
            );
            apply(&mut t, &default_upd).unwrap();
            assert!(
                t["graphics"].get(key).is_none(),
                "{key}: the built-in default is removed"
            );
        }
        // OLED care: its own table, same rule.
        let mut t: toml::Table =
            toml::from_str("[oled]\ndrift = true\nbrightness = 0.7\n").unwrap();
        apply(
            &mut t,
            &upd(|s| {
                s.oled_drift = Some(false);
                s.oled_brightness = Some(1.0);
            }),
        )
        .unwrap();
        assert!(t["oled"].get("drift").is_none(), "oled.drift removed");
        assert!(
            t["oled"].get("brightness").is_none(),
            "oled.brightness removed"
        );
        // Unrelated keys survive the removals.
        let mut t: toml::Table =
            toml::from_str("[graphics]\nmsaa = 4\nfuture = 7\n\n[rom]\npath = \"x\"\n").unwrap();
        apply(&mut t, &upd(|s| s.msaa = Some(1))).unwrap();
        assert_eq!(t["graphics"]["future"].as_integer(), Some(7));
        assert_eq!(t["rom"]["path"].as_str(), Some("x"));
    }

    /// U18, low-end variant: with the preset active its fps 60 / scale
    /// 0.75 are the effective defaults, so they are removed too; other
    /// defaults and real choices are unaffected.
    #[test]
    fn low_end_preset_defaults_removed() {
        let mut t = toml::Table::new();
        super::apply(
            &mut t,
            &SavedSettings {
                fps: Some("60".into()),
                scale: Some(0.75),
                ..Default::default()
            },
            true,
        )
        .unwrap();
        assert!(
            t["graphics"].get("fps").is_none() && t["graphics"].get("scale").is_none(),
            "the preset's defaults are removed with the preset active"
        );
        // Choices different from the preset's defaults are written.
        super::apply(
            &mut t,
            &SavedSettings {
                fps: Some("144".into()),
                scale: Some(2.0),
                ..Default::default()
            },
            true,
        )
        .unwrap();
        assert_eq!(t["graphics"]["fps"].as_integer(), Some(144));
        assert_eq!(t["graphics"]["scale"].as_float(), Some(2.0));
        // The plain defaults are not affected by the preset.
        super::apply(
            &mut t,
            &SavedSettings {
                msaa: Some(1),
                vsync: Some(false),
                ..Default::default()
            },
            true,
        )
        .unwrap();
        assert!(
            t["graphics"].get("msaa").is_none() && t["graphics"].get("vsync").is_none(),
            "plain defaults still removed"
        );
    }

    /// U18: a binding map equal to the full built-in default is removed
    /// like an empty map (nothing is rebound, nothing is pinned).
    #[test]
    fn default_input_maps_are_removed() {
        // The map really is the built-in default: the first default input
        // of every slot, in the settings screen's `format!("{k:?}")`
        // spelling.
        let defaults = (builtin_input_map("keyboard"), builtin_input_map("gamepad"));
        for (_, slot, _) in crate::input::KEYBOARD_SLOTS {
            assert!(
                defaults.0.contains_key(slot),
                "every keyboard slot has a default entry"
            );
        }
        for (_, slot, _) in crate::input::GAMEPAD_SLOTS {
            assert!(
                defaults.1.contains_key(slot),
                "every gamepad slot has a default entry"
            );
        }
        assert_eq!(defaults.0["A"], "Space");
        assert_eq!(defaults.0["Z"], "KeyZ", "first default of a multi-key slot");
        assert_eq!(defaults.0["STICK_UP"], "KeyW");
        assert_eq!(defaults.1["A"], "South");
        assert_eq!(defaults.1["B"], "West", "first default of a multi-key slot");
        let maps = |k: BTreeMap<String, String>, g: BTreeMap<String, String>| SavedSettings {
            keyboard: Some(k),
            gamepad: Some(g),
            ..Default::default()
        };
        let mut t = toml::Table::new();
        apply(&mut t, &maps(defaults.0.clone(), defaults.1.clone())).unwrap();
        assert!(
            t["input"].get("keyboard").is_none(),
            "the full default map is removed"
        );
        assert!(t["input"].get("gamepad").is_none());
        // A map with one real rebinding is written (the other map stays
        // at the default and is removed).
        let mut rebound = defaults.0.clone();
        rebound.insert("A".into(), "KeyX".into());
        apply(&mut t, &maps(rebound, defaults.1)).unwrap();
        assert_eq!(t["input"]["keyboard"]["A"].as_str(), Some("KeyX"));
    }

    #[test]
    fn invalid_toml_yields_defaults() {
        // Unclosed table: an error, not a panic (the loader falls back).
        assert!(parse("[graphics\nmsaa = 4\n").is_err());
        assert!(parse("msaa = not a number").is_err());
    }

    /// S1: one hand-edited bad value only loses that key; the settings,
    /// bindings and remembered ROM survive. The lenient forms (integer
    /// 0/1 for booleans, numeric strings for numbers) parse.
    #[test]
    fn one_bad_value_loses_only_that_key() {
        // `vsync = 1` (integer for a bool) is accepted, and nothing else
        // in the file is lost.
        let c = parse(
            "[graphics]\nvsync = 1\nmsaa = 4\n\n[input.keyboard]\nA = \"Space\"\n\n[rom]\npath = \"x\"\n",
        )
        .unwrap();
        assert!(c.graphics.vsync);
        assert_eq!(c.graphics.msaa, Some(4));
        assert_eq!(c.input.keyboard.get("A").map(String::as_str), Some("Space"));
        assert_eq!(c.rom.path.as_deref(), Some("x"));
        assert!(c.ignored.is_empty(), "lenient form is not an error");

        // `msaa = "4"` (numeric string) parses, other keys survive.
        let c = parse("[graphics]\nmsaa = \"4\"\nscale = 2.0\n").unwrap();
        assert_eq!(c.graphics.msaa, Some(4));
        assert_eq!(c.graphics.scale, Some(2.0));
        assert!(c.ignored.is_empty());

        // `A = 32` (a number where a key name belongs) drops only that
        // slot; the other bindings and the graphics section survive.
        let c = parse("[input.keyboard]\nA = 32\nB = \"KeyX\"\n\n[graphics]\nmsaa = 4\n").unwrap();
        assert!(!c.input.keyboard.contains_key("A"), "bad slot dropped");
        assert_eq!(c.input.keyboard.get("B").map(String::as_str), Some("KeyX"));
        assert_eq!(c.graphics.msaa, Some(4));
        assert_eq!(c.ignored, vec!["input.keyboard.A".to_string()]);
    }

    /// S1: a whole section with the wrong type drops only that section,
    /// and the skipped names collect for the window's toast.
    #[test]
    fn wrong_type_sections_and_values_are_recorded() {
        let c = parse("rom = 3\n\n[graphics]\nmsaa = 4\n\n[oled]\ndrift = true\n").unwrap();
        assert_eq!(c.graphics.msaa, Some(4));
        assert!(c.oled.drift);
        assert_eq!(c.rom.path, None, "non-table [rom] section dropped");
        assert_eq!(c.ignored, vec!["rom".to_string()]);
        // A wrong type for a plain key is recorded too.
        let c = parse("[graphics]\nmsaa = 4\nvolume = \"loud\"\n").unwrap();
        assert_eq!(c.graphics.volume, 1.0, "kept the default");
        assert_eq!(c.graphics.msaa, Some(4));
        assert_eq!(c.ignored, vec!["graphics.volume".to_string()]);
        // Invalid values (not just wrong types) are recorded as well.
        let c = parse("[graphics]\nmsaa = 3\n").unwrap();
        assert_eq!(c.graphics.msaa, None);
        assert_eq!(c.ignored, vec!["graphics.msaa".to_string()]);
    }

    /// `config::ignored_keys()` surfaces what the loader skipped.
    #[test]
    fn ignored_keys_fn_reads_the_loaded_config() {
        // `get()` reads the developer's real pw64.toml; just check the
        // function is wired and never panics (the toast reads it later).
        let _ = ignored_keys();
    }

    #[test]
    fn invalid_values_are_dropped() {
        let c = parse(
            "[graphics]\nmsaa = 3\nscale = 0.1\nscale_filter = \"smash\"\nfilter = \"trilinear\"\n",
        )
        .unwrap();
        assert_eq!(c.graphics.msaa, None);
        assert_eq!(c.graphics.scale, None);
        assert_eq!(c.graphics.scale_filter, None);
        assert_eq!(c.graphics.filter, None);
        // 0.5 is the new lower bound for the render scale (S6).
        let c = parse("[graphics]\nscale = 0.5\n").unwrap();
        assert_eq!(c.graphics.scale, Some(0.5));
        // Partial files keep what they have.
        let c = parse("[input.keyboard]\nA = \"Space\"\n").unwrap();
        assert!(c.input.keyboard.contains_key("A"));
        assert_eq!(c.graphics.msaa, None);
    }

    #[test]
    fn precedence_env_config_default() {
        // env > config > default, independent of the type.
        assert_eq!(opts::precedence(Some(8u32), Some(4), 1), 8);
        assert_eq!(opts::precedence(None, Some(4u32), 1), 4);
        assert_eq!(opts::precedence(None, None, 1), 1);
        assert_eq!(opts::precedence(Some(1.5f32), None, 1.0), 1.5);
    }

    #[test]
    fn parses_settings_triggers_and_volume() {
        let c = parse(
            "[graphics]\nvolume = 0.35\n\n[input]\nsettings_key = \"F9\"\n\
             settings_pad_button = \"Select\"\n",
        )
        .unwrap();
        assert_eq!(c.graphics.volume, 0.35);
        assert_eq!(c.input.settings.key.as_deref(), Some("F9"));
        assert_eq!(c.input.settings.pad_button.as_deref(), Some("Select"));
        // Defaults when the tables are absent.
        let d = parse("").unwrap();
        assert_eq!(d.graphics.volume, 1.0);
        assert!(d.input.settings.key.is_none());
        assert!(d.input.settings.pad_button.is_none());
    }

    #[test]
    fn volume_out_of_range_is_dropped() {
        let c = parse("[graphics]\nvolume = 1.5\n").unwrap();
        assert_eq!(c.graphics.volume, 1.0);
    }

    #[test]
    fn apply_round_trips_unknown_keys() {
        let mut table: toml::Table =
            toml::from_str("[graphics]\nmsaa = 1\nfuture_thing = 7\n\n[extra]\nx = \"y\"\n")
                .unwrap();
        apply(
            &mut table,
            &SavedSettings {
                msaa: Some(4),
                volume: Some(0.35),
                ..Default::default()
            },
        )
        .unwrap();
        let out = toml::to_string_pretty(&table).unwrap();
        let back: toml::Table = toml::from_str(&out).unwrap();
        assert_eq!(back["graphics"]["msaa"].as_integer(), Some(4));
        // Written as 0.35, not f32 noise (0.3499999940395355).
        assert_eq!(back["graphics"]["volume"].as_float(), Some(0.35));
        // Keys the screen doesn't know survive the write.
        assert_eq!(back["graphics"]["future_thing"].as_integer(), Some(7));
        assert_eq!(back["extra"]["x"].as_str(), Some("y"));
        // A non-table [graphics] is an error, not a silent "saved".
        let mut bad: toml::Table = toml::from_str("graphics = 3\n").unwrap();
        assert!(apply(&mut bad, &SavedSettings::default()).is_err());
    }

    #[test]
    fn apply_fill_screen_writes_or_removes() {
        let mut table: toml::Table = toml::from_str("[graphics]\nfill_screen = false\n").unwrap();
        let fill = |t: &toml::Table| t["graphics"].get("fill_screen").and_then(|v| v.as_bool());
        let upd = |f| SavedSettings {
            fill_screen: f,
            ..Default::default()
        };
        apply(&mut table, &upd(None)).unwrap();
        assert_eq!(fill(&table), Some(false), "None leaves the key");
        apply(&mut table, &upd(Some(Some(true)))).unwrap();
        assert_eq!(fill(&table), Some(true));
        apply(&mut table, &upd(Some(None))).unwrap();
        assert_eq!(fill(&table), None, "back to the default removes the key");
    }

    #[test]
    fn display_mode_round_trips() {
        use crate::opts::parse_display_mode;
        let c = parse("[graphics]\ndisplay_mode = \"borderless\"\n").unwrap();
        assert_eq!(c.graphics.display_mode.as_deref(), Some("borderless"));
        let c = parse("[graphics]\ndisplay_mode = \"wide\"\n").unwrap();
        assert_eq!(c.graphics.display_mode, None, "invalid dropped");
        let d = parse("").unwrap();
        assert_eq!(d.graphics.display_mode, None);
        // Saved, and read back through `parse`.
        let mut t = toml::Table::new();
        apply(
            &mut t,
            &SavedSettings {
                display_mode: Some("exclusive"),
                ..Default::default()
            },
        )
        .unwrap();
        let c = parse(&toml::to_string_pretty(&t).unwrap()).unwrap();
        assert_eq!(
            c.graphics
                .display_mode
                .as_deref()
                .and_then(parse_display_mode),
            Some(opts::DisplayMode::Exclusive)
        );
    }

    #[test]
    fn fullscreen_resolution_writes_or_removes() {
        use crate::opts::parse_resolution;
        let mut table: toml::Table =
            toml::from_str("[graphics]\nfullscreen_resolution = \"640x480\"\n").unwrap();
        let res = |t: &toml::Table| {
            t["graphics"]
                .get("fullscreen_resolution")
                .and_then(|v| v.as_str().map(String::from))
        };
        let upd = |r| SavedSettings {
            fullscreen_resolution: r,
            ..Default::default()
        };
        apply(&mut table, &upd(None)).unwrap();
        assert_eq!(res(&table).as_deref(), Some("640x480"), "None leaves it");
        apply(&mut table, &upd(Some(Some("1280x720@60".into())))).unwrap();
        assert_eq!(res(&table).as_deref(), Some("1280x720@60"));
        apply(&mut table, &upd(Some(None))).unwrap();
        assert_eq!(res(&table), None, "back to desktop removes the key");
        // Both spellings read back as a valid `Res`.
        for v in ["640x480", "1280x720@60"] {
            assert!(parse_resolution(v).is_some());
        }
        let c = parse("[graphics]\nfullscreen_resolution = \"0x0\"\n").unwrap();
        assert_eq!(c.graphics.fullscreen_resolution, None, "invalid dropped");
    }

    #[test]
    fn fps_accepts_strings_and_numbers() {
        let c = parse("[graphics]\nfps = 144\n").unwrap();
        assert_eq!(c.graphics.fps.as_deref(), Some("144"));
        let c = parse("[graphics]\nfps = \"monitor\"\n").unwrap();
        assert_eq!(c.graphics.fps.as_deref(), Some("monitor"));
        let c = parse("[graphics]\nfps = 0\nmsaa = 4\n").unwrap();
        assert_eq!(c.graphics.fps.as_deref(), Some("0"));
        assert_eq!(c.graphics.msaa, Some(4));
        let c = parse("[graphics]\nfps = 12\n").unwrap();
        assert_eq!(c.graphics.fps, None);
        // Saved as an integer or a string, and read back. (The default
        // spelling "monitor" is no longer written: it is removed, see
        // `default_values_removed_non_defaults_written`.)
        let mut t = toml::Table::new();
        for v in ["144", "0"] {
            apply(
                &mut t,
                &SavedSettings {
                    fps: Some(v.into()),
                    ..Default::default()
                },
            )
            .unwrap();
            let c = parse(&toml::to_string_pretty(&t).unwrap()).unwrap();
            assert_eq!(c.graphics.fps.as_deref(), Some(v));
        }
        assert_eq!(
            t["graphics"]["fps"].as_integer(),
            Some(0),
            "saved as a number"
        );
    }

    #[test]
    fn widescreen_accepts_numbers_and_drops_invalid() {
        // `widescreen = 1` (as documented) must not discard the whole file.
        let c = parse("[graphics]\nwidescreen = 1\nmsaa = 4\n").unwrap();
        assert_eq!(c.graphics.widescreen.as_deref(), Some("1"));
        assert_eq!(c.graphics.msaa, Some(4));
        let c = parse("[graphics]\nwidescreen = 2.33\n").unwrap();
        assert_eq!(c.graphics.widescreen.as_deref(), Some("2.33"));
        let c = parse("[graphics]\nwidescreen = 0\n").unwrap();
        assert_eq!(c.graphics.widescreen.as_deref(), Some("0"));
        let c = parse("[graphics]\nwidescreen = \"wide\"\n").unwrap();
        assert_eq!(c.graphics.widescreen, None);
    }

    #[test]
    fn input_maps_write_or_remove() {
        let mut table: toml::Table = toml::from_str(
            "[input]\nsettings_key = \"F9\"\n\n[input.keyboard]\nA = \"KeyX\"\n\n\
             [input.gamepad]\nZ = \"South\"\n\n[input.ble]\nenabled = true\n",
        )
        .unwrap();
        let upd = |k, g| SavedSettings {
            keyboard: k,
            gamepad: g,
            ..Default::default()
        };
        // A rebinding Save replaces the sub-tables wholesale; the settings
        // trigger and `[input.ble]` survive.
        apply(
            &mut table,
            &upd(
                Some(
                    [("B".to_string(), "KeyY".to_string())]
                        .into_iter()
                        .collect(),
                ),
                Some(
                    [("A".to_string(), "East".to_string())]
                        .into_iter()
                        .collect(),
                ),
            ),
        )
        .unwrap();
        let back: toml::Table = toml::from_str(&toml::to_string_pretty(&table).unwrap()).unwrap();
        assert_eq!(back["input"]["settings_key"].as_str(), Some("F9"));
        assert_eq!(back["input"]["ble"]["enabled"].as_bool(), Some(true));
        assert_eq!(back["input"]["keyboard"]["B"].as_str(), Some("KeyY"));
        assert!(
            back["input"]["keyboard"].get("A").is_none(),
            "replaced, not merged"
        );
        assert_eq!(back["input"]["gamepad"]["A"].as_str(), Some("East"));
        assert!(
            back["input"]["gamepad"].get("Z").is_none(),
            "replaced, not merged"
        );
        // Empty maps remove the sub-tables; `[input]` itself and
        // `[input.ble]` stay.
        apply(
            &mut table,
            &upd(Some(BTreeMap::new()), Some(BTreeMap::new())),
        )
        .unwrap();
        let back: toml::Table = toml::from_str(&toml::to_string_pretty(&table).unwrap()).unwrap();
        assert!(back["input"].get("keyboard").is_none());
        assert!(back["input"].get("gamepad").is_none());
        assert_eq!(back["input"]["settings_key"].as_str(), Some("F9"));
        assert_eq!(back["input"]["ble"]["enabled"].as_bool(), Some(true));
        // The written file parses back into the validated config (empty
        // maps = defaults; the trigger still reads).
        let c = parse(&toml::to_string_pretty(&table).unwrap()).unwrap();
        assert!(c.input.keyboard.is_empty());
        assert!(c.input.gamepad.is_empty());
        assert_eq!(c.input.settings.key.as_deref(), Some("F9"));
        assert_eq!(c.input.ble, Some(true));
        // None leaves both sub-tables alone.
        apply(&mut table, &upd(None, None)).unwrap();
        let back: toml::Table = toml::from_str(&toml::to_string_pretty(&table).unwrap()).unwrap();
        assert!(back["input"].get("keyboard").is_none(), "still absent");
        // A non-table [input] is an error, not a silent "saved".
        let mut bad: toml::Table = toml::from_str("input = 3\n").unwrap();
        assert!(apply(&mut bad, &upd(Some(BTreeMap::new()), None)).is_err());
    }

    #[test]
    fn ui_hint_flag_round_trips() {
        let c = parse("[ui]\nsettings_hint_shown = true\n").unwrap();
        assert!(c.ui.settings_hint_shown);
        // Absent → false (the hint has not been shown yet).
        let d = parse("[graphics]\nmsaa = 4\n").unwrap();
        assert!(!d.ui.settings_hint_shown);
        // U26: the background-pause key parses; the default is on.
        let p = parse("[ui]\npause_in_background = false\n").unwrap();
        assert!(!p.ui.pause_in_background);
        assert!(d.ui.pause_in_background, "the default is on");
        // The settings screen writes it into [ui] (U26): a non-default
        // value is written, the default is removed (U18), like [graphics].
        let mut t = toml::Table::new();
        apply(
            &mut t,
            &SavedSettings {
                show_fps: Some(true),
                pause_in_background: Some(false),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(t["graphics"]["show_fps"].as_bool(), Some(true));
        assert_eq!(t["ui"]["pause_in_background"].as_bool(), Some(false));
        // Back to the defaults: the keys are removed, not written (U18).
        apply(
            &mut t,
            &SavedSettings {
                show_fps: Some(false),
                pause_in_background: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(t["graphics"].get("show_fps").is_none());
        assert!(t["ui"].get("pause_in_background").is_none());
        // A non-table [ui] is an error, not a silent "saved".
        let mut bad: toml::Table = toml::from_str("ui = 3\n").unwrap();
        assert!(
            apply(
                &mut bad,
                &SavedSettings {
                    pause_in_background: Some(false),
                    ..Default::default()
                }
            )
            .is_err()
        );
        // The writer's table round-trip keeps unrelated keys, and the flag
        // reads back through `parse`.
        let table: toml::Table = toml::from_str("[graphics]\nmsaa = 4\nfuture = 7\n").unwrap();
        let table = with_ui_flag(table).unwrap();
        let text = toml::to_string_pretty(&table).unwrap();
        assert!(parse(&text).unwrap().ui.settings_hint_shown);
        let back: toml::Table = toml::from_str(&text).unwrap();
        assert_eq!(back["ui"]["settings_hint_shown"].as_bool(), Some(true));
        assert_eq!(back["graphics"]["msaa"].as_integer(), Some(4));
        assert_eq!(back["graphics"]["future"].as_integer(), Some(7));
        // A non-table [ui] is an error, not a silent "saved".
        let bad: toml::Table = toml::from_str("ui = 3\n").unwrap();
        assert!(with_ui_flag(bad).is_err());
    }

    #[test]
    fn rom_path_round_trips() {
        let c = parse("[rom]\npath = \"C:/roms/pw64.z64\"\n").unwrap();
        assert_eq!(c.rom.path.as_deref(), Some("C:/roms/pw64.z64"));
        // Absent → none (until the first-run dialog ran).
        let d = parse("[graphics]\nmsaa = 4\n").unwrap();
        assert_eq!(d.rom.path, None);
        // The writer's table round-trip keeps unrelated keys, and a backslash
        // path survives the TOML escaping.
        let table: toml::Table = toml::from_str("[graphics]\nmsaa = 4\nfuture = 7\n").unwrap();
        let table = with_rom_path(table, std::path::Path::new(r"C:\roms\pw64.z64")).unwrap();
        let back: toml::Table = toml::from_str(&toml::to_string_pretty(&table).unwrap()).unwrap();
        assert_eq!(back["rom"]["path"].as_str(), Some(r"C:\roms\pw64.z64"));
        assert_eq!(back["graphics"]["msaa"].as_integer(), Some(4));
        assert_eq!(back["graphics"]["future"].as_integer(), Some(7));
        // A non-table [rom] is an error, not a silent "saved".
        let bad: toml::Table = toml::from_str("rom = 3\n").unwrap();
        assert!(with_rom_path(bad, std::path::Path::new("x")).is_err());
    }
}
