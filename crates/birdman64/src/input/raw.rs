//! Custom setups for controllers without a usable mapping (docs/notes/input.md
//! "Generic controllers").
//!
//! gilrs normalises a pad only when the SDL mapping DB (or the OS driver)
//! knows it; a generic USB joystick or an N64 clone often reports nothing but
//! `Button::Unknown` / `Axis::Unknown`. Its raw state is still there: gilrs
//! keeps every button and axis by its native code (`gilrs::ev::Code`), mapped
//! or not. A *custom setup* binds the N64 slots straight to those codes, per
//! controller model (its SDL-style GUID), in `pw64.toml`:
//!
//! ```toml
//! [input.devices."03000000790000000600000000000000"]
//! name = "USB Joystick"     # informational
//! STICK_X = "ABS(0)+"       # analog: the raw direction that is stick right
//! STICK_Y = "ABS(1)-"       # analog: the raw direction that is stick up
//! A = "KEY(290)"            # a raw button
//! C_UP = "ABS(5)-"          # half an axis, as a button (pressed past half way)
//! STICK_UP = "ABS(17)-"     # a hat direction: a digital stick direction
//! SETTINGS = "KEY(296)"     # opens the settings screen (optional; a
//!                           # combination too: "KEY(296)+KEY(297)")
//! ```
//!
//! A raw input is the code's gilrs spelling (`Display`, platform specific:
//! `KEY(304)` / `ABS(1)` on Linux, `Button(3)` / `Axis(1)` / `Switch(0)` on
//! Windows, `BUTTON(3)` / `GENERIC_DESKTOP(49)` on macOS), with `+` / `-` for
//! one half of an axis. Settings > Controls > Controller setup captures them
//! and shows the live raw state, so nobody has to type these by hand.
//!
//! This module is the platform-independent part (parsing, evaluation,
//! capture matching); `input.rs` feeds it gilrs state.

use super::{
    CONT_A, CONT_B, CONT_C_DOWN, CONT_C_LEFT, CONT_C_RIGHT, CONT_DOWN, CONT_LEFT, CONT_RIGHT,
    CONT_START, CONT_UP, GAMEPAD_SLOTS, Pad, STICK_DOWN_BIT, STICK_LEFT_BIT, STICK_RIGHT_BIT,
    STICK_UP_BIT, slot_bits_pad, stick_to_n64,
};
use std::collections::BTreeMap;

/// A half axis or a hat direction counts as pressed past this (gilrs's own
/// axis-to-button threshold is 0.75; a worn clone stick may never get there).
const PRESS: f32 = 0.5;

/// Capture: an axis must move this far from where it rested when the capture
/// armed (a trigger resting at -1 moves to +1; a stick from 0 to ±1).
const CAPTURE_TRAVEL: f32 = 0.6;

/// Which half of an axis (`+`: positive values, `-`: negative values).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Half {
    Pos,
    Neg,
}

/// How far an axis value `v` presses half `h` (0..1).
pub(crate) fn half_level(h: Half, v: f32) -> f32 {
    (h.sign() * v).max(0.0)
}

impl Half {
    fn sign(self) -> f32 {
        match self {
            Half::Pos => 1.0,
            Half::Neg => -1.0,
        }
    }
}

/// One raw input: a native code in its gilrs spelling, and for an axis the
/// half that counts (`None`: a button).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawInput {
    pub code: String,
    pub half: Option<Half>,
}

