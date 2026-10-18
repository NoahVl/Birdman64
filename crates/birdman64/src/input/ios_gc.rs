//! iOS GameController pads → the shared mobile pad (`mobile_pad`), via
//! Apple's GameController framework (`objc2-game-controller`): gilrs has
//! no iOS backend and winit reports no pads. `poll` runs once per frame on
//! the main thread (the window's redraw path), takes the first controller
//! with an extended-gamepad profile and — on change — pushes the state
//! through `mobile_pad::set_pad`, mapped to the same gilrs [`Button`]
//! names a desktop pad reports. The sticks' y flips sign (`mobile_pad`'s
//! `Axes` use Android's y-down convention); the d-pad arrives as buttons,
//! so the hat axes stay 0. `poll` reports pad activity (a button newly
//! pressed, a stick moved past half travel) so the caller hides the touch
//! controls, as an Android pad key does.

use super::mobile_pad::{self, Axes};
use gilrs::Button;
use objc2_game_controller::{GCController, GCDevice, GCExtendedGamepad};
use std::sync::Mutex;

/// One extended-gamepad profile's raw readings, plain data so the mapping
/// below is pure and testable.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Profile {
    a: bool,
    b: bool,
    x: bool,
    y: bool,
    lb: bool,
    rb: bool,
    /// Triggers: `isPressed` (a held button) plus the analog value.
    lt: bool,
    rt: bool,
    lt_v: f32,
    rt_v: f32,
    /// Sticks, ObjC's y-up convention (the mapping flips y).
    lx: f32,
    ly: f32,
    rx: f32,
    ry: f32,
    l3: bool,
    r3: bool,
    menu: bool,
    options: bool,
    home: bool,
    dpad_up: bool,
    dpad_down: bool,
    dpad_left: bool,
    dpad_right: bool,
}

/// Maps the raw profile readings to the published state: gilrs `Button`s
/// (the `[input.gamepad]` bindings and the pad capture speak them) and
/// `Axes` with the stick y flipped to Android's y-down convention. Pure.
fn to_pad(p: &Profile) -> (Vec<Button>, Axes) {
    let mut held = Vec::new();
    for (on, b) in [
        (p.a, Button::South),
        (p.b, Button::East),
        (p.x, Button::West),
        (p.y, Button::North),
        (p.lb, Button::LeftTrigger),
        (p.rb, Button::RightTrigger),
        (p.lt, Button::LeftTrigger2),
        (p.rt, Button::RightTrigger2),
        (p.l3, Button::LeftThumb),
        (p.r3, Button::RightThumb),
        (p.menu, Button::Start),
        (p.options, Button::Select),
        (p.home, Button::Mode),
        (p.dpad_up, Button::DPadUp),
        (p.dpad_down, Button::DPadDown),
        (p.dpad_left, Button::DPadLeft),
        (p.dpad_right, Button::DPadRight),
    ] {
        if on {
            held.push(b);
        }
    }
    let axes = Axes {
        lx: p.lx,
        ly: -p.ly,
        rx: p.rx,
        ry: -p.ry,
        lt: p.lt_v,
        rt: p.rt_v,
        hat_x: 0.0,
        hat_y: 0.0,
    };
    (held, axes)
}

/// The last state pushed to `mobile_pad`, for the change detection and the
/// touch-hide activity check. `poll` runs on the main thread only.
#[derive(Debug, PartialEq)]
struct Last {
    held: Vec<Button>,
    axes: Axes,
    name: String,
}

static LAST: Mutex<Option<Last>> = Mutex::new(None);

/// True when a stick moved past half travel since the last poll.
fn stick_crossed(prev: &Axes, now: &Axes) -> bool {
    /// Half travel: a stick pushed past this hides the touch controls.
    const PAST: f32 = 0.5;
    [
        (prev.lx, now.lx),
        (prev.ly, now.ly),
        (prev.rx, now.rx),
        (prev.ry, now.ry),
    ]
    .iter()
    .any(|&(old, new)| new.abs() > PAST && old.abs() <= PAST)
}

