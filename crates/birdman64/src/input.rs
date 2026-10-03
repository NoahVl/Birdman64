//! Host input → N64 controller 1 (docs/notes/input.md).
//!
//! A background thread polls `gilrs` (Xbox via XInput/WGI, DualShock 4 /
//! DualSense / Switch Pro via WGI raw controllers + the SDL mapping DB) and
//! merges every connected pad with the keyboard state — plus the virtual
//! Switch 2 pads over BLE (`ble.rs`, off by default) — into one N64 pad,
//! published through `pw64_platform::headless::set_controller1`. Only
//! controller 1 exists: the game reads `osContInit`'s pattern once and is
//! single-player, so extra pads just merge into pad 1. Button bindings are
//! configurable in `pw64.toml` (`[input.keyboard]` / `[input.gamepad]`,
//! parsed against winit / gilrs names below); unlisted slots keep their
//! defaults. Keyboard stick directions are slots too (`STICK_UP` …), bound
//! the same way.

use crate::config;
use gilrs::{Axis, Button, Gilrs};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use winit::keyboard::KeyCode;

// libultra `os_cont.h` button bits.
pub const CONT_A: u16 = 0x8000;
pub const CONT_B: u16 = 0x4000;
pub const CONT_Z: u16 = 0x2000;
pub const CONT_START: u16 = 0x1000;
pub const CONT_UP: u16 = 0x0800;
pub const CONT_DOWN: u16 = 0x0400;
pub const CONT_LEFT: u16 = 0x0200;
pub const CONT_RIGHT: u16 = 0x0100;
pub const CONT_L: u16 = 0x0020;
pub const CONT_R: u16 = 0x0010;
pub const CONT_C_UP: u16 = 0x0008;
pub const CONT_C_DOWN: u16 = 0x0004;
pub const CONT_C_LEFT: u16 = 0x0002;
pub const CONT_C_RIGHT: u16 = 0x0001;

/// Keyboard stick slots live above the CONT button bits (`1 << 16 .. 1 << 19`):
/// `key_mapping` splits a key's OR of slot bits back into CONT buttons (low
/// 16) and a stick direction (high).
const STICK_UP_BIT: u32 = 1 << 16;
const STICK_DOWN_BIT: u32 = 1 << 17;
const STICK_LEFT_BIT: u32 = 1 << 18;
const STICK_RIGHT_BIT: u32 = 1 << 19;

/// Radial deadzone of a modern stick (fraction of full deflection).
const STICK_DEADZONE: f32 = 0.12;
/// `uvReadController` (kernel/system.c) ignores |axis| < 7 and saturates at
/// 68 (x) / 70 (y). Output starts at 7 so the host deadzone replaces the
/// game's instead of stacking on it.
const GAME_DEADZONE: f32 = 7.0;
/// Nominal N64 stick range (`os_cont.h`: -80 <= stick_x <= 80).
const N64_RANGE: f32 = 80.0;
/// Octagonal gate corner per axis, as a fraction of the 0..1 range that
/// `stick_to_n64` maps to 7..80: 0.875 → ~71, a real pad's ~70,70 corner.
const OCTAGON_CORNER: f32 = 0.875;
/// Right stick → C buttons threshold, and analog trigger → button.
const C_STICK_THRESHOLD: f32 = 0.5;
const TRIGGER_THRESHOLD: f32 = 0.3;
const POLL_INTERVAL: Duration = Duration::from_millis(4);

/// U26: the last pad must stay gone this long before the game pauses (a
/// Bluetooth pad that drops and reconnects at once shouldn't pause).
const PAUSE_GRACE: Duration = Duration::from_millis(1500);

/// One N64 pad state.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Pad {
    pub button: u16,
    pub stick_x: i8,
    pub stick_y: i8,
}

impl Pad {
    /// Buttons OR'd; the stick with the larger deflection wins.
    pub fn merge(self, o: Pad) -> Pad {
        let mag = |p: &Pad| (p.stick_x as i32).abs() + (p.stick_y as i32).abs();
        let s = if mag(&o) > mag(&self) { o } else { self };
        Pad {
            button: self.button | o.button,
            stick_x: s.stick_x,
            stick_y: s.stick_y,
        }
    }
}

/// Modern stick (x right, y up, each -1..1) → N64 `stick_x`/`stick_y`.
/// Radial deadzone, remap the rest to 0..1, then per axis
/// `7 + |a| * (80 - 7)` so the first step past the deadzone is the game's
/// first live value and the game's saturation (68/70) stays reachable.
///
/// The round range is stretched onto the N64's octagonal gate: cardinals
/// unchanged, a full diagonal reaches ~71 on both axes (a real pad's gate
/// corner is ~70,70). The game saturates each axis at 68/70 (system.c
/// `uvController` read), so on hardware a full diagonal is full on both
/// axes; a round 59,59 diagonal read as 0.83 per axis, and the glider's
/// squared stick curve cut pitch authority to ~68 % whenever you steered.
pub fn stick_to_n64(x: f32, y: f32) -> (i8, i8) {
    let (x, y) = (x.clamp(-1.0, 1.0), y.clamp(-1.0, 1.0));
    let mag = (x * x + y * y).sqrt();
    if !mag.is_finite() || mag <= STICK_DEADZONE {
        return (0, 0);
    }
    let scaled = ((mag - STICK_DEADZONE) / (1.0 - STICK_DEADZONE)).min(1.0);
    // Octagon radius along this direction: edges run from the cardinal
    // vertex (1,0) to the corner (G,G), G = OCTAGON_CORNER. On that edge
    // big + small·(1-G)/G = 1, so the radius is 1 / (big + small·(1-G)/G).
    let (big, small) = (x.abs().max(y.abs()) / mag, x.abs().min(y.abs()) / mag);
    let gate = 1.0 / (big + small * (1.0 - OCTAGON_CORNER) / OCTAGON_CORNER);
    let k = scaled * gate / mag;
    let axis = |a: f32| -> i8 {
        let a = (a * k).clamp(-1.0, 1.0);
        if a == 0.0 {
            return 0;
        }
        let v = GAME_DEADZONE + a.abs() * (N64_RANGE - GAME_DEADZONE);
        (v.round().min(N64_RANGE) * a.signum()) as i8
    };
    (axis(x), axis(y))
}

/// Right stick → C buttons (each direction past the threshold).
pub fn c_stick_buttons(x: f32, y: f32) -> u16 {
    let mut b = 0;
    if x >= C_STICK_THRESHOLD {
        b |= CONT_C_RIGHT;
    }
    if x <= -C_STICK_THRESHOLD {
        b |= CONT_C_LEFT;
    }
    if y >= C_STICK_THRESHOLD {
        b |= CONT_C_UP;
    }
    if y <= -C_STICK_THRESHOLD {
        b |= CONT_C_DOWN;
    }
    b
}

/// Keyboard slots for `pw64.toml` `[input.keyboard]`: CONT button bits plus
/// the four stick directions (`1 << 16 .. 1 << 19`, so one `resolve` pass
/// treats the stick like any other slot: an input bound elsewhere is taken
/// off the stick's defaults), config name, and the default keys used when
/// the slot isn't overridden.
pub(crate) const KEYBOARD_SLOTS: [(u32, &str, &[KeyCode]); 18] = [
    (CONT_A as u32, "A", &[KeyCode::Space]),
    (CONT_B as u32, "B", &[KeyCode::ShiftLeft]),
    (CONT_Z as u32, "Z", &[KeyCode::KeyZ, KeyCode::ControlLeft]),
    (CONT_L as u32, "L", &[KeyCode::KeyQ]),
    (CONT_R as u32, "R", &[KeyCode::KeyE]),
    (CONT_START as u32, "START", &[KeyCode::Enter]),
    (CONT_C_UP as u32, "C_UP", &[KeyCode::KeyI]),
    (CONT_C_DOWN as u32, "C_DOWN", &[KeyCode::KeyK]),
    (CONT_C_LEFT as u32, "C_LEFT", &[KeyCode::KeyJ]),
    (CONT_C_RIGHT as u32, "C_RIGHT", &[KeyCode::KeyL]),
    (CONT_UP as u32, "UP", &[KeyCode::KeyT]),
    (CONT_DOWN as u32, "DOWN", &[KeyCode::KeyG]),
    (CONT_LEFT as u32, "LEFT", &[KeyCode::KeyF]),
    (CONT_RIGHT as u32, "RIGHT", &[KeyCode::KeyH]),
    (STICK_UP_BIT, "STICK_UP", &[KeyCode::KeyW, KeyCode::ArrowUp]),
    (
        STICK_DOWN_BIT,
        "STICK_DOWN",
        &[KeyCode::KeyS, KeyCode::ArrowDown],
    ),
    (
        STICK_LEFT_BIT,
        "STICK_LEFT",
        &[KeyCode::KeyA, KeyCode::ArrowLeft],
    ),
    (
        STICK_RIGHT_BIT,
        "STICK_RIGHT",
        &[KeyCode::KeyD, KeyCode::ArrowRight],
    ),
];

/// Same for `[input.gamepad]` (positional defaults, so Nintendo layouts
/// match). CONT bits only: the gamepad stick is not rebindable.
pub(crate) const GAMEPAD_SLOTS: [(u32, &str, &[Button]); 11] = [
    (CONT_A as u32, "A", &[Button::South]),
    (CONT_B as u32, "B", &[Button::West, Button::East]),
    (CONT_Z as u32, "Z", &[Button::LeftTrigger2]),
    (CONT_L as u32, "L", &[Button::LeftTrigger]),
    (
        CONT_R as u32,
        "R",
        &[Button::RightTrigger, Button::RightTrigger2],
    ),
    (CONT_START as u32, "START", &[Button::Start]),
    (CONT_C_UP as u32, "C_UP", &[Button::North]),
    (CONT_UP as u32, "UP", &[Button::DPadUp]),
    (CONT_DOWN as u32, "DOWN", &[Button::DPadDown]),
    (CONT_LEFT as u32, "LEFT", &[Button::DPadLeft]),
    (CONT_RIGHT as u32, "RIGHT", &[Button::DPadRight]),
];

/// One N64 slot's resolved host inputs (config override or defaults).
struct Binding<K> {
    bit: u32,
    name: &'static str,
    inputs: Vec<K>,
}

