//! Mobile pads → N64 controller slots. gilrs has no Android or iOS
//! backend, so a platform module feeds the pad state here: on Android the
//! window reports pad keys as `PhysicalKey::Unidentified(NativeKeyCode::
//! Android(keycode))` and the d-pad as the arrow keys ([`android_key`]),
//! and the sticks, analog triggers and hat as `AxisMotion` ([`android_axis`],
//! from a winit patch: stock winit 0.30 turns them into touches); on iOS the GameController poller (`ios_gc.rs`) pushes a full pad state
//! through [`set_pad`] every frame. Each Android keycode / GC button
//! becomes the gilrs [`Button`] at the same position, so the
//! `[input.gamepad]` bindings, pad capture, the settings trigger and the
//! overlay navigation behave as for desktop pads. The state lands in the
//! same `gamepads` list the gilrs thread fills on desktop (one entry, id
//! [`PAD_ID`]). The on-screen touch controls' state lives in
//! [`super::touch_pad`] (shared with iOS); this module reads it on every
//! republish, so keys, axes and touch land in one publish. Until a pad
//! sends an axis, the d-pad also moves the stick (digital).
//!
//! TODO(android): per-device ids and names (`InputDevice.getName()` through
//! JNI): every pad merges into one "Android gamepad" entry today.

use super::{
    CONT_A, CONT_B, CONT_DOWN, CONT_LEFT, CONT_RIGHT, CONT_START, CONT_UP, FOCUSED, Pad, SOURCES,
    TRIGGER_THRESHOLD, c_stick_buttons, digital_stick, gamepad_bindings, hat_buttons, publish,
    slot_bits_pad, stick_to_n64,
};
// Pad capture (settings rebinding): Android key events, iOS `set_pad`.
use super::{CAPTURED_BUTTON, PAD_CAPTURE};
use crate::touch::TouchPad;
use gilrs::Button;
use std::sync::Mutex;
use std::sync::atomic::Ordering;
// Android key events only: winit's Android keycode repr.
#[cfg(target_os = "android")]
use winit::keyboard::{KeyCode, NativeKeyCode, PhysicalKey};

/// The `gamepads` entry id: no gilrs ids exist on mobile to collide with.
const PAD_ID: usize = 0;
/// The published pad name until [`set_pad`] stores a device's own name:
/// every Android pad merges into one entry today; on iOS the touch
/// controls are the pad until a GameController is seen.
#[cfg(target_os = "android")]
const PAD_NAME: &str = "Android gamepad";
#[cfg(target_os = "ios")]
const PAD_NAME: &str = "Touch controls";

/// Android `KeyEvent` keycodes (android/keycodes.h).
#[cfg(target_os = "android")]
const KEYCODE_BACK: u32 = 4;

/// What the window should do with a key event.
#[cfg(target_os = "android")]
pub enum AndroidKey {
    /// A pad key: consumed here.
    Pad,
    /// The system Back key: opens / closes the settings overlay (the
    /// activity must not finish on it).
    Back,
    /// Not a pad key: the window's keyboard path handles it.
    Keyboard,
}

pub use super::pad_axes::Axes;
#[cfg(target_os = "android")]
use super::pad_axes::{RawAxes, axis_buttons};

struct State {
    held: Vec<Button>,
    axes: Axes,
    /// The latest raw Android axis values ([`android_axis`]).
    #[cfg(target_os = "android")]
    raw: RawAxes,
    /// Some axis event arrived: the d-pad stops doubling as the stick.
    axes_seen: bool,
    settings_down: bool,
    /// The pad name a platform module stored ([`set_pad`]); empty falls
    /// back to [`PAD_NAME`].
    name: String,
}

static STATE: Mutex<State> = Mutex::new(State {
    held: Vec::new(),
    axes: Axes {
        lx: 0.0,
        ly: 0.0,
        rx: 0.0,
        ry: 0.0,
        lt: 0.0,
        rt: 0.0,
        hat_x: 0.0,
        hat_y: 0.0,
    },
    #[cfg(target_os = "android")]
    raw: RawAxes::new(),
    axes_seen: false,
    settings_down: false,
    name: String::new(),
});

/// Android gamepad keycode → the gilrs button at the same position
/// (BUTTON_A is the bottom face button = `South`). Pure.
#[cfg(target_os = "android")]
pub fn button_for_keycode(code: u32) -> Option<Button> {
    Some(match code {
        19 => Button::DPadUp,
        20 => Button::DPadDown,
        21 => Button::DPadLeft,
        22 => Button::DPadRight,
        96 => Button::South,
        97 => Button::East,
        98 => Button::C,
        99 => Button::West,
        100 => Button::North,
        101 => Button::Z,
        102 => Button::LeftTrigger,
        103 => Button::RightTrigger,
        104 => Button::LeftTrigger2,
        105 => Button::RightTrigger2,
        106 => Button::LeftThumb,
        107 => Button::RightThumb,
        108 => Button::Start,
        109 => Button::Select,
        110 => Button::Mode,
        _ => return None,
    })
}