/// Polls the pads (main thread, once per frame): publishes the first
/// extended-gamepad controller's state when it changed, reports pad
/// activity (see module docs) and publishes one idle pad when the pad went
/// away (a disconnect must not leave stale buttons).
pub fn poll() -> bool {
    // SAFETY: main thread (the window's redraw path), see `read_profile`.
    let current = unsafe { read_profile() };
    let mut last = LAST.lock().unwrap();
    match current {
        Some((profile, name)) => {
            let (held, axes) = to_pad(&profile);
            let activity = match last.as_ref() {
                Some(prev) => {
                    held.iter().any(|b| !prev.held.contains(b)) || stick_crossed(&prev.axes, &axes)
                }
                None => false,
            };
            let changed = match last.as_ref() {
                Some(prev) => prev.held != held || prev.axes != axes || prev.name != name,
                None => true,
            };
            if changed {
                mobile_pad::set_pad(&held, axes, &name);
                *last = Some(Last { held, axes, name });
            }
            activity
        }
        None => {
            if let Some(prev) = last.take()
                && (!prev.held.is_empty() || prev.axes != Axes::default())
            {
                mobile_pad::set_pad(&[], Axes::default(), &prev.name);
            }
            false
        }
    }
}

/// Reads the first controller with an extended-gamepad profile: its raw
/// readings and name. `None` without one.
///
/// SAFETY: the getters are ObjC message sends on live system objects,
/// called from the main thread (`poll` runs in the window's redraw path).
unsafe fn read_profile() -> Option<(Profile, String)> {
    let controllers = unsafe { GCController::controllers() };
    for i in 0..controllers.count() {
        let controller = controllers.objectAtIndex(i);
        // SAFETY: as above.
        let pad = unsafe { controller.extendedGamepad() };
        if let Some(pad) = pad {
            return Some(unsafe { profile(&controller, &pad) });
        }
    }
    None
}

/// Reads one profile's elements.
///
/// SAFETY: the getters are ObjC message sends on live system objects,
/// called from the main thread (`poll` runs in the window's redraw path).
unsafe fn profile(controller: &GCController, pad: &GCExtendedGamepad) -> (Profile, String) {
    // SAFETY: as above.
    unsafe {
        let dpad = pad.dpad();
        let lx = pad.leftThumbstick();
        let rx = pad.rightThumbstick();
        let lt = pad.leftTrigger();
        let rt = pad.rightTrigger();
        let name = GCDevice::vendorName(controller)
            .map(|n| n.to_string())
            .unwrap_or_else(|| "Controller".to_string());
        let p = Profile {
            a: pad.buttonA().isPressed(),
            b: pad.buttonB().isPressed(),
            x: pad.buttonX().isPressed(),
            y: pad.buttonY().isPressed(),
            lb: pad.leftShoulder().isPressed(),
            rb: pad.rightShoulder().isPressed(),
            lt: lt.isPressed(),
            rt: rt.isPressed(),
            lt_v: lt.value(),
            rt_v: rt.value(),
            lx: lx.xAxis().value(),
            ly: lx.yAxis().value(),
            rx: rx.xAxis().value(),
            ry: rx.yAxis().value(),
            l3: pad.leftThumbstickButton().is_some_and(|b| b.isPressed()),
            r3: pad.rightThumbstickButton().is_some_and(|b| b.isPressed()),
            menu: pad.buttonMenu().isPressed(),
            options: pad.buttonOptions().is_some_and(|b| b.isPressed()),
            home: pad.buttonHome().is_some_and(|b| b.isPressed()),
            dpad_up: dpad.up().isPressed(),
            dpad_down: dpad.down().isPressed(),
            dpad_left: dpad.left().isPressed(),
            dpad_right: dpad.right().isPressed(),
        };
        (p, name)
    }
}

#[cfg(test)]
mod tests {
    use super::{Profile, to_pad};
    use gilrs::Button;

    #[test]
    fn buttons_dpad_and_flipped_stick() {
        let p = Profile {
            a: true,
            menu: true,
            dpad_up: true,
            ..Default::default()
        };
        let (held, axes) = to_pad(&p);
        assert_eq!(held, vec![Button::South, Button::Start, Button::DPadUp]);
        assert_eq!(
            (axes.lx, axes.ly, axes.lt, axes.hat_x),
            (0.0, 0.0, 0.0, 0.0)
        );
    }

    #[test]
    fn stick_y_flips_to_y_down_triggers_pass_through() {
        let p = Profile {
            ly: -0.7,
            rx: 0.2,
            lt_v: 0.9,
            ..Default::default()
        };
        let (_, axes) = to_pad(&p);
        assert_eq!((axes.lx, axes.ly), (0.0, 0.7));
        assert_eq!((axes.rx, axes.ry), (0.2, 0.0));
        assert_eq!(axes.lt, 0.9);
    }
}