/// Resolves a slot table against its `pw64.toml` section. Slot names match
/// case-insensitively; unknown slots / input names warn (once, at startup)
/// and are ignored. An overridden slot replaces *all* its defaults, and an
/// input bound explicitly anywhere is taken out of every other slot's
/// defaults — so `R = "LeftTrigger2"` moves ZL from Z to R instead of the
/// first table entry silently winning. `reserved` (the settings-screen
/// trigger) is never bound: only the overlay consumes it.
fn resolve<K: Copy + PartialEq + std::fmt::Debug>(
    table: &[(u32, &'static str, &[K])],
    cfg: &HashMap<String, String>,
    section: &str,
    parse: fn(&str) -> Option<K>,
    reserved: Option<K>,
) -> Vec<Binding<K>> {
    let mut overrides: Vec<(usize, K)> = Vec::new();
    for (slot, value) in cfg {
        let Some(i) = table
            .iter()
            .position(|(_, n, _)| n.eq_ignore_ascii_case(slot.trim()))
        else {
            let names: Vec<_> = table.iter().map(|(_, n, _)| *n).collect();
            eprintln!(
                "[config] [input.{section}] unknown slot {slot:?} (one of {}); ignored",
                names.join(" ")
            );
            continue;
        };
        match parse(value) {
            Some(k) if Some(k) == reserved => eprintln!(
                "[config] [input.{section}] {slot} = {value:?} is the settings-screen \
                 trigger; slot keeps its defaults"
            ),
            Some(k) => overrides.push((i, k)),
            None => eprintln!(
                "[config] [input.{section}] {slot} = {value:?}: unknown name; slot keeps \
                 its defaults"
            ),
        }
    }
    table
        .iter()
        .enumerate()
        .map(|(i, &(bit, name, defaults))| {
            let explicit: Vec<K> = overrides
                .iter()
                .filter(|(j, _)| *j == i)
                .map(|&(_, k)| k)
                .collect();
            let inputs = if explicit.is_empty() {
                defaults
                    .iter()
                    .copied()
                    .filter(|d| Some(*d) != reserved && !overrides.iter().any(|(_, k)| k == d))
                    .collect()
            } else {
                explicit
            };
            Binding { bit, name, inputs }
        })
        .collect()
}

/// OR of every slot bit `input` is bound to (CONT bits below `1 << 16`,
/// keyboard stick slots above).
fn bits_for<K: PartialEq>(bindings: &[Binding<K>], input: K) -> u32 {
    bindings
        .iter()
        .filter(|b| b.inputs.contains(&input))
        .fold(0, |acc, b| acc | b.bit)
}

/// Slot name → bound inputs as text, for the settings screen's read-only list.
fn binding_rows<K: std::fmt::Debug>(bindings: &[Binding<K>]) -> Vec<(&'static str, String)> {
    bindings
        .iter()
        .map(|b| {
            let names: Vec<_> = b.inputs.iter().map(|k| format!("{k:?}")).collect();
            let text = if names.is_empty() {
                "(unbound)".to_string()
            } else {
                names.join(" / ")
            };
            (b.name, text)
        })
        .collect()
}

/// One kind's live bindings (keyboard or gamepad): the override map (config
/// first, then the settings screen's rebinds) and the bindings resolved from
/// it. Readers (`key_mapping` per key event, `gamepad_pad` every 4 ms per
/// pad) take an `Arc` snapshot under the mutex and scan it unlocked; bind and
/// reset swap in a fresh `Arc`. No guard ever escapes `with`, so there is no
/// reader/writer ordering to get wrong (an `RwLock` read guard held across a
/// rebind self-deadlocks).
struct Live<K> {
    overrides: HashMap<String, String>,
    bindings: Arc<Vec<Binding<K>>>,
}

struct LiveTable<K: 'static> {
    state: Mutex<Option<Live<K>>>,
    /// The `pw64.toml` section, loaded on first use.
    config: fn() -> &'static HashMap<String, String>,
    resolve: fn(&HashMap<String, String>) -> Vec<Binding<K>>,
    parse: fn(&str) -> Option<K>,
}

impl<K: Copy + PartialEq + std::fmt::Debug> LiveTable<K> {
    /// Runs `f` on the state, built from the config on first use. Never call
    /// another `LiveTable` method from inside `f`.
    fn with<R>(&self, f: impl FnOnce(&mut Live<K>) -> R) -> R {
        let mut g = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let live = g.get_or_insert_with(|| {
            let overrides = (self.config)().clone();
            Live {
                bindings: Arc::new((self.resolve)(&overrides)),
                overrides,
            }
        });
        f(live)
    }

    fn snapshot(&self) -> Arc<Vec<Binding<K>>> {
        self.with(|l| l.bindings.clone())
    }

    fn overrides(&self) -> BTreeMap<String, String> {
        self.with(|l| {
            l.overrides
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
    }

    /// Edits the override map and re-resolves the bindings.
    fn edit(&self, f: impl FnOnce(&mut HashMap<String, String>)) {
        let resolve = self.resolve;
        self.with(|l| {
            f(&mut l.overrides);
            l.bindings = Arc::new(resolve(&l.overrides));
        })
    }

    /// One input per slot: drops the slot's own override (any spelling of
    /// its name, as `resolve` matches case-insensitively) and every other
    /// override that parses to `k` (aliases too), then binds `slot = k`.
    fn bind(&self, slot: &'static str, k: K) {
        let parse = self.parse;
        self.edit(|m| {
            m.retain(|s, v| !s.trim().eq_ignore_ascii_case(slot) && parse(v) != Some(k));
            m.insert(slot.to_string(), format!("{k:?}"));
        })
    }
}

static KEYBOARD: LiveTable<KeyCode> = LiveTable {
    state: Mutex::new(None),
    config: || &config::get().input.keyboard,
    // The settings key never reaches `on_key` (window.rs consumes it), so
    // nothing is reserved.
    resolve: |m| resolve(&KEYBOARD_SLOTS, m, "keyboard", parse_key, None),
    parse: parse_key,
};

/// Gamepad bindings never include the settings pad button.
static GAMEPAD: LiveTable<Button> = LiveTable {
    state: Mutex::new(None),
    config: || &config::get().input.gamepad,
    resolve: |m| {
        let reserved = Some(crate::settings::pad_button());
        resolve(&GAMEPAD_SLOTS, m, "gamepad", parse_gamepad_button, reserved)
    },
    parse: parse_gamepad_button,
};

/// Snapshot of the resolved keyboard bindings (config + rebinds).
fn keyboard_bindings() -> Arc<Vec<Binding<KeyCode>>> {
    KEYBOARD.snapshot()
}

/// Snapshot of the resolved gamepad bindings, minus the settings pad button.
fn gamepad_bindings() -> Arc<Vec<Binding<Button>>> {
    GAMEPAD.snapshot()
}

/// Effective `[input.keyboard]` overrides (slot name → key name), for the
/// settings screen.
#[allow(dead_code)] // the settings screen's Controls page wires these (R5)
pub fn keyboard_overrides() -> BTreeMap<String, String> {
    KEYBOARD.overrides()
}

/// Same for `[input.gamepad]` (gilrs button names).
#[allow(dead_code)] // R5
pub fn gamepad_overrides() -> BTreeMap<String, String> {
    GAMEPAD.overrides()
}

/// Binds `k` to `slot` (a `KEYBOARD_SLOTS` name), replacing the slot's
/// defaults, and drops every other slot's override that parses to the same
/// key (`Debug` spellings and aliases alike) — one input per slot, matching
/// what `resolve` does to defaults for a `pw64.toml` override. Applies
/// live: the next poll picks the rebuilt bindings up.
#[allow(dead_code)] // R5
pub fn bind_key(slot: &'static str, k: KeyCode) {
    KEYBOARD.bind(slot, k);
}

/// Same for gamepad buttons.
#[allow(dead_code)] // R5
pub fn bind_button(slot: &'static str, b: Button) {
    GAMEPAD.bind(slot, b);
}

/// Drops every keyboard override: the built-in defaults again.
#[allow(dead_code)] // R5
pub fn reset_keyboard() {
    KEYBOARD.edit(HashMap::clear);
}

/// Same for gamepad bindings.
#[allow(dead_code)] // R5
pub fn reset_gamepad() {
    GAMEPAD.edit(HashMap::clear);
}

/// Every winit `KeyCode` variant name in its `Debug` spelling — exactly
/// what `bind_key` stores in the override maps — so a saved binding of any
/// real key parses back after a restart (S2 review: the numpad, `Super*`,
/// `Intl*` and F13+ names used to come back unknown). The round-trip test
/// iterates this table, so a missing entry cannot hide.
const KEYCODE_NAMES: [(&str, KeyCode); 194] = [
    ("Backquote", KeyCode::Backquote),
    ("Backslash", KeyCode::Backslash),
    ("BracketLeft", KeyCode::BracketLeft),
    ("BracketRight", KeyCode::BracketRight),
    ("Comma", KeyCode::Comma),
    ("Digit0", KeyCode::Digit0),
    ("Digit1", KeyCode::Digit1),
    ("Digit2", KeyCode::Digit2),
    ("Digit3", KeyCode::Digit3),
    ("Digit4", KeyCode::Digit4),
    ("Digit5", KeyCode::Digit5),
    ("Digit6", KeyCode::Digit6),
    ("Digit7", KeyCode::Digit7),
    ("Digit8", KeyCode::Digit8),
    ("Digit9", KeyCode::Digit9),
    ("Equal", KeyCode::Equal),
    ("IntlBackslash", KeyCode::IntlBackslash),
    ("IntlRo", KeyCode::IntlRo),
    ("IntlYen", KeyCode::IntlYen),
    ("KeyA", KeyCode::KeyA),
    ("KeyB", KeyCode::KeyB),
    ("KeyC", KeyCode::KeyC),
    ("KeyD", KeyCode::KeyD),
    ("KeyE", KeyCode::KeyE),
    ("KeyF", KeyCode::KeyF),
    ("KeyG", KeyCode::KeyG),
    ("KeyH", KeyCode::KeyH),
    ("KeyI", KeyCode::KeyI),
    ("KeyJ", KeyCode::KeyJ),
    ("KeyK", KeyCode::KeyK),
    ("KeyL", KeyCode::KeyL),
    ("KeyM", KeyCode::KeyM),
    ("KeyN", KeyCode::KeyN),
    ("KeyO", KeyCode::KeyO),
    ("KeyP", KeyCode::KeyP),
    ("KeyQ", KeyCode::KeyQ),
    ("KeyR", KeyCode::KeyR),
    ("KeyS", KeyCode::KeyS),
    ("KeyT", KeyCode::KeyT),
    ("KeyU", KeyCode::KeyU),
    ("KeyV", KeyCode::KeyV),
    ("KeyW", KeyCode::KeyW),
    ("KeyX", KeyCode::KeyX),
    ("KeyY", KeyCode::KeyY),
    ("KeyZ", KeyCode::KeyZ),
    ("Minus", KeyCode::Minus),
    ("Period", KeyCode::Period),
    ("Quote", KeyCode::Quote),
    ("Semicolon", KeyCode::Semicolon),
    ("Slash", KeyCode::Slash),
    ("AltLeft", KeyCode::AltLeft),
    ("AltRight", KeyCode::AltRight),
    ("Backspace", KeyCode::Backspace),
    ("CapsLock", KeyCode::CapsLock),
    ("ContextMenu", KeyCode::ContextMenu),
    ("ControlLeft", KeyCode::ControlLeft),
    ("ControlRight", KeyCode::ControlRight),
    ("Enter", KeyCode::Enter),
    ("SuperLeft", KeyCode::SuperLeft),
    ("SuperRight", KeyCode::SuperRight),
    ("ShiftLeft", KeyCode::ShiftLeft),
    ("ShiftRight", KeyCode::ShiftRight),
    ("Space", KeyCode::Space),
    ("Tab", KeyCode::Tab),
    ("Convert", KeyCode::Convert),
    ("KanaMode", KeyCode::KanaMode),
    ("Lang1", KeyCode::Lang1),
    ("Lang2", KeyCode::Lang2),
    ("Lang3", KeyCode::Lang3),
    ("Lang4", KeyCode::Lang4),
    ("Lang5", KeyCode::Lang5),
    ("NonConvert", KeyCode::NonConvert),
    ("Delete", KeyCode::Delete),
    ("End", KeyCode::End),
    ("Help", KeyCode::Help),
    ("Home", KeyCode::Home),
    ("Insert", KeyCode::Insert),
    ("PageDown", KeyCode::PageDown),
    ("PageUp", KeyCode::PageUp),
    ("ArrowDown", KeyCode::ArrowDown),
    ("ArrowLeft", KeyCode::ArrowLeft),
    ("ArrowRight", KeyCode::ArrowRight),
    ("ArrowUp", KeyCode::ArrowUp),
    ("NumLock", KeyCode::NumLock),
    ("Numpad0", KeyCode::Numpad0),
    ("Numpad1", KeyCode::Numpad1),
    ("Numpad2", KeyCode::Numpad2),
    ("Numpad3", KeyCode::Numpad3),
    ("Numpad4", KeyCode::Numpad4),
    ("Numpad5", KeyCode::Numpad5),
    ("Numpad6", KeyCode::Numpad6),
    ("Numpad7", KeyCode::Numpad7),
    ("Numpad8", KeyCode::Numpad8),
    ("Numpad9", KeyCode::Numpad9),
    ("NumpadAdd", KeyCode::NumpadAdd),
    ("NumpadBackspace", KeyCode::NumpadBackspace),
    ("NumpadClear", KeyCode::NumpadClear),
    ("NumpadClearEntry", KeyCode::NumpadClearEntry),
    ("NumpadComma", KeyCode::NumpadComma),
    ("NumpadDecimal", KeyCode::NumpadDecimal),
    ("NumpadDivide", KeyCode::NumpadDivide),
    ("NumpadEnter", KeyCode::NumpadEnter),
    ("NumpadEqual", KeyCode::NumpadEqual),
    ("NumpadHash", KeyCode::NumpadHash),
    ("NumpadMemoryAdd", KeyCode::NumpadMemoryAdd),
    ("NumpadMemoryClear", KeyCode::NumpadMemoryClear),
    ("NumpadMemoryRecall", KeyCode::NumpadMemoryRecall),
    ("NumpadMemoryStore", KeyCode::NumpadMemoryStore),
    ("NumpadMemorySubtract", KeyCode::NumpadMemorySubtract),
    ("NumpadMultiply", KeyCode::NumpadMultiply),
    ("NumpadParenLeft", KeyCode::NumpadParenLeft),
    ("NumpadParenRight", KeyCode::NumpadParenRight),
    ("NumpadStar", KeyCode::NumpadStar),
    ("NumpadSubtract", KeyCode::NumpadSubtract),
    ("Escape", KeyCode::Escape),
    ("Fn", KeyCode::Fn),
    ("FnLock", KeyCode::FnLock),
    ("PrintScreen", KeyCode::PrintScreen),
    ("ScrollLock", KeyCode::ScrollLock),
    ("Pause", KeyCode::Pause),
    ("BrowserBack", KeyCode::BrowserBack),
    ("BrowserFavorites", KeyCode::BrowserFavorites),
    ("BrowserForward", KeyCode::BrowserForward),
    ("BrowserHome", KeyCode::BrowserHome),
    ("BrowserRefresh", KeyCode::BrowserRefresh),
    ("BrowserSearch", KeyCode::BrowserSearch),
    ("BrowserStop", KeyCode::BrowserStop),
    ("Eject", KeyCode::Eject),
    ("LaunchApp1", KeyCode::LaunchApp1),
    ("LaunchApp2", KeyCode::LaunchApp2),
    ("LaunchMail", KeyCode::LaunchMail),
    ("MediaPlayPause", KeyCode::MediaPlayPause),
    ("MediaSelect", KeyCode::MediaSelect),
    ("MediaStop", KeyCode::MediaStop),
    ("MediaTrackNext", KeyCode::MediaTrackNext),
    ("MediaTrackPrevious", KeyCode::MediaTrackPrevious),
    ("Power", KeyCode::Power),
    ("Sleep", KeyCode::Sleep),
    ("AudioVolumeDown", KeyCode::AudioVolumeDown),
    ("AudioVolumeMute", KeyCode::AudioVolumeMute),
    ("AudioVolumeUp", KeyCode::AudioVolumeUp),
    ("WakeUp", KeyCode::WakeUp),
    ("Meta", KeyCode::Meta),
    ("Hyper", KeyCode::Hyper),
    ("Turbo", KeyCode::Turbo),
    ("Abort", KeyCode::Abort),
    ("Resume", KeyCode::Resume),
    ("Suspend", KeyCode::Suspend),
    ("Again", KeyCode::Again),
    ("Copy", KeyCode::Copy),
    ("Cut", KeyCode::Cut),
    ("Find", KeyCode::Find),
    ("Open", KeyCode::Open),
    ("Paste", KeyCode::Paste),
    ("Props", KeyCode::Props),
    ("Select", KeyCode::Select),
    ("Undo", KeyCode::Undo),
    ("Hiragana", KeyCode::Hiragana),
    ("Katakana", KeyCode::Katakana),
    ("F1", KeyCode::F1),
    ("F2", KeyCode::F2),
    ("F3", KeyCode::F3),
    ("F4", KeyCode::F4),
    ("F5", KeyCode::F5),
    ("F6", KeyCode::F6),
    ("F7", KeyCode::F7),
    ("F8", KeyCode::F8),
    ("F9", KeyCode::F9),
    ("F10", KeyCode::F10),
    ("F11", KeyCode::F11),
    ("F12", KeyCode::F12),
    ("F13", KeyCode::F13),
    ("F14", KeyCode::F14),
    ("F15", KeyCode::F15),
    ("F16", KeyCode::F16),
    ("F17", KeyCode::F17),
    ("F18", KeyCode::F18),
    ("F19", KeyCode::F19),
    ("F20", KeyCode::F20),
    ("F21", KeyCode::F21),
    ("F22", KeyCode::F22),
    ("F23", KeyCode::F23),
    ("F24", KeyCode::F24),
    ("F25", KeyCode::F25),
    ("F26", KeyCode::F26),
    ("F27", KeyCode::F27),
    ("F28", KeyCode::F28),
    ("F29", KeyCode::F29),
    ("F30", KeyCode::F30),
    ("F31", KeyCode::F31),
    ("F32", KeyCode::F32),
    ("F33", KeyCode::F33),
    ("F34", KeyCode::F34),
    ("F35", KeyCode::F35),
];

/// Key name → `KeyCode` (`pw64.toml` `[input.keyboard]` values). Accepts
/// every winit `KeyCode` variant name (`Space`, `ShiftLeft`, `KeyW`,
/// `ArrowUp`, `NumpadDivide`, `F13`, …) plus short forms (`W`, `1`,
/// `LShift`, `Esc`). Unknown names return `None`, which leaves that slot on
/// its defaults.
pub(crate) fn parse_key(name: &str) -> Option<KeyCode> {
    use KeyCode::*;
    let upper = name.trim().to_ascii_uppercase();
    // Friendly aliases first; several differ from winit's spelling
    // (`LShift` = ShiftLeft, `Esc` = Escape, `Return` = Enter).
    let alias = match upper.as_str() {
        "SPACE" => Some(Space),
        // `NumpadEnter` is its own key now (S2): aliasing it to `Enter`
        // broke the round trip of a saved numpad binding.
        "ENTER" | "RETURN" => Some(Enter),
        "TAB" => Some(Tab),
        "BACKSPACE" => Some(Backspace),
        "ESCAPE" | "ESC" => Some(Escape),
        "UP" | "ARROWUP" => Some(ArrowUp),
        "DOWN" | "ARROWDOWN" => Some(ArrowDown),
        "LEFT" | "ARROWLEFT" => Some(ArrowLeft),
        "RIGHT" | "ARROWRIGHT" => Some(ArrowRight),
        "SHIFTLEFT" | "LSHIFT" => Some(ShiftLeft),
        "SHIFTRIGHT" | "RSHIFT" => Some(ShiftRight),
        "CONTROLLEFT" | "LCTRL" => Some(ControlLeft),
        "CONTROLRIGHT" | "RCTRL" => Some(ControlRight),
        "ALTLEFT" | "LALT" => Some(AltLeft),
        "ALTRIGHT" | "RALT" => Some(AltRight),
        // The UI-events names for the OS keys (winit calls them Super*).
        "METALEFT" => Some(SuperLeft),
        "METARIGHT" => Some(SuperRight),
        "CAPSLOCK" => Some(CapsLock),
        "INSERT" => Some(Insert),
        "DELETE" => Some(Delete),
        "HOME" => Some(Home),
        "END" => Some(End),
        "PAGEUP" => Some(PageUp),
        "PAGEDOWN" => Some(PageDown),
        "BACKQUOTE" | "BACKTICK" => Some(Backquote),
        "MINUS" => Some(Minus),
        "EQUAL" => Some(Equal),
        "BRACKETLEFT" => Some(BracketLeft),
        "BRACKETRIGHT" => Some(BracketRight),
        "SEMICOLON" => Some(Semicolon),
        "QUOTE" => Some(Quote),
        "COMMA" => Some(Comma),
        "PERIOD" => Some(Period),
        "SLASH" => Some(Slash),
        "BACKSLASH" => Some(Backslash),
        _ => None,
    };
    alias
        .or_else(|| {
            // Full winit spellings spell out the same table as the short
            // form: `KeyW` → "W", `Digit1` → "1".
            upper
                .strip_prefix("KEY")
                .or_else(|| upper.strip_prefix("DIGIT"))
                .and_then(parse_key)
        })
        .or_else(|| {
            // Any remaining KeyCode variant name (`NumpadDivide`, `F13`,
            // `SuperLeft`, …) resolves through the table.
            KEYCODE_NAMES
                .iter()
                .find(|(n, _)| upper.eq_ignore_ascii_case(n))
                .map(|(_, k)| *k)
        })
        .or_else(|| {
            // Short form: one letter → `KeyX`, one digit → `DigitX`.
            let mut c = upper.chars();
            let (Some(ch), None) = (c.next(), c.next()) else {
                return None;
            };
            let key = match ch {
                'A' => KeyA,
                'B' => KeyB,
                'C' => KeyC,
                'D' => KeyD,
                'E' => KeyE,
                'F' => KeyF,
                'G' => KeyG,
                'H' => KeyH,
                'I' => KeyI,
                'J' => KeyJ,
                'K' => KeyK,
                'L' => KeyL,
                'M' => KeyM,
                'N' => KeyN,
                'O' => KeyO,
                'P' => KeyP,
                'Q' => KeyQ,
                'R' => KeyR,
                'S' => KeyS,
                'T' => KeyT,
                'U' => KeyU,
                'V' => KeyV,
                'W' => KeyW,
                'X' => KeyX,
                'Y' => KeyY,
                'Z' => KeyZ,
                '0' => Digit0,
                '1' => Digit1,
                '2' => Digit2,
                '3' => Digit3,
                '4' => Digit4,
                '5' => Digit5,
                '6' => Digit6,
                '7' => Digit7,
                '8' => Digit8,
                '9' => Digit9,
                _ => return None,
            };
            Some(key)
        })
}

/// gilrs button name → `Button` (`pw64.toml` `[input.gamepad]` values).
/// Accepts gilrs's names plus the common abbreviations (`LB`, `ZR`, …).
pub(crate) fn parse_gamepad_button(name: &str) -> Option<Button> {
    use Button::*;
    Some(match name.trim().to_ascii_uppercase().as_str() {
        "SOUTH" | "A" => South,
        "EAST" | "B" => East,
        "WEST" | "X" => West,
        "NORTH" | "Y" => North,
        // Extra action-pad buttons some pads report (gilrs `Button::C`/`Z`;
        // capture can save them, S2 review).
        "C" => C,
        "Z" => Z,
        "LEFTTRIGGER" | "LT" | "LB" => LeftTrigger,
        "LEFTTRIGGER2" | "ZL" => LeftTrigger2,
        "RIGHTTRIGGER" | "RT" | "RB" => RightTrigger,
        "RIGHTTRIGGER2" | "ZR" => RightTrigger2,
        "START" | "MENU" => Start,
        "SELECT" | "BACK" | "VIEW" => Select,
        "MODE" | "GUIDE" | "HOME" => Mode,
        "LEFTTHUMB" | "LS" => LeftThumb,
        "RIGHTTHUMB" | "RS" => RightThumb,
        "DPADUP" | "DPAD_UP" => DPadUp,
        "DPADDOWN" | "DPAD_DOWN" => DPadDown,
        "DPADLEFT" | "DPAD_LEFT" => DPadLeft,
        "DPADRIGHT" | "DPAD_RIGHT" => DPadRight,
        _ => return None,
    })
}

/// Gamepad button → N64 bits (positional defaults, so Nintendo layouts
/// match; `[input.gamepad]` in `pw64.toml` overrides slots).
#[cfg(test)]
fn gamepad_button(b: Button) -> u16 {
    bits_for(&gamepad_bindings(), b) as u16
}

/// One gilrs pad → N64 pad. Scans the resolved bindings (config + defaults)
/// instead of a fixed button list, so remapped slots are picked up. One read
/// lock for the whole scan: this runs every 4 ms per connected pad.
fn gamepad_pad(g: &gilrs::Gamepad) -> Pad {
    let bindings = gamepad_bindings();
    let mut button = 0;
    for binding in bindings.iter() {
        let down = binding.inputs.iter().any(|&b| {
            if matches!(b, Button::LeftTrigger2 | Button::RightTrigger2) {
                g.button_data(b)
                    .is_some_and(|d| d.value() >= TRIGGER_THRESHOLD)
            } else {
                g.is_pressed(b)
            }
        });
        if down {
            // All gamepad slots are CONT bits (the stick is not rebindable).
            button |= binding.bit as u16;
        }
    }
    // Some raw controllers report the d-pad as a hat axis pair.
    button |= hat_buttons(g.value(Axis::DPadX), g.value(Axis::DPadY));
    button |= c_stick_buttons(g.value(Axis::RightStickX), g.value(Axis::RightStickY));
    let (stick_x, stick_y) = stick_to_n64(g.value(Axis::LeftStickX), g.value(Axis::LeftStickY));
    Pad {
        button,
        stick_x,
        stick_y,
    }
}

/// One pad's settings-overlay navigation state (W3), from fixed physical
/// buttons so a rebinding can never lock the player out of the menu: South
/// confirms (`CONT_A`), East goes back (`CONT_B`), Start closes
/// (`CONT_START`), the d-pad (buttons or hat axes) and the left stick move.
/// Positional like the default bindings (input.md): a Nintendo pad's bottom
/// button confirms.
fn gamepad_nav_pad(g: &gilrs::Gamepad) -> Pad {
    let mut button = 0;
    for (b, bit) in [
        (Button::South, CONT_A),
        (Button::East, CONT_B),
        (Button::Start, CONT_START),
        (Button::DPadUp, CONT_UP),
        (Button::DPadDown, CONT_DOWN),
        (Button::DPadLeft, CONT_LEFT),
        (Button::DPadRight, CONT_RIGHT),
    ] {
        if g.is_pressed(b) {
            button |= bit;
        }
    }
    button |= hat_buttons(g.value(Axis::DPadX), g.value(Axis::DPadY));
    let (stick_x, stick_y) = stick_to_n64(g.value(Axis::LeftStickX), g.value(Axis::LeftStickY));
    Pad {
        button,
        stick_x,
        stick_y,
    }
}

/// D-pad reported as a hat axis pair (some raw controllers) → CONT bits.
fn hat_buttons(dx: f32, dy: f32) -> u16 {
    let mut button = 0;
    if dx >= 0.5 {
        button |= CONT_RIGHT;
    }
    if dx <= -0.5 {
        button |= CONT_LEFT;
    }
    if dy >= 0.5 {
        button |= CONT_UP;
    }
    if dy <= -0.5 {
        button |= CONT_DOWN;
    }
    button
}

/// Keyboard key → (buttons, stick direction x/y in -1..1).
/// Both come from the resolved bindings (`keyboard_bindings`): the 14 button
/// slots' CONT bits and the `STICK_*` slots' bits at `1 << 16 .. 1 << 19`,
/// all in one `resolve` pass — so a key rebound to a button is off the stick
/// (and vice versa) instead of doing both.
pub fn key_mapping(k: KeyCode) -> Option<(u16, i8, i8)> {
    let bits = bits_for(&keyboard_bindings(), k);
    let buttons = (bits & u32::from(u16::MAX)) as u16;
    let mut stick = (0i8, 0i8);
    if bits & STICK_UP_BIT != 0 {
        stick.1 += 1;
    }
    if bits & STICK_DOWN_BIT != 0 {
        stick.1 -= 1;
    }
    if bits & STICK_LEFT_BIT != 0 {
        stick.0 -= 1;
    }
    if bits & STICK_RIGHT_BIT != 0 {
        stick.0 += 1;
    }
    if buttons == 0 && stick == (0, 0) {
        return None;
    }
    Some((buttons, stick.0, stick.1))
}

/// Held keyboard keys → N64 pad (digital stick at full deflection,
/// diagonals at the N64 gate corner ~70,70 via `stick_to_n64`).
pub fn keyboard_pad(held: &[KeyCode]) -> Pad {
    let (mut button, mut x, mut y) = (0u16, 0i32, 0i32);
    for &k in held {
        if let Some((b, dx, dy)) = key_mapping(k) {
            button |= b;
            x += dx as i32;
            y += dy as i32;
        }
    }
    let (fx, fy) = (x.signum() as f32, y.signum() as f32);
    let n = (fx * fx + fy * fy).sqrt().max(1.0);
    let (stick_x, stick_y) = stick_to_n64(fx / n, fy / n);
    Pad {
        button,
        stick_x,
        stick_y,
    }
}

/// Current sources; every update re-merges and publishes.
struct Sources {
    gamepads: Pad,
    held_keys: Vec<KeyCode>,
    /// `PW64_INPUT_SCRIPT` state for the current retrace.
    script: Pad,
    /// Virtual pads from BLE (`ble.rs`); updated from the poll loop.
    ble: Pad,
    /// Buttons still held when the settings overlay closed (e.g. the A or
    /// Start that closed it): masked from the game until released, so the
    /// closing press doesn't leak into it as a fresh press.
    suppressed: u16,
    /// The gamepads' settings-overlay navigation state (W3: fixed physical
    /// buttons, `gamepad_nav_pad`), independent of the bindings.
    nav: Pad,
}

impl Sources {
    fn merged(&self) -> Pad {
        // W7: unfocused, real pads are ignored; a script still drives (a
        // scripted window run may sit in the background).
        if !FOCUSED.load(Ordering::Relaxed) {
            return keyboard_pad(&self.held_keys).merge(self.script);
        }
        self.gamepads
            .merge(self.ble)
            .merge(keyboard_pad(&self.held_keys))
            .merge(self.script)
    }

    /// The overlay's navigation pad: the physical gamepad state plus the
    /// BLE pads (fixed, label-based mapping; not rebindable). The keyboard
    /// reaches the overlay through the window's key events instead.
    fn nav(&self) -> Pad {
        if !FOCUSED.load(Ordering::Relaxed) {
            return IDLE;
        }
        self.nav.merge(self.ble)
    }
}

/// W7: does the game window have the focus? Set by the window; pads drive
/// neither the game nor the settings trigger while it is false. True by
/// default (headless runs have no window to focus).
static FOCUSED: AtomicBool = AtomicBool::new(true);

/// The window gained or lost the focus (W7): re-publish so the game sees an
/// idle pad (or the live one again) right away.
pub fn set_focused(focused: bool) {
    if FOCUSED.swap(focused, Ordering::Relaxed) != focused {
        publish(&mut SOURCES.lock().unwrap());
    }
}

const IDLE: Pad = Pad {
    button: 0,
    stick_x: 0,
    stick_y: 0,
};

static SOURCES: Mutex<Sources> = Mutex::new(Sources {
    gamepads: IDLE,
    held_keys: Vec::new(),
    script: IDLE,
    ble: IDLE,
    suppressed: 0,
    nav: IDLE,
});

/// Pad state for menu navigation: the settings overlay (`settings.rs`) and
/// the first-run setup screen poll it on the window thread. While the
/// overlay is open the game sees an idle pad instead.
static OVERLAY_PAD: Mutex<Pad> = Mutex::new(IDLE);

fn publish(s: &mut Sources) {
    let p = s.merged();
    // Always kept current: the first-run setup screen (firstrun.rs, U13)
    // polls it too, before the game or the overlay exist.
    *OVERLAY_PAD.lock().unwrap() = s.nav();
    if crate::settings::is_open() {
        // The overlay owns input: the game sees an idle pad while paused.
        pw64_platform::headless::set_controller1(0, 0, 0);
        if let Some(r) = RECORD.lock().unwrap().as_mut() {
            r.pad = IDLE;
        }
    } else {
        s.suppressed &= p.button; // released buttons reach the game again
        let button = p.button & !s.suppressed;
        pw64_platform::headless::set_controller1(button, p.stick_x, p.stick_y);
        if let Some(r) = RECORD.lock().unwrap().as_mut() {
            r.pad = Pad { button, ..p };
        }
    }
}

/// `PW64_RECORD_INPUT=<file>`: the pad the game sees, written as an input
/// script (one step per change, keyed to the retrace like `PW64_INPUT_SCRIPT`)
/// so a playtest can be replayed headless.
struct Recorder {
    out: std::io::BufWriter<std::fs::File>,
    pad: Pad,
    written: Pad,
}

static RECORD: Mutex<Option<Recorder>> = Mutex::new(None);

/// Script-format line for `pad` at `retrace` (inverse of `parse_script`).
fn script_line(retrace: u64, pad: Pad) -> String {
    const NAMES: [(u16, &str); 14] = [
        (CONT_A, "A"),
        (CONT_B, "B"),
        (CONT_Z, "Z"),
        (CONT_START, "START"),
        (CONT_L, "L"),
        (CONT_R, "R"),
        (CONT_C_UP, "CU"),
        (CONT_C_DOWN, "CD"),
        (CONT_C_LEFT, "CL"),
        (CONT_C_RIGHT, "CR"),
        (CONT_UP, "DU"),
        (CONT_DOWN, "DD"),
        (CONT_LEFT, "DL"),
        (CONT_RIGHT, "DR"),
    ];
    let mut line = retrace.to_string();
    for (bit, name) in NAMES {
        if pad.button & bit != 0 {
            line.push(' ');
            line.push_str(name);
        }
    }
    if pad.stick_x != 0 || pad.stick_y != 0 {
        line.push_str(&format!(" stick {},{}", pad.stick_x, pad.stick_y));
    }
    line
}

/// Last merged pad for the overlay's navigation (window thread).
pub fn overlay_pad() -> Pad {
    *OVERLAY_PAD.lock().unwrap()
}

/// Window lost focus: release every held key (no stuck buttons).
pub fn release_all_keys() {
    let mut s = SOURCES.lock().unwrap();
    if !s.held_keys.is_empty() {
        s.held_keys.clear();
        publish(&mut s);
    }
}

/// The settings overlay opened or closed (`settings.rs`, after flipping
/// `settings::OPEN`): re-route the pad — into the overlay's nav state or
/// back to the game. The input thread only publishes on change, so without
/// this the overlay would start from a stale pad and the game would resume
/// with whatever it had before the overlay opened.
pub fn overlay_toggled(open: bool) {
    let mut s = SOURCES.lock().unwrap();
    if !open {
        s.suppressed = s.merged().button;
    }
    publish(&mut s);
}

/// One `PW64_INPUT_SCRIPT` step: from `retrace` on, hold `pad`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScriptStep {
    pub retrace: u64,
    pub pad: Pad,
}

/// Parses an input script. One step per line, `#` comments:
/// `<retrace> [buttons...] [stick X,Y]`. `<retrace>` is absolute or `+N`
/// relative to the previous step; the pad is held until the next step.
/// Buttons: A B Z START L R CU CD CL CR DU DD DL DR (case-insensitive).
/// Stick values are raw N64 (-80..80, y up).
pub fn parse_script(text: &str) -> Result<Vec<ScriptStep>, String> {
    let mut steps = Vec::new();
    let mut last = 0u64;
    for (n, line) in text.lines().enumerate() {
        let err = |m: String| format!("line {}: {m}", n + 1);
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let mut toks = line.split_whitespace();
        let t = toks.next().unwrap_or_default();
        let retrace = match t.strip_prefix('+') {
            Some(rel) => rel.parse::<u64>().map(|d| last + d),
            None => t.parse::<u64>(),
        }
        .map_err(|_| err(format!("bad retrace `{t}`")))?;
        if retrace < last {
            return Err(err(format!("retrace {retrace} goes backwards")));
        }
        let mut pad = IDLE;
        while let Some(tok) = toks.next() {
            let b = match tok.to_ascii_uppercase().as_str() {
                "A" => CONT_A,
                "B" => CONT_B,
                "Z" => CONT_Z,
                "START" => CONT_START,
                "L" => CONT_L,
                "R" => CONT_R,
                "CU" => CONT_C_UP,
                "CD" => CONT_C_DOWN,
                "CL" => CONT_C_LEFT,
                "CR" => CONT_C_RIGHT,
                "DU" => CONT_UP,
                "DD" => CONT_DOWN,
                "DL" => CONT_LEFT,
                "DR" => CONT_RIGHT,
                "STICK" => {
                    let v = toks.next().ok_or_else(|| err("stick needs X,Y".into()))?;
                    let (x, y) = v
                        .split_once(',')
                        .and_then(|(x, y)| Some((x.parse::<i8>().ok()?, y.parse::<i8>().ok()?)))
                        .ok_or_else(|| err(format!("bad stick `{v}`")))?;
                    pad.stick_x = x;
                    pad.stick_y = y;
                    0
                }
                other => return Err(err(format!("unknown token `{other}`"))),
            };
            pad.button |= b;
        }
        steps.push(ScriptStep { retrace, pad });
        last = retrace;
    }
    Ok(steps)
}

/// Loaded script + index of the next step to apply.
static SCRIPT: Mutex<(Vec<ScriptStep>, usize)> = Mutex::new((Vec::new(), 0));

/// Loads `PW64_INPUT_SCRIPT` if set (exits on a bad script: it's a test tool)
/// and opens `PW64_RECORD_INPUT`.
pub fn load_script_from_env() {
    if let Some(path) = std::env::var_os("PW64_RECORD_INPUT") {
        match std::fs::File::create(&path) {
            Ok(f) => {
                eprintln!("[record] input → {}", std::path::Path::new(&path).display());
                *RECORD.lock().unwrap() = Some(Recorder {
                    out: std::io::BufWriter::new(f),
                    pad: IDLE,
                    written: IDLE,
                });
            }
            Err(e) => eprintln!("[record] can't create {}: {e}", path.display()),
        }
    }
    let Some(path) = std::env::var_os("PW64_INPUT_SCRIPT") else {
        return;
    };
    let steps = std::fs::read_to_string(&path)
        .map_err(|e| e.to_string())
        .and_then(|t| parse_script(&t));
    match steps {
        Ok(steps) => {
            eprintln!(
                "[script] {}: {} steps",
                std::path::Path::new(&path).display(),
                steps.len()
            );
            *SCRIPT.lock().unwrap() = (steps, 0);
        }
        Err(e) => {
            eprintln!("error: PW64_INPUT_SCRIPT {}: {e}", path.display());
            std::process::exit(2);
        }
    }
}

/// Called every VI retrace (game thread): applies the steps now due.
pub fn script_tick(retrace: u64) {
    apply_script_steps(retrace);
    // After the script: a recorded scripted run replays at the same retraces.
    if let Some(r) = RECORD.lock().unwrap().as_mut()
        && r.pad != r.written
    {
        use std::io::Write;
        // Flushed per step: the game may exit without unwinding.
        let _ = writeln!(r.out, "{}", script_line(retrace, r.pad)).and_then(|_| r.out.flush());
        r.written = r.pad;
    }
}

fn apply_script_steps(retrace: u64) {
    let mut sc = SCRIPT.lock().unwrap();
    let (steps, next) = &mut *sc;
    let mut changed = None;
    while *next < steps.len() && steps[*next].retrace <= retrace {
        changed = Some(steps[*next].pad);
        *next += 1;
    }
    drop(sc);
    if let Some(pad) = changed {
        eprintln!(
            "[script] retrace {retrace}: buttons {:#06x} stick {},{}",
            pad.button, pad.stick_x, pad.stick_y
        );
        let mut s = SOURCES.lock().unwrap();
        s.script = pad;
        publish(&mut s);
    }
}

/// Feed a winit key event (from the game window's event loop).
pub fn on_key(code: KeyCode, pressed: bool) {
    if key_mapping(code).is_none() {
        return;
    }
    let mut s = SOURCES.lock().unwrap();
    s.held_keys.retain(|&k| k != code);
    if pressed {
        s.held_keys.push(code);
    }
    publish(&mut s);
}

/// Starts the gamepad polling thread. Without gilrs support the pad stays
/// keyboard-only / idle. Hot-plug: pads are re-enumerated every poll.
pub fn spawn() {
    let r = std::thread::Builder::new()
        .name("pw64-input".into())
        .spawn(move || {
            let mut gilrs = match Gilrs::new() {
                Ok(g) => g,
                Err(gilrs::Error::NotImplemented(g)) => {
                    eprintln!("[input] gamepads not supported on this platform");
                    g
                }
                Err(e) => {
                    eprintln!("[input] gamepad init failed: {e}; keyboard only");
                    return;
                }
            };
            eprintln!(
                "[input] gilrs ready: {} gamepad(s)",
                gilrs.gamepads().count()
            );
            for (_, g) in gilrs.gamepads() {
                eprintln!("[input] gamepad: {} ({:?})", g.name(), g.mapping_source());
            }
            // Pads present at startup fire Connected only on some backends:
            // seed the active pad from the first connected one, so
            // `active_pad` can name the controller right away.
            if let Some((id, g)) = gilrs.gamepads().find(|(_, g)| g.is_connected()) {
                set_active_pad(id, &g);
            }
            // U7: gilrs sends `Connected` for pads already present at init
            // on some backends. Toasts start after this grace period.
            let started = std::time::Instant::now();
            // The settings trigger (`settings.rs`): a button the game never
            // reads (`Select` by default — unmapped in `GAMEPAD_SLOTS`), so
            // only the overlay consumes it. Edge-detected here; the toggle
            // happens on the window thread (via the event loop proxy).
            let settings_btn = crate::settings::pad_button();
            let mut settings_was_down = false;
            // U26: when the last pad left (+ PAUSE_GRACE); cleared when one
            // reconnects. The window decides whether to pause, on its own
            // thread (`window::request_pause`).
            let mut pause_at: Option<std::time::Instant> = None;
            loop {
                while let Some(ev) = gilrs.next_event() {
                    match ev.event {
                        gilrs::EventType::Connected => {
                            let g = gilrs.gamepad(ev.id);
                            eprintln!("[input] connected: {} ({:?})", g.name(), g.mapping_source());
                            set_active_pad(ev.id, &g);
                            // U26: a pad back within the grace: no pause.
                            pause_at = None;
                            // U7: a pad plugged in after the start says so
                            // (the welcome card names the ones at startup).
                            if started.elapsed() > std::time::Duration::from_secs(2) {
                                crate::window::notify_toast(format!(
                                    "Controller connected: {}",
                                    g.name()
                                ));
                            }
                        }
                        gilrs::EventType::Disconnected => {
                            eprintln!("[input] disconnected: gamepad {}", ev.id);
                            active_pad_disconnected(&gilrs, ev.id);
                            // U7: even the pad unplugged in the first 2 s
                            // was never announced, so it gets no goodbye.
                            if started.elapsed() > std::time::Duration::from_secs(2) {
                                crate::window::notify_toast("Controller disconnected".to_string());
                                // U26: that was the last pad: arm the pause
                                // (same 2 s startup grace as the toast:
                                // init-time phantom events must not pause).
                                // It fires below after PAUSE_GRACE unless a
                                // pad comes back (a Bluetooth hiccup).
                                if !gilrs.gamepads().any(|(_, g)| g.is_connected()) {
                                    pause_at = Some(std::time::Instant::now() + PAUSE_GRACE);
                                }
                            }
                        }
                        // Pad capture (the settings screen): the press never
                        // reaches the game — it is stored for the overlay and
                        // capture disarms itself.
                        gilrs::EventType::ButtonPressed(b, _)
                            if PAD_CAPTURE.load(Ordering::Relaxed)
                                && !matches!(b, Button::Unknown | Button::Mode)
                                && b != settings_btn =>
                        {
                            *CAPTURED_BUTTON.lock().unwrap() = Some(b);
                            PAD_CAPTURE.store(false, Ordering::Relaxed);
                        }
                        // Any other press: that pad becomes the active one
                        // (last used wins; `active_pad` names its buttons).
                        gilrs::EventType::ButtonPressed(_, _) => {
                            active_pad_pressed(&gilrs, ev.id);
                        }
                        _ => {}
                    }
                }
                let pads: Vec<_> = gilrs.gamepads().filter(|(_, g)| g.is_connected()).collect();
                let pad = pads
                    .iter()
                    .map(|(_, g)| gamepad_pad(g))
                    .fold(Pad::default(), Pad::merge);
                let nav = pads
                    .iter()
                    .map(|(_, g)| gamepad_nav_pad(g))
                    .fold(Pad::default(), Pad::merge);
                // W7: no settings trigger while another window is focused.
                let settings_down = FOCUSED.load(Ordering::Relaxed)
                    && (pads.iter().any(|(_, g)| g.is_pressed(settings_btn))
                        || crate::ble::settings_pressed());
                if settings_down && !settings_was_down {
                    crate::window::notify_settings();
                }
                settings_was_down = settings_down;
                // BLE pads live in their own threads (`ble.rs`); pick up the
                // latest virtual pad here so everything merges into one
                // published snapshot.
                let ble = crate::ble::pad();
                if pause_at.is_some_and(|t| std::time::Instant::now() >= t) {
                    pause_at = None;
                    if pads.is_empty() && !crate::ble::any_connected() {
                        crate::window::request_pause();
                    }
                }
                let mut s = SOURCES.lock().unwrap();
                if (s.gamepads, s.ble, s.nav) != (pad, ble, nav) {
                    (s.gamepads, s.ble, s.nav) = (pad, ble, nav);
                    publish(&mut s);
                }
                drop(s);
                std::thread::sleep(POLL_INTERVAL);
            }
        });
    if let Err(e) = r {
        eprintln!("[input] could not start input thread: {e}");
    }
}

/// Effective keyboard bindings per slot (winit names, as `pw64.toml`
/// accepts them) for the settings screen's read-only list.
pub fn keyboard_binding_rows() -> Vec<(&'static str, String)> {
    binding_rows(&keyboard_bindings())
}

/// Same for gamepad bindings (gilrs names).
pub fn gamepad_binding_rows() -> Vec<(&'static str, String)> {
    binding_rows(&gamepad_bindings())
}

/// Pad family of the active controller: which names the settings screen
/// shows for its buttons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PadFamily {
    Xbox,
    PlayStation,
    Nintendo,
    Generic,
}

/// Classifies a pad by USB vendor id when the backend reports one (gilrs's
/// `vendor_id()` is `None` on some backends, so the name carries it), else
/// by name (case-insensitive). Anything unrecognised is `Generic` (D5:
/// generic pads are shown Xbox-style names).
pub fn pad_family(vendor: Option<u16>, name: &str) -> PadFamily {
    match vendor {
        Some(0x045E) => return PadFamily::Xbox,
        Some(0x054C) => return PadFamily::PlayStation,
        Some(0x057E) => return PadFamily::Nintendo,
        _ => {}
    }
    let n = name.to_ascii_lowercase();
    let has = |keys: &[&str]| keys.iter().any(|k| n.contains(k));
    if has(&["xbox", "xinput"]) {
        PadFamily::Xbox
    } else if has(&[
        "dualsense",
        "dualshock",
        "playstation",
        "ps4",
        "ps5",
        "wireless controller",
    ]) {
        PadFamily::PlayStation
    } else if has(&["pro controller", "joy-con", "switch"]) {
        PadFamily::Nintendo
    } else {
        PadFamily::Generic
    }
}

/// The controller the player last used: its pad name (as gilrs reports it)
/// and family. BLE pads (`ble.rs`) are Nintendo-family but stay out of this
/// for 1.0: they have no gilrs events to hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PadInfo {
    pub name: String,
    pub family: PadFamily,
}