impl RawInput {
    pub fn button(code: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            half: None,
        }
    }

    pub fn axis(code: impl Into<String>, half: Half) -> Self {
        Self {
            code: code.into(),
            half: Some(half),
        }
    }

    /// `NAME(digits)` with an optional `+` / `-` (surrounding spaces
    /// allowed). Anything else is `None`: the setup keeps the slot unbound.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        let (code, half) = match s.as_bytes().last()? {
            b'+' => (&s[..s.len() - 1], Some(Half::Pos)),
            b'-' => (&s[..s.len() - 1], Some(Half::Neg)),
            _ => (s, None),
        };
        let code = code.trim_end();
        let (name, rest) = code.split_once('(')?;
        let digits = rest.strip_suffix(')')?;
        let name_ok = !name.is_empty()
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            && !name.as_bytes()[0].is_ascii_digit();
        let digits_ok = !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit());
        (name_ok && digits_ok).then(|| Self {
            code: code.to_string(),
            half,
        })
    }

    /// The `pw64.toml` spelling (`parse` reads it back).
    pub fn to_config(&self) -> String {
        match self.half {
            None => self.code.clone(),
            Some(Half::Pos) => format!("{}+", self.code),
            Some(Half::Neg) => format!("{}-", self.code),
        }
    }

    /// How far this input is pressed, 0..1: a button 0/1, a half axis its
    /// value on that side.
    fn level(&self, state: &impl RawState) -> f32 {
        match self.half {
            None => f32::from(u8::from(state.button(&self.code))),
            Some(h) => (h.sign() * state.axis(&self.code)).max(0.0),
        }
    }

    fn pressed(&self, state: &impl RawState) -> bool {
        self.level(state) >= PRESS
    }
}

/// A pad's raw state by code spelling: what a setup is evaluated against
/// (gilrs's `GamepadState` in `input.rs`, a table in the tests).
pub(crate) trait RawState {
    /// Is the button with this code held? Unknown codes: not held.
    fn button(&self, code: &str) -> bool;
    /// The axis value (-1..1) with this code. Unknown codes: 0.
    fn axis(&self, code: &str) -> f32;
}

/// A code whose spelling says it is a hat (a d-pad switch reported as two
/// axes): Linux `ABS_HAT0X..ABS_HAT3Y` (codes 16..23), Windows `Switch(n)`,
/// macOS the hat-switch usage (57). `input.rs` also asks gilrs whether the
/// code is its d-pad.
pub(crate) fn looks_like_hat(code: &str) -> bool {
    let Some((name, rest)) = code.split_once('(') else {
        return false;
    };
    let n: Option<u32> = rest.trim_end_matches(')').parse().ok();
    match name {
        "Switch" => true,
        "ABS" => n.is_some_and(|n| (16..=23).contains(&n)),
        "GENERIC_DESKTOP" => n == Some(57),
        _ => false,
    }
}

/// Digital slots beyond `[input.gamepad]`'s: a gamepad's C Down / Left /
/// Right come from its right stick, a clone's are buttons.
const EXTRA_SLOTS: [(u32, &str); 3] = [
    (CONT_C_DOWN as u32, "C_DOWN"),
    (CONT_C_LEFT as u32, "C_LEFT"),
    (CONT_C_RIGHT as u32, "C_RIGHT"),
];

/// Every digital slot of a setup: bit and name.
fn digital_slots() -> impl Iterator<Item = (u32, &'static str)> {
    GAMEPAD_SLOTS
        .iter()
        .map(|&(bit, n, _)| (bit, n))
        .chain(EXTRA_SLOTS)
}

/// Slot names a setup accepts: the digital slots, the two analog stick
/// axes and the settings trigger.
pub(crate) fn slot_names() -> impl Iterator<Item = &'static str> {
    digital_slots()
        .map(|(_, n)| n)
        .chain(["STICK_X", "STICK_Y", "SETTINGS"])
}

/// The most inputs a settings combination may have.
const MAX_CHORD: usize = 3;

/// A `SETTINGS` value: one raw input or a combination joined by `+`
/// (`"KEY(296)+KEY(297)"`, `"ABS(2)++KEY(5)"`: a `+` starts a new input only
/// where a name follows, so an axis half's own `+` stays). `None`: an
/// unreadable part, a repeat, or more than three.
pub(crate) fn parse_chord(s: &str) -> Option<Vec<RawInput>> {
    let b = s.as_bytes();
    let mut parts = Vec::new();
    let mut start = 0;
    for i in 0..b.len() {
        let next = s[i + 1..].trim_start().as_bytes().first();
        if b[i] == b'+' && next.is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_') {
            parts.push(&s[start..i]);
            start = i + 1;
        }
    }
    parts.push(&s[start..]);
    let mut out: Vec<RawInput> = Vec::new();
    for p in parts {
        let i = RawInput::parse(p)?;
        if out.contains(&i) {
            return None;
        }
        out.push(i);
    }
    (out.len() <= MAX_CHORD).then_some(out)
}

