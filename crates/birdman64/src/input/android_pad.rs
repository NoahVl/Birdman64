//! Android pads → N64 controller slots. gilrs has no Android backend, so the
//! window feeds pad key events here: winit reports gamepad keys as
//! `PhysicalKey::Unidentified(NativeKeyCode::Android(keycode))` and the
//! d-pad as the arrow keys. Each Android keycode becomes the gilrs
//! [`Button`] at the same position, so the `[input.gamepad]` bindings, pad
//! capture, the settings trigger and the overlay navigation behave as for
//! desktop pads. The state lands in the same `gamepads` list the gilrs
//! thread fills (one entry, id [`PAD_ID`]).
//!
//! TODO(android): analog sticks and triggers. winit 0.30 turns joystick
//! MotionEvents into `Touch` events (X/Y only, no source), so the axes
//! (AXIS_X/Y, Z/RZ, LTRIGGER/RTRIGGER, HAT_X/Y) need their own path (a winit
//! patch, or reading the input queue ahead of winit); [`set_axes`] takes
//! them. Until axes arrive, the d-pad also moves the stick (digital).
//! TODO(android): per-device ids and names (`InputDevice.getName()` through
//! JNI): every pad merges into one "Android gamepad" entry today.

use super::{
    CAPTURED_BUTTON, CONT_A, CONT_B, CONT_DOWN, CONT_LEFT, CONT_RIGHT, CONT_START, CONT_UP,
    FOCUSED, PAD_CAPTURE, Pad, SOURCES, TRIGGER_THRESHOLD, c_stick_buttons, digital_stick,
    gamepad_bindings, hat_buttons, publish, slot_bits_pad, stick_to_n64,
};
use crate::touch::TouchPad;
use gilrs::Button;
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use winit::keyboard::{KeyCode, NativeKeyCode, PhysicalKey};

/// The `gamepads` entry id: no gilrs ids exist on Android to collide with.
const PAD_ID: usize = 0;
const PAD_NAME: &str = "Android gamepad";

/// Android `KeyEvent` keycodes (android/keycodes.h).
const KEYCODE_BACK: u32 = 4;

/// What the window should do with a key event.
pub enum AndroidKey {
    /// A pad key: consumed here.
    Pad,
    /// The system Back key: opens / closes the settings overlay (the
    /// activity must not finish on it).
    Back,
    /// Not a pad key: the window's keyboard path handles it.
    Keyboard,
}

/// Analog axes, Android's ranges: sticks and hat -1..1 (y down), triggers
/// 0..1.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Axes {
    pub lx: f32,
    pub ly: f32,
    pub rx: f32,
    pub ry: f32,
    pub lt: f32,
    pub rt: f32,
    pub hat_x: f32,
    pub hat_y: f32,
}

#[derive(Default)]
struct State {
    held: Vec<Button>,
    axes: Axes,
    /// Some axis event arrived: the d-pad stops doubling as the stick.
    axes_seen: bool,
    settings_down: bool,
    /// The on-screen touch controls (`touch.rs`): N64 bits, merged after
    /// the bindings (they are labelled with N64 names, never rebound).
    touch: TouchPad,
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
    axes_seen: false,
    settings_down: false,
    touch: TouchPad::IDLE,
});

/// Android gamepad keycode → the gilrs button at the same position
/// (BUTTON_A is the bottom face button = `South`). Pure.
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
    republish(&mut st, settings_btn);
    AndroidKey::Pad
}

/// The touch controls' state (window thread, on every touch event).
pub fn set_touch(touch: TouchPad) {
    let mut st = STATE.lock().unwrap();
    if st.touch != touch {
        st.touch = touch;
        republish(&mut st, crate::settings::pad_button());
    }
}

/// Releases everything held (pad keys, axes, touch): the window lost focus
/// or the app went to the background, where the release events never come.
/// Called by `input::release_all_keys` before it locks the sources (lock
/// order: this `STATE`, then `SOURCES`).
pub fn release_all() {
    let mut st = STATE.lock().unwrap();
    st.held.clear();
    st.axes = Axes::default();
    st.touch = TouchPad::IDLE;
    republish(&mut st, crate::settings::pad_button());
}

/// TODO(android) hook: the analog axes of the pad, once something delivers
/// them (module docs). Its caller should also hide the touch controls
/// (`touch::Controls::hide`), as a pad key does.
#[allow(dead_code)]
pub fn set_axes(axes: Axes) {
    let mut st = STATE.lock().unwrap();
    st.axes = axes;
    st.axes_seen = true;
    republish(&mut st, crate::settings::pad_button());
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

/// Publishes the pad (window thread) and edge-detects the settings trigger.
fn republish(st: &mut State, settings_btn: Button) {
    let settings_down = FOCUSED.load(Ordering::Relaxed) && st.held.contains(&settings_btn);
    if settings_down && !st.settings_down {
        crate::window::notify_settings();
    }
    st.settings_down = settings_down;
    let pad = game_pad(&st.held, &st.axes, st.axes_seen, &st.touch);
    let nav = nav_pad(&st.held, &st.axes);
    let mut s = SOURCES.lock().unwrap();
    s.gamepads = vec![(PAD_ID, PAD_NAME.to_string(), pad)];
    s.nav = nav;
    publish(&mut s);
}
