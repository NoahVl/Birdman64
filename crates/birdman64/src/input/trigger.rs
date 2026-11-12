//! The controller's settings trigger (`[input] settings_pad_button`,
//! docs/notes/input.md "Settings trigger"): one button (default Select,
//! reserved: never bound to a game input, exactly as before) or a
//! combination like `"Select+Start"` (reserves nothing: each button stays a
//! normal, rebindable game input).
//!
//! A combination fires once per controller when its last button goes down,
//! on one controller only. Its buttons are then kept from the game and the
//! overlay until all are released, and a member pressed on its own is held
//! back for [`GRACE`] (if the rest follows in time, the game never saw it;
//! if not, it arrives that much late). A single button needs none of that:
//! it is unbound, so the game never sees it anyway. [`ChordGate`] does the
//! timing per controller; `input.rs` / `mobile_pad.rs` feed it.

use super::{PadFamily, button_name, parse_gamepad_button};
use gilrs::Button;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// How long a combination's first button is held back from the game and
/// the overlay, waiting for the rest.
pub(crate) const GRACE: Duration = Duration::from_millis(50);

/// The most buttons a combination may have.
const MAX_CHORD: usize = 3;

/// The settings trigger on a gilrs / mobile pad.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PadTrigger {
    /// One button, reserved for the overlay (the old behaviour).
    One(Button),
    /// Two or three buttons held together on one controller; none reserved.
    Chord(Vec<Button>),
}

impl Default for PadTrigger {
    fn default() -> Self {
        PadTrigger::One(Button::Select)
    }
}

impl PadTrigger {
    /// `"Select"` or `"Select+Start"` (gilrs names and the usual
    /// abbreviations, any case, spaces allowed). `None`: unknown names,
    /// a repeated button, more than three, or `Unknown`.
    pub fn parse(s: &str) -> Option<Self> {
        let names: Vec<&str> = s.split('+').map(str::trim).collect();
        let mut buttons = Vec::new();
        for n in names {
            let b = parse_gamepad_button(n)?;
            if b == Button::Unknown || buttons.contains(&b) {
                return None;
            }
            buttons.push(b);
        }
        match buttons.len() {
            1 => Some(PadTrigger::One(buttons[0])),
            2..=MAX_CHORD => Some(PadTrigger::Chord(buttons)),
            _ => None,
        }
    }

    /// From a captured set of buttons (in press order).
    pub fn from_buttons(buttons: &[Button]) -> Option<Self> {
        match buttons {
            [] => None,
            [b] => Some(PadTrigger::One(*b)),
            bs if bs.len() <= MAX_CHORD => Some(PadTrigger::Chord(bs.to_vec())),
            _ => None,
        }
    }

    /// The `pw64.toml` spelling (`parse` reads it back).
    pub fn to_config(&self) -> String {
        self.members()
            .iter()
            .map(|b| format!("{b:?}"))
            .collect::<Vec<_>>()
            .join("+")
    }

    pub fn members(&self) -> &[Button] {
        match self {
            PadTrigger::One(b) => std::slice::from_ref(b),
            PadTrigger::Chord(bs) => bs,
        }
    }

    /// The button kept out of the game bindings: only a single-button
    /// trigger reserves one.
    pub fn reserved(&self) -> Option<Button> {
        match self {
            PadTrigger::One(b) => Some(*b),
            PadTrigger::Chord(_) => None,
        }
    }

    /// Player-facing: "View", "Select + Start" in the pad family's names.
    pub fn label(&self, family: PadFamily) -> String {
        self.members()
            .iter()
            .map(|&b| button_name(b, family))
            .collect::<Vec<_>>()
            .join(" + ")
    }
}

static TRIGGER: Mutex<Option<PadTrigger>> = Mutex::new(None);

/// The live trigger: `pw64.toml` on first use (an unreadable value warns and
/// falls back to Select), then what the Controls page set.
pub fn pad_trigger() -> PadTrigger {
    TRIGGER
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get_or_insert_with(|| {
            let name = crate::config::get().input.settings.pad_button.as_deref();
            name.and_then(|n| {
                let t = PadTrigger::parse(n);
                if t.is_none() {
                    eprintln!(
                        "[config] input.settings_pad_button = {n:?}: not a button or a \
                         combination like \"Select+Start\"; using Select"
                    );
                }
                t
            })
            .unwrap_or_default()
        })
        .clone()
}