static ACTIVE_PAD: Mutex<Option<PadInfo>> = Mutex::new(None);
/// The gilrs id behind `ACTIVE_PAD`, so a disconnect of *that* pad can be
/// told apart from another pad's.
static ACTIVE_PAD_ID: Mutex<Option<gilrs::GamepadId>> = Mutex::new(None);

/// The active pad, if one has been connected / used.
pub fn active_pad() -> Option<PadInfo> {
    ACTIVE_PAD.lock().unwrap().clone()
}

/// (Re)sets the active pad from a gilrs handle.
fn set_active_pad(id: gilrs::GamepadId, g: &gilrs::Gamepad) {
    *ACTIVE_PAD.lock().unwrap() = Some(PadInfo {
        name: g.name().to_string(),
        family: pad_family(g.vendor_id(), g.name()),
    });
    *ACTIVE_PAD_ID.lock().unwrap() = Some(id);
}

/// A button press from a pad: it becomes the active one (last used wins).
fn active_pad_pressed(gilrs: &Gilrs, id: gilrs::GamepadId) {
    if *ACTIVE_PAD_ID.lock().unwrap() != Some(id) {
        set_active_pad(id, &gilrs.gamepad(id));
    }
}

/// A pad disconnected: when it was the active one, fall back to any pad
/// still connected (a second pad's unplug must not lose the button names).
fn active_pad_disconnected(gilrs: &Gilrs, id: gilrs::GamepadId) {
    if *ACTIVE_PAD_ID.lock().unwrap() == Some(id) {
        *ACTIVE_PAD.lock().unwrap() = None;
        *ACTIVE_PAD_ID.lock().unwrap() = None;
        if let Some((other, g)) = gilrs.gamepads().find(|(_, g)| g.is_connected()) {
            set_active_pad(other, &g);
        }
    }
}