/// The window's key event → pad state. The arrow keys count as the d-pad
/// (that is how winit reports a pad's d-pad; a Bluetooth keyboard's arrows
/// then steer too, as on desktop).
#[cfg(target_os = "android")]
pub fn android_key(key: PhysicalKey, pressed: bool, repeat: bool) -> AndroidKey {
    let button = match key {
        PhysicalKey::Unidentified(NativeKeyCode::Android(KEYCODE_BACK)) => {
            return if pressed && !repeat {
                AndroidKey::Back
            } else {
                AndroidKey::Pad
            };
        }
        PhysicalKey::Unidentified(NativeKeyCode::Android(code)) => button_for_keycode(code),
        PhysicalKey::Code(KeyCode::ArrowUp) => Some(Button::DPadUp),
        PhysicalKey::Code(KeyCode::ArrowDown) => Some(Button::DPadDown),
        PhysicalKey::Code(KeyCode::ArrowLeft) => Some(Button::DPadLeft),
        PhysicalKey::Code(KeyCode::ArrowRight) => Some(Button::DPadRight),
        _ => None,
    };
    let Some(button) = button else {
        return AndroidKey::Keyboard;
    };
    if repeat {
        return AndroidKey::Pad;
    }
    let settings_btn = crate::settings::pad_button();
    // Pad capture (settings screen rebinding): the press is stored for the
    // overlay and never reaches the game (same rule as the gilrs thread).
    if pressed
        && PAD_CAPTURE.load(Ordering::Relaxed)
        && !matches!(button, Button::Mode)
        && button != settings_btn
    {
        *CAPTURED_BUTTON.lock().unwrap() = Some(button);
        PAD_CAPTURE.store(false, Ordering::Relaxed);
        return AndroidKey::Pad;
    }
    let mut st = STATE.lock().unwrap();
    st.held.retain(|&b| b != button);
    if pressed {
        st.held.push(button);
    }
    republish_state(&mut st, settings_btn);
    AndroidKey::Pad
}

/// Releases everything held (pad keys, axes, touch): the window lost focus
/// or the app went to the background, where the release events never come.
/// Called by `input::release_all_keys` before it locks the sources (lock
/// order: this `STATE` and the touch state, then `SOURCES`).
pub fn release_all() {
    super::touch_pad::clear();
    let mut st = STATE.lock().unwrap();
    st.held.clear();
    st.axes = Axes::default();
    #[cfg(target_os = "android")]
    {
        st.raw = RawAxes::new();
    }
    republish_state(&mut st, crate::settings::pad_button());
}

/// The window's `AxisMotion` event (one per axis of a pad MotionEvent) →
/// pad state. Returns true while the pad is clearly in use (a stick past
/// half travel, a trigger or the hat pressed): the window then hides the
/// touch controls, as a pad key does. Resting stick noise doesn't count.
#[cfg(target_os = "android")]
pub fn android_axis(axis: u32, value: f32) -> bool {
    let settings_btn = crate::settings::pad_button();
    let mut st = STATE.lock().unwrap();
    st.raw.set(axis, value);
    let axes = st.raw.axes();
    let pressed = axis_buttons(&axes, TRIGGER_THRESHOLD);
    let in_use = !pressed.is_empty()
        || [axes.lx, axes.ly, axes.rx, axes.ry]
            .iter()
            .any(|v| v.abs() > 0.5);
    if st.axes_seen && axes == st.axes {
        return in_use;
    }
    // Pad capture: a trigger or hat press binds like a button, as gilrs
    // reports them on desktop (the game is paused under the overlay).
    if PAD_CAPTURE.load(Ordering::Relaxed) {
        let before = axis_buttons(&st.axes, TRIGGER_THRESHOLD);
        if let Some(&b) = pressed
            .iter()
            .find(|&&b| !before.contains(&b) && b != settings_btn)
        {
            *CAPTURED_BUTTON.lock().unwrap() = Some(b);
            PAD_CAPTURE.store(false, Ordering::Relaxed);
        }
    }
    st.axes = axes;
    st.axes_seen = true;
    republish_state(&mut st, settings_btn);
    in_use
}