/// Applies a new trigger at once (the gamepad bindings re-resolve: a
/// single button leaves them, a combination's buttons return) and saves
/// it (Select, the default, removes the key).
pub fn set_pad_trigger(t: PadTrigger) -> Result<(), String> {
    let value = (t != PadTrigger::default()).then(|| t.to_config());
    *TRIGGER.lock().unwrap_or_else(PoisonError::into_inner) = Some(t);
    super::refresh_gamepad_bindings();
    crate::config::save_settings_pad_button(value.as_deref())
}

/// The reserved settings button, if the trigger is a single one.
pub fn reserved() -> Option<Button> {
    pad_trigger().reserved()
}

/// One controller's view of its trigger, per poll: is it complete, should
/// it fire, and which of its members to keep from the game / overlay.
#[derive(Debug, Clone)]
pub struct ChordGate {
    /// Since when each member has been held (`None`: up), by member index.
    since: Vec<Option<Instant>>,
    /// Fired and not yet fully released.
    latched: bool,
}

impl Default for ChordGate {
    fn default() -> Self {
        Self::new()
    }
}

/// One [`ChordGate::update`] result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateOut {
    /// The trigger completed on this poll: open / close the overlay.
    pub fire: bool,
    /// Fired and still held (the global edge detection reads this).
    pub down: bool,
    /// Per member: keep it from the game and the overlay this poll.
    pub mask: Vec<bool>,
}

impl ChordGate {
    /// No member held, not fired (const: `mobile_pad`'s static state).
    pub const fn new() -> Self {
        Self {
            since: Vec::new(),
            latched: false,
        }
    }

    /// `held[i]`: member `i` is down. `grace`: how long a lone member is
    /// held back ([`GRACE`] for a combination on a polled pad; zero for a
    /// single button, or where nothing re-polls in time).
    pub fn update(&mut self, held: &[bool], now: Instant, grace: Duration) -> GateOut {
        if self.since.len() != held.len() {
            *self = ChordGate {
                since: vec![None; held.len()],
                latched: false,
            };
        }
        for (s, &h) in self.since.iter_mut().zip(held) {
            match (h, *s) {
                (true, None) => *s = Some(now),
                (false, _) => *s = None,
                _ => {}
            }
        }
        let all = !held.is_empty() && held.iter().all(|&h| h);
        let any = held.iter().any(|&h| h);
        let fire = all && !self.latched;
        if fire {
            self.latched = true;
        } else if self.latched && !any {
            self.latched = false;
        }
        let mask = if self.latched {
            held.to_vec()
        } else {
            self.since
                .iter()
                .map(|s| s.is_some_and(|t| now.duration_since(t) < grace))
                .collect()
        };
        GateOut {
            fire,
            down: self.latched,
            mask,
        }
    }
}

/// Capture for the Controls page's "Open settings" row: everything pressed
/// on one controller until the first release becomes the trigger (a press
/// and release: one button; two or three held, then let go: a combination).
struct Capture {
    pad: Option<gilrs::GamepadId>,
    held: Vec<Button>,
    got: Option<PadTrigger>,
}