/// gilrs `Button` → the name shown on the settings screen, per pad family.
/// A `Generic` pad gets the Xbox names (D5: Steam Input and most PC pads
/// present as Xbox); Nintendo names are positional, so its South is labelled
/// "B". The D-pad names are the same for every family.
pub fn button_name(b: Button, f: PadFamily) -> &'static str {
    use PadFamily::{Generic as Any, Nintendo as Nin, PlayStation as Ps, Xbox as Xbx};
    match (b, f) {
        (Button::DPadUp, _) => "D-pad up",
        (Button::DPadDown, _) => "D-pad down",
        (Button::DPadLeft, _) => "D-pad left",
        (Button::DPadRight, _) => "D-pad right",
        (Button::South, Xbx | Any) => "A",
        (Button::East, Xbx | Any) => "B",
        (Button::West, Xbx | Any) => "X",
        (Button::North, Xbx | Any) => "Y",
        (Button::LeftTrigger, Xbx | Any) => "LB",
        (Button::LeftTrigger2, Xbx | Any) => "LT",
        (Button::RightTrigger, Xbx | Any) => "RB",
        (Button::RightTrigger2, Xbx | Any) => "RT",
        (Button::Select, Xbx | Any) => "View",
        (Button::Start, Xbx | Any) => "Menu",
        (Button::LeftThumb, Xbx | Any) => "LS",
        (Button::RightThumb, Xbx | Any) => "RS",
        (Button::South, Ps) => "Cross",
        (Button::East, Ps) => "Circle",
        (Button::West, Ps) => "Square",
        (Button::North, Ps) => "Triangle",
        (Button::LeftTrigger, Ps) => "L1",
        (Button::LeftTrigger2, Ps) => "L2",
        (Button::RightTrigger, Ps) => "R1",
        (Button::RightTrigger2, Ps) => "R2",
        (Button::Select, Ps) => "Create",
        (Button::Start, Ps) => "Options",
        (Button::LeftThumb, Ps) => "L3",
        (Button::RightThumb, Ps) => "R3",
        (Button::South, Nin) => "B",
        (Button::East, Nin) => "A",
        (Button::West, Nin) => "Y",
        (Button::North, Nin) => "X",
        (Button::LeftTrigger, Nin) => "L",
        (Button::LeftTrigger2, Nin) => "ZL",
        (Button::RightTrigger, Nin) => "R",
        (Button::RightTrigger2, Nin) => "ZR",
        (Button::Select, Nin) => "\u{2212}",
        (Button::Start, Nin) => "+",
        (Button::LeftThumb, Nin) => "L stick press",
        (Button::RightThumb, Nin) => "R stick press",
        (Button::Mode, _) => "Guide",
        (Button::Unknown, _) => "Unknown",
        // SNES-style extras gilrs names but no family claims.
        (Button::C, _) => "C",
        (Button::Z, _) => "Z",
    }
}

