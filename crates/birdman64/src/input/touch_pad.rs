//! The on-screen touch controls ([`crate::touch`]) as an N64 pad source
//! (docs/notes/input.md): the window reports every touch state here and the
//! bits are merged into controller 1 like any other source. Both platforms'
//! other pad sources live in `mobile_pad` (Android pad keys, iOS
//! GameController pads), which owns the publish — its keys/pad and the
//! touch land in one pad — and reads this state on every republish; the
//! settings overlay itself is driven by the window's touch-as-pointer path,
//! so it needs no nav pad.

use crate::touch::TouchPad;
use std::sync::Mutex;

/// The touch controls' latest state (window thread, every touch event).
/// Lock order: this mutex is taken and released before `SOURCES` (as
/// `mobile_pad`'s STATE is).
static TOUCH: Mutex<TouchPad> = Mutex::new(TouchPad::IDLE);

/// The touch state, as the pad sources merge it into controller 1.
pub fn touch() -> TouchPad {
    *TOUCH.lock().unwrap()
}

/// Drops the touch state without publishing: callers that clear every
/// source (`mobile_pad::release_all`) republish the whole pad themselves.
pub fn clear() {
    *TOUCH.lock().unwrap() = TouchPad::IDLE;
}

/// The touch controls' state changed (window thread, every touch event):
/// store it and republish controller 1.
pub fn set_touch(touch: TouchPad) {
    let changed = {
        let mut t = TOUCH.lock().unwrap();
        let changed = *t != touch;
        *t = touch;
        changed
    };
    if changed {
        // One publish covers the platform's pad state, axes and touch.
        super::mobile_pad::republish();
    }
}