/// A combination's `pw64.toml` spelling (`parse_chord` reads it back).
fn chord_config(inputs: &[RawInput]) -> String {
    inputs
        .iter()
        .map(RawInput::to_config)
        .collect::<Vec<_>>()
        .join("+")
}

/// One controller model's custom setup (`[input.devices."<GUID>"]`).
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Profile {
    /// The name saved with it (the pad's name when it was set up).
    pub name: String,
    /// Digital slots: CONT bits, `STICK_*` bits.
    pub buttons: Vec<(u32, RawInput)>,
    /// The settings trigger: inputs held together (empty: keyboard only).
    /// A combination's inputs may also be game buttons.
    pub settings: Vec<RawInput>,
    /// Analog stick: the raw axis half that is right / up.
    pub stick_x: Option<RawInput>,
    pub stick_y: Option<RawInput>,
}

impl Profile {
    /// Parses one `[input.devices."<GUID>"]` table (slot → raw input; `name`
    /// is the label). Unknown slots and unreadable values are reported in
    /// `problems` and skipped; the rest of the setup still applies.
    pub fn from_map(map: &BTreeMap<String, String>, problems: &mut Vec<String>) -> Self {
        let mut p = Profile::default();
        for (slot, value) in map {
            let slot = slot.trim();
            if slot.eq_ignore_ascii_case("name") {
                p.name = value.clone();
                continue;
            }
            let Some(name) = slot_names().find(|n| n.eq_ignore_ascii_case(slot)) else {
                problems.push(format!("unknown slot {slot:?}"));
                continue;
            };
            if name == "SETTINGS" {
                match parse_chord(value) {
                    Some(c) => p.settings = c,
                    None => problems.push(format!(
                        "SETTINGS = {value:?}: not a raw input or a combination like \
                         \"KEY(296)+KEY(297)\""
                    )),
                }
                continue;
            }
            let Some(input) = RawInput::parse(value) else {
                problems.push(format!(
                    "{name} = {value:?}: not a raw input like \"KEY(304)\""
                ));
                continue;
            };
            match name {
                "STICK_X" | "STICK_Y" if input.half.is_none() => {
                    problems.push(format!("{name} = {value:?}: needs an axis with + or -"));
                }
                "STICK_X" => p.stick_x = Some(input),
                "STICK_Y" => p.stick_y = Some(input),
                _ => p.buttons.push((slot_bit(name), input)),
            }
        }
        p
    }