static CAPTURE: Mutex<Option<Capture>> = Mutex::new(None);
static ARMED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn start_capture() {
    *CAPTURE.lock().unwrap_or_else(PoisonError::into_inner) = Some(Capture {
        pad: None,
        held: Vec::new(),
        got: None,
    });
    ARMED.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// The captured trigger (taken once).
pub fn take_capture() -> Option<PadTrigger> {
    let mut g = CAPTURE.lock().unwrap_or_else(PoisonError::into_inner);
    let got = g.as_mut().and_then(|c| c.got.take());
    if got.is_some() {
        *g = None;
    }
    got
}

pub fn cancel_capture() {
    ARMED.store(false, std::sync::atomic::Ordering::Relaxed);
    *CAPTURE.lock().unwrap_or_else(PoisonError::into_inner) = None;
}

/// A trigger capture is waiting (the input thread holds the current
/// trigger back meanwhile: the player is pressing buttons for a new one).
pub fn capturing() -> bool {
    ARMED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Input thread, per gilrs event while armed. `true`: the event was taken.
pub fn capture_event(ev: &gilrs::Event) -> bool {
    if !capturing() {
        return false;
    }
    let mut g = CAPTURE.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(c) = g.as_mut().filter(|c| c.got.is_none()) else {
        return false;
    };
    if c.pad.is_some_and(|p| p != ev.id) {
        return false;
    }
    match ev.event {
        gilrs::EventType::ButtonPressed(b, _) if !matches!(b, Button::Unknown | Button::Mode) => {
            c.pad = Some(ev.id);
            if !c.held.contains(&b) && c.held.len() < MAX_CHORD {
                c.held.push(b);
            }
            true
        }
        gilrs::EventType::ButtonReleased(b, _) if c.held.contains(&b) => {
            c.got = PadTrigger::from_buttons(&c.held);
            ARMED.store(false, std::sync::atomic::Ordering::Relaxed);
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_round_trip() {
        assert_eq!(
            PadTrigger::parse("Select"),
            Some(PadTrigger::One(Button::Select))
        );
        assert_eq!(
            PadTrigger::parse("back"),
            Some(PadTrigger::One(Button::Select))
        );
        let chord = PadTrigger::parse(" select + START ").unwrap();
        assert_eq!(
            chord,
            PadTrigger::Chord(vec![Button::Select, Button::Start])
        );
        assert_eq!(chord.to_config(), "Select+Start");
        assert_eq!(PadTrigger::parse(&chord.to_config()), Some(chord.clone()));
        assert_eq!(chord.reserved(), None, "a combination reserves nothing");
        assert_eq!(PadTrigger::default().reserved(), Some(Button::Select));
        for bad in [
            "",
            "Select+",
            "Select+Select",
            "Lever",
            "A+B+X+Y",
            "Unknown",
        ] {
            assert_eq!(PadTrigger::parse(bad), None, "{bad:?}");
        }
        assert_eq!(
            chord.label(PadFamily::Xbox),
            "View + Menu",
            "names follow the controller family"
        );
        assert_eq!(
            PadTrigger::from_buttons(&[Button::LeftTrigger, Button::RightTrigger]),
            Some(PadTrigger::Chord(vec![
                Button::LeftTrigger,
                Button::RightTrigger
            ]))
        );
        assert_eq!(PadTrigger::from_buttons(&[]), None);
    }

    /// Either order fires once, when the second button goes down; the
    /// first one is held back for the grace; everything stays masked until
    /// both are up; a lone press past the grace reaches the game.
    #[test]
    fn chord_gate_timing() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        for order in [[true, false], [false, true]] {
            let mut g = ChordGate::default();
            let up = g.update(&[false, false], ms(0), GRACE);
            assert!(!up.fire && !up.down && up.mask == [false, false]);
            let first = g.update(&order, ms(10), GRACE);
            assert!(!first.fire);
            assert_eq!(first.mask, order, "the first button is held back");
            let both = g.update(&[true, true], ms(30), GRACE);
            assert!(both.fire && both.down);
            assert_eq!(both.mask, [true, true]);
            let still = g.update(&[true, true], ms(500), GRACE);
            assert!(!still.fire, "fires once");
            let one_up = g.update(&order, ms(510), GRACE);
            assert!(!one_up.fire && one_up.down);
            assert_eq!(one_up.mask, order, "masked until all are released");
            // Re-pressing the released one does not fire again.
            assert!(!g.update(&[true, true], ms(520), GRACE).fire);
            let released = g.update(&[false, false], ms(530), GRACE);
            assert!(!released.down);
            assert!(
                g.update(&[true, true], ms(540), GRACE).fire,
                "again after release"
            );
        }
        // A lone press: held back for the grace, then the game gets it.
        let mut g = ChordGate::default();
        assert_eq!(g.update(&[true, false], ms(0), GRACE).mask, [true, false]);
        assert_eq!(g.update(&[true, false], ms(49), GRACE).mask, [true, false]);
        assert_eq!(g.update(&[true, false], ms(50), GRACE).mask, [false, false]);
        // A single button: fires on press, no hold-back needed.
        let mut g = ChordGate::default();
        let o = g.update(&[true], ms(0), Duration::ZERO);
        assert!(o.fire && o.down && o.mask == [true]);
        assert!(!g.update(&[true], ms(10), Duration::ZERO).fire);
        assert!(!g.update(&[false], ms(20), Duration::ZERO).down);
        // A changed trigger (other member count) starts over.
        let mut g = ChordGate::default();
        g.update(&[true, true], ms(0), GRACE);
        assert!(g.update(&[true, true, true], ms(1), GRACE).fire);
    }
}