/// The iOS GameController poller's hook (`ios_gc::poll`, called every
/// frame): replaces the whole pad state — buttons, axes and the device
/// name for the settings screen. Axes always arrive here, so the d-pad
/// never doubles as the stick. No Android caller yet (keys only).
#[allow(dead_code)]
pub fn set_pad(held: &[Button], axes: Axes, name: &str) {
    let settings_btn = crate::settings::pad_button();
    let mut st = STATE.lock().unwrap();
    // Pad capture (settings screen rebinding), as in `android_key`: the
    // first newly pressed button is stored for the overlay (the game is
    // paused under it, so the press also reaching the pad is harmless).
    if PAD_CAPTURE.load(Ordering::Relaxed)
        && let Some(&b) = held
            .iter()
            .find(|&&b| !st.held.contains(&b) && b != Button::Mode && b != settings_btn)
    {
        *CAPTURED_BUTTON.lock().unwrap() = Some(b);
        PAD_CAPTURE.store(false, Ordering::Relaxed);
    }
    st.held = held.to_vec();
    st.axes = axes;
    st.axes_seen = true;
    st.name = name.to_string();
    republish_state(&mut st, settings_btn);
}

/// One pad's N64 state from the resolved gamepad bindings (the gilrs
/// `gamepad_pad` rules: triggers past the threshold, the hat as the d-pad,
/// right stick → C buttons). Android's stick y points down.
fn game_pad(held: &[Button], a: &Axes, axes_seen: bool, touch: &TouchPad) -> Pad {
    let hat = hat_buttons(a.hat_x, -a.hat_y);
    let mut bits = 0u32;
    for binding in gamepad_bindings().iter() {
        let down = binding.inputs.iter().any(|&b| {
            held.contains(&b)
                || match b {
                    Button::LeftTrigger2 => a.lt >= TRIGGER_THRESHOLD,
                    Button::RightTrigger2 => a.rt >= TRIGGER_THRESHOLD,
                    Button::DPadUp => hat & CONT_UP != 0,
                    Button::DPadDown => hat & CONT_DOWN != 0,
                    Button::DPadLeft => hat & CONT_LEFT != 0,
                    Button::DPadRight => hat & CONT_RIGHT != 0,
                    _ => false,
                }
        });
        if down {
            bits |= binding.bit;
        }
    }
    let (stick_x, stick_y) = if axes_seen {
        stick_to_n64(a.lx, -a.ly)
    } else {
        // No axes yet (module docs): the d-pad doubles as the stick.
        let dir = |pos: Button, neg: Button| {
            i32::from(held.contains(&pos)) - i32::from(held.contains(&neg))
        };
        digital_stick(
            dir(Button::DPadRight, Button::DPadLeft),
            dir(Button::DPadUp, Button::DPadDown),
        )
    };
    let (touch_x, touch_y) = stick_to_n64(touch.x, touch.y);
    slot_bits_pad(bits)
        .merge(Pad {
            button: c_stick_buttons(a.rx, -a.ry),
            stick_x,
            stick_y,
        })
        .merge(Pad {
            button: touch.buttons,
            stick_x: touch_x,
            stick_y: touch_y,
        })
}

/// The overlay's navigation pad (W3: fixed physical buttons, never rebound).
fn nav_pad(held: &[Button], a: &Axes) -> Pad {
    let mut button = hat_buttons(a.hat_x, -a.hat_y);
    for (b, bit) in [
        (Button::South, CONT_A),
        (Button::East, CONT_B),
        (Button::Start, CONT_START),
        (Button::DPadUp, CONT_UP),
        (Button::DPadDown, CONT_DOWN),
        (Button::DPadLeft, CONT_LEFT),
        (Button::DPadRight, CONT_RIGHT),
    ] {
        if held.contains(&b) {
            button |= bit;
        }
    }
    let (stick_x, stick_y) = stick_to_n64(a.lx, -a.ly);
    Pad {
        button,
        stick_x,
        stick_y,
    }
}

/// Republishes the whole pad from the current state: `touch_pad` calls
/// this when only its state changed (keys, axes and touch land in one
/// publish).
pub(crate) fn republish() {
    let settings_btn = crate::settings::pad_button();
    let mut st = STATE.lock().unwrap();
    republish_state(&mut st, settings_btn);
}

/// Publishes the pad (window thread) and edge-detects the settings trigger.
fn republish_state(st: &mut State, settings_btn: Button) {
    let settings_down = FOCUSED.load(Ordering::Relaxed) && st.held.contains(&settings_btn);
    if settings_down && !st.settings_down {
        crate::window::notify_settings();
    }
    st.settings_down = settings_down;
    let touch = super::touch_pad::touch();
    let pad = game_pad(&st.held, &st.axes, st.axes_seen, &touch);
    let nav = nav_pad(&st.held, &st.axes);
    let name = if st.name.is_empty() {
        PAD_NAME
    } else {
        &st.name
    };
    let mut s = SOURCES.lock().unwrap();
    s.gamepads = vec![(PAD_ID, name.to_string(), pad)];
    s.nav = nav;
    publish(&mut s);
}