    /// The `pw64.toml` table for this setup (`from_map` reads it back).
    pub fn to_map(&self) -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        if !self.name.is_empty() {
            m.insert("name".to_string(), self.name.clone());
        }
        for (slot, input) in [("STICK_X", &self.stick_x), ("STICK_Y", &self.stick_y)] {
            if let Some(i) = input {
                m.insert(slot.to_string(), i.to_config());
            }
        }
        for (bit, input) in &self.buttons {
            m.insert(slot_name(*bit).to_string(), input.to_config());
        }
        if !self.settings.is_empty() {
            m.insert("SETTINGS".to_string(), chord_config(&self.settings));
        }
        m
    }

    /// Slot bits held right now (CONT, `STICK_*`).
    fn bits(&self, state: &impl RawState) -> u32 {
        self.buttons
            .iter()
            .filter(|(_, i)| i.pressed(state))
            .fold(0, |b, (bit, _)| b | bit)
    }

    /// The analog stick, -1..1 per axis (y up): the bound halves keep every
    /// intermediate value the controller reports.
    fn stick(&self, state: &impl RawState) -> (f32, f32) {
        let axis = |i: &Option<RawInput>| {
            i.as_ref().map_or(0.0, |i| {
                i.half.map_or(1.0, Half::sign) * state.axis(&i.code)
            })
        };
        (axis(&self.stick_x), axis(&self.stick_y))
    }

    /// The N64 pad this controller gives the game. (A single settings
    /// input is never also a game button: the guided setup refuses that; a
    /// combination's inputs are kept from the game by the trigger gate.)
    pub fn pad(&self, state: &impl RawState) -> Pad {
        self.pad_from(self.bits(state), state)
    }

    /// The settings overlay's navigation pad: the setup's own A / B / Start
    /// confirm / go back / close; its stick, `STICK_*` and N64 d-pad
    /// directions move.
    pub fn nav(&self, state: &impl RawState) -> Pad {
        self.pad_from(
            self.bits(state) & (u32::from(MENU_BITS) | STICK_BITS),
            state,
        )
    }

    fn pad_from(&self, bits: u32, state: &impl RawState) -> Pad {
        let (x, y) = self.stick(state);
        let (stick_x, stick_y) = stick_to_n64(x, y);
        slot_bits_pad(bits).merge(Pad {
            button: 0,
            stick_x,
            stick_y,
        })
    }

    /// Per settings input: is it held (for the trigger gate).
    pub fn settings_held(&self, state: &impl RawState) -> Vec<bool> {
        self.settings.iter().map(|i| i.pressed(state)).collect()
    }

    /// The N64 buttons (game, overlay navigation) an input gives here: what
    /// the trigger gate masks while a combination holds it.
    pub fn input_bits(&self, input: &RawInput) -> (u16, u16) {
        let game = self
            .buttons
            .iter()
            .filter(|(_, i)| i == input)
            .fold(0u32, |b, (bit, _)| b | bit) as u16;
        (game, game & MENU_BITS)
    }
}

/// The N64 buttons the settings overlay reads.
const MENU_BITS: u16 = CONT_A | CONT_B | CONT_START | CONT_UP | CONT_DOWN | CONT_LEFT | CONT_RIGHT;

const STICK_BITS: u32 = STICK_UP_BIT | STICK_DOWN_BIT | STICK_LEFT_BIT | STICK_RIGHT_BIT;

/// Slot name → bit.
fn slot_bit(name: &str) -> u32 {
    digital_slots()
        .find(|&(_, n)| n == name)
        .map_or(0, |(bit, _)| bit)
}

/// Bit → slot name (inverse of `slot_bit`).
fn slot_name(bit: u32) -> &'static str {
    digital_slots()
        .find(|&(b, _)| b == bit)
        .map_or("?", |(_, n)| n)
}

/// What a capture step takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Want {
    /// An axis moved off its rest (the analog stick steps).
    Axis,
    /// A button, or an axis / hat moved off its rest.
    Any,
    /// Like `Any`, but everything pressed before the first release: one
    /// input or a combination (the settings step).
    Chord,
}

/// One gilrs event, reduced to what the capture needs.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RawEvent {
    Pressed(String),
    Axis(String, f32),
}

/// Capture matching: the first input that counts for `want`. `rest` is the
/// axis value when the capture armed (0 for an axis not seen yet).
pub(crate) fn capture(want: Want, ev: &RawEvent, rest: f32) -> Option<RawInput> {
    match ev {
        RawEvent::Pressed(code) if want != Want::Axis => Some(RawInput::button(code.clone())),
        RawEvent::Axis(code, v) if (v - rest).abs() >= CAPTURE_TRAVEL && v.abs() >= PRESS => {
            let half = if *v > 0.0 { Half::Pos } else { Half::Neg };
            Some(RawInput::axis(code.clone(), half))
        }
        _ => None,
    }
}

