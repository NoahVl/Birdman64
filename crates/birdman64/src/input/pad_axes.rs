//! Mobile pad axes: the [`Axes`] state `mobile_pad` publishes, and the
//! Android mapping from raw `AMOTION_EVENT_AXIS_*` values to it. winit
//! reports a joystick-class MotionEvent (gamepad sticks, analog triggers, a
//! hat d-pad) as one `WindowEvent::AxisMotion` per axis, with the Android
//! axis id as `axis`. Pure, so it is unit-tested on desktop.

use gilrs::Button;

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

/// Android axis ids (android/input.h `AMOTION_EVENT_AXIS_*`).
const AXIS_X: u32 = 0;
const AXIS_Y: u32 = 1;
const AXIS_Z: u32 = 11;
const AXIS_RZ: u32 = 14;
const AXIS_HAT_X: u32 = 15;
const AXIS_HAT_Y: u32 = 16;
const AXIS_LTRIGGER: u32 = 17;
const AXIS_RTRIGGER: u32 = 18;
const AXIS_GAS: u32 = 22;
const AXIS_BRAKE: u32 = 23;
const AXIS_COUNT: usize = 24;

/// The latest value of every Android axis a pad reports.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
#[derive(Clone, Copy, Debug, Default)]
pub struct RawAxes([f32; AXIS_COUNT]);

#[cfg_attr(not(target_os = "android"), allow(dead_code))]
impl RawAxes {
    /// All axes at rest (`const`, for a static).
    pub const fn new() -> Self {
        Self([0.0; AXIS_COUNT])
    }

    /// Stores one axis value (ids past the table are ignored).
    pub fn set(&mut self, axis: u32, value: f32) {
        if let Some(v) = self.0.get_mut(axis as usize) {
            *v = value;
        }
    }

    /// The pad's [`Axes`]. Right stick = Z/RZ, Android's gamepad convention
    /// (its key layouts map Xbox, DualSense and handheld pads there; RX/RY
    /// are left alone, as some generic pads put triggers on them). A trigger
    /// is LTRIGGER/RTRIGGER or BRAKE/GAS, whichever is further down: pads
    /// report one pair or both.
    pub fn axes(&self) -> Axes {
        let v = |axis: u32| self.0[axis as usize];
        let trigger = |a: u32, b: u32| v(a).max(v(b)).clamp(0.0, 1.0);
        Axes {
            lx: v(AXIS_X),
            ly: v(AXIS_Y),
            rx: v(AXIS_Z),
            ry: v(AXIS_RZ),
            lt: trigger(AXIS_LTRIGGER, AXIS_BRAKE),
            rt: trigger(AXIS_RTRIGGER, AXIS_GAS),
            hat_x: v(AXIS_HAT_X),
            hat_y: v(AXIS_HAT_Y),
        }
    }
}

/// The buttons the axes hold, the way gilrs reports them on desktop: an
/// analog trigger past `threshold` is LT2/RT2, the hat is the d-pad (y
/// down). Pad capture binds these, as it does a desktop pad's.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub fn axis_buttons(a: &Axes, threshold: f32) -> Vec<Button> {
    [
        (a.lt >= threshold, Button::LeftTrigger2),
        (a.rt >= threshold, Button::RightTrigger2),
        (a.hat_y <= -0.5, Button::DPadUp),
        (a.hat_y >= 0.5, Button::DPadDown),
        (a.hat_x <= -0.5, Button::DPadLeft),
        (a.hat_x >= 0.5, Button::DPadRight),
    ]
    .into_iter()
    .filter_map(|(down, b)| down.then_some(b))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sticks_triggers_hat() {
        let mut raw = RawAxes::default();
        raw.set(AXIS_X, 0.5);
        raw.set(AXIS_Y, -1.0);
        raw.set(AXIS_Z, 0.25);
        raw.set(AXIS_RZ, -0.75);
        raw.set(AXIS_HAT_X, -1.0);
        raw.set(AXIS_HAT_Y, 1.0);
        raw.set(99, 1.0); // unknown id: ignored
        let a = raw.axes();
        assert_eq!((a.lx, a.ly, a.rx, a.ry), (0.5, -1.0, 0.25, -0.75));
        assert_eq!((a.hat_x, a.hat_y), (-1.0, 1.0));
        assert_eq!((a.lt, a.rt), (0.0, 0.0));
    }

    #[test]
    fn trigger_pairs() {
        let mut raw = RawAxes::default();
        // BRAKE/GAS only (a generic HID pad).
        raw.set(AXIS_BRAKE, 0.8);
        raw.set(AXIS_GAS, 0.4);
        assert_eq!((raw.axes().lt, raw.axes().rt), (0.8, 0.4));
        // Both pairs: the further one wins; out-of-range values clamp.
        raw.set(AXIS_LTRIGGER, 0.9);
        raw.set(AXIS_RTRIGGER, 1.5);
        assert_eq!((raw.axes().lt, raw.axes().rt), (0.9, 1.0));
    }

    #[test]
    fn buttons_from_axes() {
        let a = Axes {
            lt: 0.5,
            rt: 0.1,
            hat_x: 1.0,
            hat_y: -1.0,
            ..Axes::default()
        };
        assert_eq!(
            axis_buttons(&a, 0.3),
            [Button::LeftTrigger2, Button::DPadUp, Button::DPadRight]
        );
        assert!(axis_buttons(&Axes::default(), 0.3).is_empty());
    }
}