/// Pad capture (the settings screen's gamepad rows): while armed, the input
/// thread stores the next pressed pad button here instead of routing it to
/// the game, and disarms itself.
static PAD_CAPTURE: AtomicBool = AtomicBool::new(false);
static CAPTURED_BUTTON: Mutex<Option<Button>> = Mutex::new(None);

/// Arms pad capture: the next pressed pad button (`Unknown`, `Mode` and the
/// settings trigger never count) is stored for `take_captured_button`.
#[allow(dead_code)] // R5
pub fn start_pad_capture() {
    *CAPTURED_BUTTON.lock().unwrap() = None;
    PAD_CAPTURE.store(true, Ordering::Relaxed);
}

/// Takes the captured button since the last take (None = still waiting).
/// Reads the stored value whether or not capture is still armed: the input
/// thread disarms itself when it stores one.
#[allow(dead_code)] // R5
pub fn take_captured_button() -> Option<Button> {
    CAPTURED_BUTTON.lock().unwrap().take()
}

/// Disarms pad capture and drops anything stored (the overlay closed or the
/// capture was cancelled).
#[allow(dead_code)] // R5
pub fn cancel_pad_capture() {
    PAD_CAPTURE.store(false, Ordering::Relaxed);
    *CAPTURED_BUTTON.lock().unwrap() = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialises the tests that read or rewrite the global binding tables
    /// (`keyboard_bindings` / `gamepad_bindings`): a bind/reset test would
    /// otherwise race a reader test's assertions.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    /// CONT-bit view of `bits_for` for the existing button assertions (the
    /// cast is safe there; keyboard stick bits live above `1 << 16` and are
    /// asserted against `STICK_*_BIT` through `bits_for` directly).
    fn bits<K: PartialEq>(bindings: &[Binding<K>], input: K) -> u16 {
        bits_for(bindings, input) as u16
    }

    /// What `uvReadController` makes of one axis (kernel/system.c).
    fn game_axis(v: i8, sat: i8, div: f32) -> f32 {
        if v < 0 {
            ((v.max(-sat).min(-7)) as f32 + 7.0) / div
        } else {
            ((v.min(sat).max(7)) as f32 - 7.0) / div
        }
    }

    #[test]
    fn deadzone_and_range() {
        assert_eq!(stick_to_n64(0.0, 0.0), (0, 0));
        assert_eq!(stick_to_n64(0.1, -0.05), (0, 0));
        assert_eq!(stick_to_n64(1.0, 0.0), (80, 0));
        assert_eq!(stick_to_n64(-1.0, 0.0), (-80, 0));
        assert_eq!(stick_to_n64(0.0, -1.0), (0, -80));
        // Beyond the unit circle (square gates) stays clamped.
        let (x, y) = stick_to_n64(1.0, 1.0);
        assert!(x <= 80 && y <= 80 && x > 50 && y > 50);
        // A full round-gate diagonal reaches the octagon corner (~70,70),
        // which the game reads as full on both axes, like a real pad.
        let d = std::f32::consts::FRAC_1_SQRT_2;
        let (x, y) = stick_to_n64(d, -d);
        assert!((70..=72).contains(&x) && (-72..=-70).contains(&y), "{x},{y}");
        assert_eq!(game_axis(x, 68, 61.0), 1.0);
        assert_eq!(game_axis(y, 70, 63.0), -1.0);
        // Halfway along a diagonal stays half (linear inside the gate).
        let (x, _) = stick_to_n64(0.5 * d, 0.5 * d);
        assert!((35..=45).contains(&x), "{x}");
        // Just past the deadzone = the game's first live value.
        let (x, _) = stick_to_n64(STICK_DEADZONE + 0.02, 0.0);
        assert!((7..=10).contains(&x), "{x}");
        assert!(game_axis(x, 68, 61.0) < 0.05);
        // Full deflection saturates the game's axis on both sides.
        assert_eq!(game_axis(stick_to_n64(1.0, 0.0).0, 68, 61.0), 1.0);
        assert_eq!(game_axis(stick_to_n64(0.0, -1.0).1, 70, 63.0), -1.0);
        // NaN input is idle, not garbage.
        assert_eq!(stick_to_n64(f32::NAN, 0.0), (0, 0));
    }

    #[test]
    fn monotonic() {
        let mut last = 0;
        for i in 0..=100 {
            let (x, _) = stick_to_n64(i as f32 / 100.0, 0.0);
            assert!(x >= last);
            last = x;
        }
    }

    #[test]
    fn buttons_and_c_stick() {
        let _lock = TEST_LOCK.lock().unwrap();
        assert_eq!(gamepad_button(Button::South), CONT_A);
        assert_eq!(gamepad_button(Button::West), CONT_B);
        assert_eq!(gamepad_button(Button::LeftTrigger2), CONT_Z);
        assert_eq!(gamepad_button(Button::RightTrigger), CONT_R);
        assert_eq!(c_stick_buttons(0.0, 0.0), 0);
        assert_eq!(c_stick_buttons(0.9, 0.9), CONT_C_RIGHT | CONT_C_UP);
        assert_eq!(c_stick_buttons(-0.6, -0.2), CONT_C_LEFT);
    }

    #[test]
    fn keyboard() {
        use KeyCode::*;
        let _lock = TEST_LOCK.lock().unwrap();
        let p = keyboard_pad(&[KeyW, Space]);
        assert_eq!(
            p,
            Pad {
                button: CONT_A,
                stick_x: 0,
                stick_y: 80
            }
        );
        // Opposite keys cancel.
        assert_eq!(keyboard_pad(&[KeyA, KeyD]).stick_x, 0);
        let d = keyboard_pad(&[ArrowUp, ArrowRight]);
        assert!(d.stick_x == d.stick_y && d.stick_x > 50 && d.stick_x < 80);
        assert_eq!(keyboard_pad(&[KeyX]), Pad::default());
    }

    #[test]
    fn merge_prefers_larger_stick() {
        let a = Pad {
            button: CONT_A,
            stick_x: 10,
            stick_y: 0,
        };
        let b = Pad {
            button: CONT_R,
            stick_x: 0,
            stick_y: -60,
        };
        assert_eq!(
            a.merge(b),
            Pad {
                button: CONT_A | CONT_R,
                stick_x: 0,
                stick_y: -60
            }
        );
    }

    #[test]
    fn script_parses() {
        let s = parse_script("# c\n100 A start # x\n+10\n200 stick -80,40 z\n").unwrap();
        assert_eq!(s.len(), 3);
        assert_eq!(s[0].retrace, 100);
        assert_eq!(s[0].pad.button, CONT_A | CONT_START);
        assert_eq!(s[1].retrace, 110);
        assert_eq!(s[1].pad, Pad::default());
        assert_eq!(
            (s[2].pad.button, s[2].pad.stick_x, s[2].pad.stick_y),
            (CONT_Z, -80, 40)
        );
        assert!(parse_script("5 Q").is_err());
        assert!(parse_script("5\n4").is_err());
        assert!(parse_script("5 stick 1").is_err());
    }

    #[test]
    fn recorded_lines_parse_back() {
        let pads = [
            Pad::default(),
            Pad {
                button: CONT_A | CONT_Z | CONT_C_RIGHT | CONT_LEFT,
                stick_x: -80,
                stick_y: 37,
            },
            Pad {
                button: 0xff3f,
                stick_x: 0,
                stick_y: -1,
            }, // all 14 buttons,
        ];
        for (i, &pad) in pads.iter().enumerate() {
            let s = parse_script(&script_line(100 + i as u64, pad)).unwrap();
            assert_eq!(
                s[0],
                ScriptStep {
                    retrace: 100 + i as u64,
                    pad
                }
            );
        }
    }

    #[test]
    fn on_key_publishes() {
        let _lock = TEST_LOCK.lock().unwrap();
        on_key(KeyCode::Enter, true);
        assert!(SOURCES.lock().unwrap().held_keys.contains(&KeyCode::Enter));
        on_key(KeyCode::Enter, false);
        assert!(SOURCES.lock().unwrap().held_keys.is_empty());
    }

    #[test]
    fn key_name_parsing() {
        use KeyCode::*;
        let _lock = TEST_LOCK.lock().unwrap();
        assert_eq!(parse_key("Space"), Some(Space));
        assert_eq!(parse_key(" KeyX "), Some(KeyX));
        assert_eq!(parse_key("W"), Some(KeyW));
        assert_eq!(parse_key("1"), Some(Digit1));
        assert_eq!(parse_key("LShift"), Some(ShiftLeft));
        assert_eq!(parse_key("Esc"), Some(Escape));
        // `NumpadEnter` is its own key, not an alias of `Enter` (S2).
        assert_eq!(parse_key("NumpadEnter"), Some(NumpadEnter));
        assert_eq!(parse_key("Enter"), Some(Enter));
        // The OS keys under their UI-events names (winit: SuperLeft/Right).
        assert_eq!(parse_key("MetaLeft"), Some(SuperLeft));
        assert_eq!(parse_key("F13"), Some(F13));
        assert_eq!(parse_key("NumpadDivide"), Some(NumpadDivide));
        assert_eq!(parse_key("F12"), Some(F12));
        assert_eq!(parse_key("NotAKey"), None);
        assert_eq!(parse_key(""), None);
        // Bound buttons and the stick slots coexist in `key_mapping`.
        assert_eq!(key_mapping(KeyCode::Space), Some((CONT_A, 0, 0)));
        assert_eq!(key_mapping(KeyCode::KeyW), Some((0, 0, 1)));
        assert_eq!(key_mapping(KeyCode::KeyX), None);
    }

    #[test]
    fn gamepad_name_parsing() {
        let _lock = TEST_LOCK.lock().unwrap();
        assert_eq!(parse_gamepad_button("South"), Some(Button::South));
        assert_eq!(parse_gamepad_button("ZR"), Some(Button::RightTrigger2));
        assert_eq!(parse_gamepad_button("dpad_up"), Some(Button::DPadUp));
        assert_eq!(parse_gamepad_button("C"), Some(Button::C));
        assert_eq!(parse_gamepad_button("Z"), Some(Button::Z));
        assert_eq!(parse_gamepad_button("Lever"), None);
        // Defaults still resolve through the binding table.
        assert_eq!(gamepad_button(Button::South), CONT_A);
        assert_eq!(gamepad_button(Button::East), CONT_B);
        assert_eq!(gamepad_button(Button::LeftTrigger2), CONT_Z);
        assert_eq!(gamepad_button(Button::Select), 0);
    }

    fn cfg(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|&(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn resolve_pad(pairs: &[(&str, &str)]) -> Vec<Binding<Button>> {
        let reserved = Some(Button::Select);
        resolve(
            &GAMEPAD_SLOTS,
            &cfg(pairs),
            "gamepad",
            parse_gamepad_button,
            reserved,
        )
    }

    #[test]
    fn override_takes_input_from_other_defaults() {
        // ZL defaults to Z; binding it to R moves it (the old first-match
        // lookup kept it on Z and silently ignored the override).
        let b = resolve_pad(&[("R", "ZL")]);
        assert_eq!(bits(&b, Button::LeftTrigger2), CONT_R);
        assert_eq!(bits(&b, Button::RightTrigger), 0, "R's defaults replaced");
        // Z lost its only default: unbound, shown as such.
        let rows = binding_rows(&b);
        assert!(rows.contains(&("Z", "(unbound)".to_string())));
        // Slot names are case-insensitive; unknown slots/names are ignored.
        let b = resolve_pad(&[("c_up", "LB"), ("Turbo", "A"), ("A", "Lever")]);
        assert_eq!(bits(&b, Button::LeftTrigger), CONT_C_UP);
        assert_eq!(bits(&b, Button::South), CONT_A);
        // The same input bound to two slots explicitly presses both.
        let b = resolve_pad(&[("A", "South"), ("B", "South")]);
        assert_eq!(bits(&b, Button::South), CONT_A | CONT_B);
    }

    #[test]
    fn settings_button_is_never_bound() {
        let b = resolve_pad(&[("START", "Select")]);
        assert_eq!(bits(&b, Button::Select), 0);
        assert_eq!(bits(&b, Button::Start), CONT_START, "keeps its default");
    }

    #[test]
    fn keyboard_override_resolution() {
        let c = cfg(&[("A", "KeyX"), ("B", "Space")]);
        let b = resolve(&KEYBOARD_SLOTS, &c, "keyboard", parse_key, None);
        assert_eq!(bits(&b, KeyCode::KeyX), CONT_A);
        assert_eq!(bits(&b, KeyCode::Space), CONT_B);
        assert_eq!(bits(&b, KeyCode::ShiftLeft), 0);
        assert_eq!(bits(&b, KeyCode::ControlLeft), CONT_Z);
    }

    /// Stick slots resolve in the same pass as the buttons: a key bound to
    /// `STICK_UP` is taken off every other slot's defaults (C_UP's default
    /// KeyI), and binding KeyW to a button takes it off the stick.
    #[test]
    fn stick_slots_resolve_like_buttons() {
        let c = cfg(&[("STICK_UP", "KeyI")]);
        let b = resolve(&KEYBOARD_SLOTS, &c, "keyboard", parse_key, None);
        assert_eq!(bits_for(&b, KeyCode::KeyI), STICK_UP_BIT);
        assert_eq!(
            bits_for(&b, KeyCode::KeyI) & u32::from(CONT_C_UP),
            0,
            "KeyI taken off C_UP's defaults"
        );
        // The override replaces STICK_UP's defaults entirely; the other
        // stick slots keep theirs.
        assert_eq!(bits_for(&b, KeyCode::KeyW), 0);
        assert_eq!(bits_for(&b, KeyCode::KeyS), STICK_DOWN_BIT);

        // Binding KeyW to A: KeyW is off STICK_UP's defaults, so the key
        // presses A only (it used to be stick + button).
        let c = cfg(&[("A", "KeyW")]);
        let b = resolve(&KEYBOARD_SLOTS, &c, "keyboard", parse_key, None);
        assert_eq!(bits_for(&b, KeyCode::KeyW), u32::from(CONT_A));
        // Sanity: the untouched defaults keep driving the stick slots.
        let d = resolve(&KEYBOARD_SLOTS, &cfg(&[]), "keyboard", parse_key, None);
        assert_eq!(bits_for(&d, KeyCode::KeyW), STICK_UP_BIT);
        assert_eq!(bits_for(&d, KeyCode::ArrowRight), STICK_RIGHT_BIT);
    }

    #[test]
    fn closing_press_is_suppressed_until_released() {
        let _lock = TEST_LOCK.lock().unwrap();
        let mut s = Sources {
            gamepads: Pad {
                button: CONT_START | CONT_A,
                ..IDLE
            },
            held_keys: Vec::new(),
            script: IDLE,
            ble: IDLE,
            suppressed: CONT_START | CONT_A, // as set by `overlay_toggled(false)`
            nav: IDLE,
        };
        publish(&mut s);
        assert_eq!(s.suppressed, CONT_START | CONT_A);
        s.gamepads.button = CONT_A; // Start released
        publish(&mut s);
        assert_eq!(s.suppressed, CONT_A);
        s.gamepads.button = CONT_START | CONT_A; // a fresh Start press
        publish(&mut s);
        assert_eq!(s.suppressed, CONT_A, "a new press is not suppressed");
    }

    /// Every default key and button round trips through its `Debug` name
    /// (the settings screen stores `format!("{k:?}")` in the override maps).
    #[test]
    fn default_debug_names_round_trip() {
        for (_, slot, keys) in KEYBOARD_SLOTS {
            for k in keys {
                assert_eq!(parse_key(&format!("{k:?}")), Some(*k), "{slot}: {k:?}");
            }
        }
        for (_, slot, buttons) in GAMEPAD_SLOTS {
            for b in buttons {
                assert_eq!(
                    parse_gamepad_button(&format!("{b:?}")),
                    Some(*b),
                    "{slot}: {b:?}"
                );
            }
        }
    }

    /// Every winit `KeyCode` round trips through its `Debug` name — the
    /// spelling `bind_key` saves (S2 review: the numpad, `Super*`, `Intl*`
    /// and F13+ names used to come back unknown after a restart, and
    /// `NumpadEnter` wrongly parsed as `Enter`).
    #[test]
    fn all_keycodes_round_trip() {
        for (name, k) in KEYCODE_NAMES {
            assert_eq!(parse_key(name), Some(k), "{name}");
            // The spelling as saved (same text; parses case-insensitively too).
            assert_eq!(parse_key(&format!("{k:?}")), Some(k), "{name}");
        }
    }

    /// Every gilrs button capture can produce round trips through its
    /// `Debug` name (S2 review: `C` and `Z` used to come back unknown).
    #[test]
    fn all_gilrs_buttons_round_trip() {
        for b in [
            Button::South,
            Button::East,
            Button::North,
            Button::West,
            Button::C,
            Button::Z,
            Button::LeftTrigger,
            Button::LeftTrigger2,
            Button::RightTrigger,
            Button::RightTrigger2,
            Button::Select,
            Button::Start,
            Button::Mode,
            Button::LeftThumb,
            Button::RightThumb,
            Button::DPadUp,
            Button::DPadDown,
            Button::DPadLeft,
            Button::DPadRight,
        ] {
            assert_eq!(parse_gamepad_button(&format!("{b:?}")), Some(b), "{b:?}");
        }
        // `Unknown` is not a real button and never parses.
        assert_eq!(parse_gamepad_button("Unknown"), None);
    }

    /// The settings screen's bind path: binding an input that is already an
    /// override drops that other override (one input per slot), the bound
    /// slot replaces its defaults, and reset puts everything back. Restores
    /// the pre-test state so parallel reader tests see the config's maps.
    #[test]
    fn rebind_conflict_and_reset() {
        let _lock = TEST_LOCK.lock().unwrap();
        let orig_keyboard = KEYBOARD.with(|l| l.overrides.clone());
        let orig_gamepad = GAMEPAD.with(|l| l.overrides.clone());
        // Restore whatever the config had, even if an assert fails.
        struct Restore {
            keyboard: HashMap<String, String>,
            gamepad: HashMap<String, String>,
        }
        impl Drop for Restore {
            fn drop(&mut self) {
                KEYBOARD.edit(|m| *m = self.keyboard.clone());
                GAMEPAD.edit(|m| *m = self.gamepad.clone());
            }
        }
        let _restore = Restore {
            keyboard: orig_keyboard,
            gamepad: orig_gamepad,
        };

        // Keyboard: X replaces A's defaults; Space falls back to nothing
        // bound to it, A shows the override.
        bind_key("A", KeyCode::KeyX);
        assert_eq!(bits(&keyboard_bindings(), KeyCode::KeyX), CONT_A);
        assert_eq!(
            bits(&keyboard_bindings(), KeyCode::Space),
            0,
            "A's defaults replaced"
        );
        assert_eq!(
            keyboard_overrides().get("A").map(String::as_str),
            Some("KeyX")
        );
        // The stick follows the same tables: KeyW is still a stick key
        // (through the STICK_UP slot) and Space is unbound.
        assert_eq!(key_mapping(KeyCode::KeyW), Some((0, 0, 1)));
        // Conflict: binding KeyX to B drops A's override, so A is back on
        // Space and B has KeyX.
        bind_key("B", KeyCode::KeyX);
        assert_eq!(bits(&keyboard_bindings(), KeyCode::KeyX), CONT_B);
        assert_eq!(
            bits(&keyboard_bindings(), KeyCode::Space),
            CONT_A,
            "A back to default"
        );
        let ov = keyboard_overrides();
        assert!(!ov.contains_key("A"), "A's conflicting override dropped");
        assert_eq!(ov.get("B").map(String::as_str), Some("KeyX"));
        // Live view through key_mapping: KeyW bound to A means buttons
        // only, no stick direction (KeyW came off STICK_UP's defaults).
        bind_key("A", KeyCode::KeyW);
        assert_eq!(key_mapping(KeyCode::KeyW), Some((CONT_A, 0, 0)));
        // Aliases count as the same input too: a "LSHIFT" override goes when
        // ShiftLeft is bound, so the short and winit spellings can't both
        // hold a slot.
        KEYBOARD.edit(|m| {
            m.insert("C_UP".to_string(), "LSHIFT".to_string());
        });
        bind_key("A", KeyCode::ShiftLeft);
        bind_key("B", KeyCode::ShiftRight);
        bind_key("A", KeyCode::ShiftLeft);
        let ov = keyboard_overrides();
        assert!(
            !ov.contains_key("C_UP"),
            "the LSHIFT alias of ShiftLeft was dropped"
        );
        assert_eq!(ov.get("A").map(String::as_str), Some("ShiftLeft"));
        assert_eq!(bits(&keyboard_bindings(), KeyCode::ShiftLeft), CONT_A);
        assert_eq!(bits(&keyboard_bindings(), KeyCode::ShiftRight), CONT_B);
        // A hand-written lower-case slot name (`a = "KeyM"` in pw64.toml)
        // is the same slot: rebinding A replaces it instead of adding a
        // second input.
        KEYBOARD.edit(|m| {
            m.insert("a".to_string(), "KeyM".to_string());
        });
        bind_key("A", KeyCode::KeyN);
        assert_eq!(bits(&keyboard_bindings(), KeyCode::KeyM), 0);
        assert_eq!(bits(&keyboard_bindings(), KeyCode::KeyN), CONT_A);
        // Reset: every default back, no overrides.
        reset_keyboard();
        assert_eq!(bits(&keyboard_bindings(), KeyCode::Space), CONT_A);
        assert_eq!(bits(&keyboard_bindings(), KeyCode::ShiftLeft), CONT_B);
        assert_eq!(bits(&keyboard_bindings(), KeyCode::KeyZ), CONT_Z);
        assert_eq!(bits(&keyboard_bindings(), KeyCode::ControlLeft), CONT_Z);
        assert_eq!(key_mapping(KeyCode::KeyW), Some((0, 0, 1)));
        assert!(keyboard_overrides().is_empty());

        // Gamepad: North (C_UP's only default) bound to START, then to A:
        // the second bind drops START's override (one input per slot), and
        // C_UP ends up unbound (its only default was taken, no override).
        bind_button("START", Button::North);
        assert_eq!(bits(&gamepad_bindings(), Button::North), CONT_START);
        bind_button("A", Button::North);
        assert_eq!(bits(&gamepad_bindings(), Button::North), CONT_A);
        assert_eq!(
            bits(&gamepad_bindings(), Button::South),
            0,
            "A's defaults replaced"
        );
        let ov = gamepad_overrides();
        assert!(
            !ov.contains_key("START"),
            "START's conflicting override dropped"
        );
        assert_eq!(ov.get("A").map(String::as_str), Some("North"));
        // The settings trigger stays free: capture never binds it (checked
        // by `settings_button_is_never_bound` through resolve).
        reset_gamepad();
        assert_eq!(bits(&gamepad_bindings(), Button::South), CONT_A);
        assert_eq!(bits(&gamepad_bindings(), Button::North), CONT_C_UP);
        assert_eq!(bits(&gamepad_bindings(), Button::LeftTrigger2), CONT_Z);
        assert!(gamepad_overrides().is_empty());
    }

    /// Pad family detection: vendor id first, then the name.
    #[test]
    fn pad_family_detection() {
        use PadFamily::*;
        assert_eq!(pad_family(Some(0x045E), "Anything"), Xbox);
        assert_eq!(pad_family(Some(0x054C), "Anything"), PlayStation);
        assert_eq!(pad_family(Some(0x057E), "Anything"), Nintendo);
        // Name fallback carries the backends where vendor_id() is None.
        assert_eq!(pad_family(None, "Xbox 360 Controller"), Xbox);
        assert_eq!(pad_family(None, "XINPUT CONTROLLER"), Xbox);
        assert_eq!(
            pad_family(None, "DualSense Wireless Controller"),
            PlayStation
        );
        assert_eq!(pad_family(None, "Wireless Controller"), PlayStation);
        assert_eq!(pad_family(None, "Sony PlayStation pad"), PlayStation);
        assert_eq!(pad_family(None, "Pro Controller"), Nintendo);
        assert_eq!(pad_family(None, "Joy-Con (L)"), Nintendo);
        assert_eq!(pad_family(None, "Generic USB Gamepad"), Generic);
        assert_eq!(pad_family(None, ""), Generic);
        // An "xbox" name wins over a later "wireless controller" match.
        assert_eq!(pad_family(None, "Xbox Wireless Controller"), Xbox);
    }

    /// The settings screen's button names, per family.
    #[test]
    fn pad_button_names() {
        use PadFamily::{Generic, Nintendo, PlayStation, Xbox};
        let xbox = pad_family(None, "Xbox Series X Controller");
        assert_eq!(button_name(Button::South, xbox), "A");
        assert_eq!(button_name(Button::East, xbox), "B");
        assert_eq!(button_name(Button::LeftTrigger2, xbox), "LT");
        assert_eq!(button_name(Button::Select, xbox), "View");
        assert_eq!(button_name(Button::Start, xbox), "Menu");
        let ps = pad_family(Some(0x054C), "DualSense");
        assert_eq!(button_name(Button::South, ps), "Cross");
        assert_eq!(button_name(Button::East, ps), "Circle");
        assert_eq!(button_name(Button::LeftTrigger2, ps), "L2");
        assert_eq!(button_name(Button::Start, ps), "Options");
        let nin = pad_family(None, "Pro Controller");
        assert_eq!(
            button_name(Button::South, nin),
            "B",
            "positional: South is B"
        );
        assert_eq!(button_name(Button::East, nin), "A");
        assert_eq!(button_name(Button::LeftTrigger2, nin), "ZL");
        assert_eq!(button_name(Button::Start, nin), "+");
        // D-pad names are family-independent.
        for f in [Xbox, PlayStation, Nintendo, Generic] {
            assert_eq!(button_name(Button::DPadUp, f), "D-pad up");
            assert_eq!(button_name(Button::DPadLeft, f), "D-pad left");
        }
        assert_eq!(
            button_name(Button::South, Generic),
            "A",
            "D5: generic = Xbox"
        );
    }

    /// Pad capture state machine: arm, store, take once; cancel drops.
    #[test]
    fn pad_capture_flow() {
        assert!(take_captured_button().is_none(), "nothing armed");
        cancel_pad_capture(); // a stray cancel is fine
        start_pad_capture();
        // The input thread's arm of the store: direct stand-in for the
        // event loop (the loop itself needs a real gilrs device).
        *CAPTURED_BUTTON.lock().unwrap() = Some(Button::East);
        PAD_CAPTURE.store(false, Ordering::Relaxed);
        assert_eq!(take_captured_button(), Some(Button::East));
        assert!(take_captured_button().is_none(), "taken once");
        cancel_pad_capture();
        assert!(take_captured_button().is_none());
        start_pad_capture();
        cancel_pad_capture();
        assert!(take_captured_button().is_none(), "cancel drops");
    }
}