/// One step of the guided setup: the slot it fills, the prompt, and what
/// it takes. The N64 d-pad slots (`UP` …) are left out: the game never
/// reads them (`pw64.toml` can still bind them).
pub(crate) const STEPS: [(&str, &str, Want); 17] = [
    ("STICK_X", "Push the stick right", Want::Axis),
    ("STICK_Y", "Push the stick up", Want::Axis),
    ("A", "Press A", Want::Any),
    ("B", "Press B", Want::Any),
    ("Z", "Press Z (the trigger underneath)", Want::Any),
    ("START", "Press Start", Want::Any),
    ("L", "Press L", Want::Any),
    ("R", "Press R", Want::Any),
    ("C_UP", "Press C Up", Want::Any),
    ("C_DOWN", "Press C Down", Want::Any),
    ("C_LEFT", "Press C Left", Want::Any),
    ("C_RIGHT", "Press C Right", Want::Any),
    (
        "STICK_UP",
        "Press D-pad up (moves like the stick)",
        Want::Any,
    ),
    ("STICK_DOWN", "Press D-pad down", Want::Any),
    ("STICK_LEFT", "Press D-pad left", Want::Any),
    ("STICK_RIGHT", "Press D-pad right", Want::Any),
    (
        "SETTINGS",
        "Hold the button (or two or three together) that should open Settings, then \
         let go (skip: keyboard only)",
        Want::Chord,
    ),
];

/// The guided setup's progress: one input per step (or skipped), applied
/// as a [`Profile`] at the end.
#[derive(Debug, Clone, Default)]
pub(crate) struct Wizard {
    pub step: usize,
    /// The controller being set up (GUID, name), locked by the first input.
    pub pad: Option<(String, String)>,
    pub got: Vec<(&'static str, RawInput)>,
}

impl Wizard {
    pub fn done(&self) -> bool {
        self.step >= STEPS.len()
    }

    pub fn current(&self) -> Option<(&'static str, &'static str, Want)> {
        STEPS.get(self.step).copied()
    }

    /// Takes a captured input for the current step (a settings
    /// combination: all its inputs). `Err` names the step that already uses
    /// it (the same code: an axis's other half counts, so the stick's X and
    /// Y can't share an axis, and a C button can't take a stick axis by
    /// accident). A combination's inputs may be game buttons too: that is
    /// what keeps every button free for the game. `Ok` moves on.
    pub fn take(&mut self, inputs: Vec<RawInput>) -> Result<(), &'static str> {
        let Some((slot, _, _)) = self.current() else {
            return Ok(());
        };
        if inputs.len() > 1 {
            if slot != "SETTINGS" {
                // Only the settings step takes a combination.
                return Err("a combination");
            }
            self.got.extend(inputs.into_iter().map(|i| (slot, i)));
            self.step += 1;
            return Ok(());
        }
        let Some(input) = inputs.into_iter().next() else {
            return Ok(());
        };
        // A stick axis owns both its halves; otherwise one input per slot,
        // but the two halves of an axis may be two buttons.
        let clash = self.got.iter().find(|(s, a)| {
            a.code == input.code && (is_stick(slot) || is_stick(s) || a.half == input.half)
        });
        if let Some((s, _)) = clash {
            return Err(s);
        }
        self.got.push((slot, input));
        self.step += 1;
        Ok(())
    }

    pub fn skip(&mut self) {
        if !self.done() {
            self.step += 1;
        }
    }

    /// The setup the captured inputs make.
    pub fn profile(&self) -> Profile {
        let mut p = Profile {
            name: self
                .pad
                .as_ref()
                .map(|(_, n)| n.clone())
                .unwrap_or_default(),
            ..Profile::default()
        };
        for (slot, input) in &self.got {
            match *slot {
                "STICK_X" => p.stick_x = Some(input.clone()),
                "STICK_Y" => p.stick_y = Some(input.clone()),
                "SETTINGS" => p.settings.push(input.clone()),
                s => p.buttons.push((slot_bit(s), input.clone())),
            }
        }
        p
    }
}

fn is_stick(slot: &str) -> bool {
    matches!(slot, "STICK_X" | "STICK_Y")
}

/// Normalises a GUID typed by hand or copied from an SDL tool: 32 hex digits,
/// lower case. `None` for anything else.
pub(crate) fn normalise_guid(s: &str) -> Option<String> {
    let s = s.trim();
    (s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit())).then(|| s.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{CONT_C_UP, CONT_Z};
    use std::collections::HashMap;

    #[derive(Default)]
    struct Table {
        buttons: Vec<&'static str>,
        axes: HashMap<&'static str, f32>,
    }

    impl RawState for Table {
        fn button(&self, code: &str) -> bool {
            self.buttons.contains(&code)
        }
        fn axis(&self, code: &str) -> f32 {
            self.axes.get(code).copied().unwrap_or(0.0)
        }
    }

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn raw_input_parse_and_round_trip() {
        for s in [
            "KEY(304)",
            "ABS(1)+",
            "ABS(1)-",
            "Switch(0)-",
            "GENERIC_DESKTOP(49)+",
        ] {
            assert_eq!(RawInput::parse(s).unwrap().to_config(), s);
        }
        assert_eq!(
            RawInput::parse(" ABS(2) - "),
            Some(RawInput::axis("ABS(2)", Half::Neg))
        );
        for bad in [
            "", "KEY", "KEY()", "KEY(3a)", "(3)", "3KEY(1)", "KEY(1)x", "South",
        ] {
            assert_eq!(RawInput::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn hats_are_told_apart_from_axes() {
        assert!(looks_like_hat("ABS(16)"));
        assert!(looks_like_hat("ABS(17)"));
        assert!(looks_like_hat("Switch(1)"));
        assert!(looks_like_hat("GENERIC_DESKTOP(57)"));
        assert!(!looks_like_hat("ABS(0)"));
        assert!(!looks_like_hat("Axis(0)"));
        assert!(!looks_like_hat("GENERIC_DESKTOP(48)"));
        assert!(!looks_like_hat("KEY(304)"));
    }

    #[test]
    fn profile_parses_reports_and_round_trips() {
        let mut problems = Vec::new();
        let p = Profile::from_map(
            &map(&[
                ("name", "USB Joystick"),
                ("stick_x", "ABS(0)+"),
                ("STICK_Y", "ABS(1)-"),
                ("A", "KEY(290)"),
                ("C_UP", "ABS(5)-"),
                ("SETTINGS", "KEY(296)"),
                ("Q", "KEY(1)"),
                ("B", "South"),
                ("STICK_X2", "x"),
            ]),
            &mut problems,
        );
        assert_eq!(p.name, "USB Joystick");
        assert_eq!(p.stick_x, Some(RawInput::axis("ABS(0)", Half::Pos)));
        assert_eq!(p.buttons.len(), 2);
        assert_eq!(p.settings, [RawInput::button("KEY(296)")]);
        assert_eq!(problems.len(), 3, "{problems:?}");
        // A stick axis without a half is refused, not guessed.
        let mut problems = Vec::new();
        let p2 = Profile::from_map(&map(&[("STICK_X", "ABS(0)")]), &mut problems);
        assert_eq!(p2.stick_x, None);
        assert_eq!(problems.len(), 1);
        // Round trip (slot names canonical).
        let mut none = Vec::new();
        assert_eq!(Profile::from_map(&p.to_map(), &mut none), p);
        assert!(none.is_empty());
    }

    #[test]
    fn analog_values_pass_through_and_hats_stay_digital() {
        let p = Profile::from_map(
            &map(&[
                ("STICK_X", "ABS(0)+"),
                ("STICK_Y", "ABS(1)-"),
                ("STICK_UP", "ABS(17)-"),
                ("Z", "KEY(294)"),
                ("C_UP", "ABS(5)-"),
                ("SETTINGS", "KEY(296)"),
            ]),
            &mut Vec::new(),
        );
        // Half-way right, a little up (raw y grows downward here).
        let mut t = Table::default();
        t.axes.insert("ABS(0)", 0.5);
        t.axes.insert("ABS(1)", -0.25);
        let pad = p.pad(&t);
        assert_eq!((pad.stick_x, pad.stick_y), stick_to_n64(0.5, 0.25));
        assert!(pad.stick_x > 7 && pad.stick_x < 80, "intermediate kept");
        // A hat direction: full deflection, never in between.
        let mut t = Table::default();
        t.axes.insert("ABS(17)", -1.0);
        let pad = p.pad(&t);
        assert_eq!((pad.stick_x, pad.stick_y), stick_to_n64(0.0, 1.0));
        // Buttons, a half axis as a C button, and the settings trigger
        // (held, but never sent to the game).
        let mut t = Table {
            buttons: vec!["KEY(294)", "KEY(296)"],
            ..Table::default()
        };
        t.axes.insert("ABS(5)", -0.9);
        let pad = p.pad(&t);
        assert_eq!(pad.button, CONT_Z | CONT_C_UP);
        assert_eq!(p.settings_held(&t), [true]);
        // The other half of the same axis does nothing.
        t.axes.insert("ABS(5)", 0.9);
        assert_eq!(p.pad(&t).button & CONT_C_UP, 0);
    }

    #[test]
    fn nav_keeps_only_the_menu_buttons() {
        let p = Profile::from_map(
            &map(&[("A", "KEY(1)"), ("Z", "KEY(2)"), ("STICK_DOWN", "KEY(3)")]),
            &mut Vec::new(),
        );
        let t = Table {
            buttons: vec!["KEY(1)", "KEY(2)", "KEY(3)"],
            ..Table::default()
        };
        let nav = p.nav(&t);
        assert_eq!(nav.button, CONT_A);
        assert!(nav.stick_y < 0, "d-pad down moves the menu down");
    }

    #[test]
    fn capture_needs_real_travel() {
        let press = RawEvent::Pressed("KEY(288)".into());
        assert_eq!(
            capture(Want::Any, &press, 0.0),
            Some(RawInput::button("KEY(288)"))
        );
        assert_eq!(
            capture(Want::Axis, &press, 0.0),
            None,
            "stick steps want an axis"
        );
        let small = RawEvent::Axis("ABS(0)".into(), 0.3);
        assert_eq!(capture(Want::Axis, &small, 0.0), None);
        let right = RawEvent::Axis("ABS(0)".into(), 0.9);
        assert_eq!(
            capture(Want::Axis, &right, 0.0),
            Some(RawInput::axis("ABS(0)", Half::Pos))
        );
        // A trigger resting at -1: resting there is no press; pulled is.
        let rest = RawEvent::Axis("ABS(2)".into(), -1.0);
        assert_eq!(capture(Want::Any, &rest, -1.0), None);
        let pulled = RawEvent::Axis("ABS(2)".into(), 1.0);
        assert_eq!(
            capture(Want::Any, &pulled, -1.0),
            Some(RawInput::axis("ABS(2)", Half::Pos))
        );
    }

    #[test]
    fn wizard_rejects_reuse_and_builds_the_profile() {
        let mut w = Wizard::default();
        assert_eq!(w.take(vec![RawInput::axis("ABS(0)", Half::Pos)]), Ok(()));
        // The stick's Y can't be the X axis, whichever half.
        assert_eq!(
            w.take(vec![RawInput::axis("ABS(0)", Half::Neg)]),
            Err("STICK_X")
        );
        assert_eq!(w.take(vec![RawInput::axis("ABS(1)", Half::Neg)]), Ok(()));
        assert_eq!(w.take(vec![RawInput::button("KEY(1)")]), Ok(())); // A
        assert_eq!(w.take(vec![RawInput::button("KEY(1)")]), Err("A")); // B
        assert_eq!(w.take(vec![RawInput::button("KEY(2)")]), Ok(())); // B
        // A stick axis can't become a button.
        assert_eq!(
            w.take(vec![RawInput::axis("ABS(1)", Half::Pos)]),
            Err("STICK_Y")
        );
        w.skip(); // Z
        // Two halves of one axis are fine as two buttons (C up / C down).
        while w.current().map(|c| c.0) != Some("C_UP") {
            w.skip();
        }
        assert_eq!(w.take(vec![RawInput::axis("ABS(5)", Half::Neg)]), Ok(()));
        assert_eq!(
            w.take(vec![RawInput::axis("ABS(5)", Half::Neg)]),
            Err("C_UP")
        );
        assert_eq!(w.take(vec![RawInput::axis("ABS(5)", Half::Pos)]), Ok(()));
        while !w.done() {
            w.skip();
        }
        let p = w.profile();
        assert_eq!(p.stick_x, Some(RawInput::axis("ABS(0)", Half::Pos)));
        assert_eq!(p.stick_y, Some(RawInput::axis("ABS(1)", Half::Neg)));
        assert_eq!(p.buttons.len(), 4);
        assert_eq!(
            p.to_map().get("C_DOWN").map(String::as_str),
            Some("ABS(5)+")
        );
    }

    #[test]
    fn every_step_is_a_slot() {
        for (slot, _, _) in STEPS {
            assert!(slot_names().any(|n| n == slot), "{slot}");
            if !is_stick(slot) && slot != "SETTINGS" {
                assert_ne!(slot_bit(slot), 0, "{slot} has a bit");
                assert_eq!(slot_name(slot_bit(slot)), slot);
            }
        }
        assert_eq!(slot_bit("C_RIGHT"), u32::from(crate::input::CONT_C_RIGHT));
    }

    #[test]
    fn settings_combinations() {
        let c = parse_chord("KEY(296)+KEY(297)").unwrap();
        assert_eq!(
            c,
            [RawInput::button("KEY(296)"), RawInput::button("KEY(297)")]
        );
        // An axis half's own `+` is not a separator.
        let c = parse_chord("ABS(2)+ + KEY(5)").unwrap();
        assert_eq!(
            c,
            [
                RawInput::axis("ABS(2)", Half::Pos),
                RawInput::button("KEY(5)")
            ]
        );
        assert_eq!(chord_config(&c), "ABS(2)++KEY(5)");
        assert_eq!(parse_chord(&chord_config(&c)), Some(c));
        assert_eq!(
            parse_chord("KEY(1)"),
            Some(vec![RawInput::button("KEY(1)")])
        );
        for bad in [
            "",
            "KEY(1)+KEY(1)",
            "KEY(1)+x",
            "KEY(1)+KEY(2)+KEY(3)+KEY(4)",
        ] {
            assert_eq!(parse_chord(bad), None, "{bad:?}");
        }
        // A combination may reuse game buttons; it round-trips; its inputs'
        // game bits are what the gate masks.
        let mut problems = Vec::new();
        let p = Profile::from_map(
            &map(&[("START", "KEY(297)"), ("SETTINGS", "KEY(296)+KEY(297)")]),
            &mut problems,
        );
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(p.settings.len(), 2);
        assert_eq!(p.to_map()["SETTINGS"], "KEY(296)+KEY(297)");
        assert_eq!(
            p.input_bits(&RawInput::button("KEY(297)")),
            (CONT_START, CONT_START)
        );
        assert_eq!(p.input_bits(&RawInput::button("KEY(296)")), (0, 0));
        // The wizard: only the settings step takes several inputs, and they
        // may be game buttons already.
        let mut w = Wizard::default();
        assert_eq!(
            w.take(vec![RawInput::button("KEY(1)"), RawInput::button("KEY(2)")]),
            Err("a combination")
        );
        while w.current().map(|c| c.0) != Some("START") {
            w.skip();
        }
        assert_eq!(w.take(vec![RawInput::button("KEY(297)")]), Ok(()));
        while w.current().map(|c| c.0) != Some("SETTINGS") {
            w.skip();
        }
        assert_eq!(w.current().unwrap().2, Want::Chord);
        let chord = vec![RawInput::button("KEY(296)"), RawInput::button("KEY(297)")];
        assert_eq!(w.take(chord.clone()), Ok(()));
        assert!(w.done());
        assert_eq!(w.profile().settings, chord);
        // A single settings input still can't be a game button.
        let mut w = Wizard::default();
        while w.current().map(|c| c.0) != Some("START") {
            w.skip();
        }
        w.take(vec![RawInput::button("KEY(297)")]).unwrap();
        while w.current().map(|c| c.0) != Some("SETTINGS") {
            w.skip();
        }
        assert_eq!(w.take(vec![RawInput::button("KEY(297)")]), Err("START"));
    }

    #[test]
    fn guid_normalisation() {
        assert_eq!(
            normalise_guid(" 03000000790000000600000000000000 "),
            Some("03000000790000000600000000000000".into())
        );
        assert_eq!(normalise_guid("0300ABCD"), None);
        assert_eq!(normalise_guid(&"g".repeat(32)), None);
    }
}
