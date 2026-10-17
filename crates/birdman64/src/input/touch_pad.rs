//! The on-screen touch controls ([`crate::touch`]) as an N64 pad source
//! (docs/notes/input.md): the window reports every touch state here and the
//! bits are merged into controller 1 like any other source. Android:
//! `android_pad` owns the publish (its keys, axes and touch land in one
//! pad) and reads [`touch`]; iOS: touch is the only pad source (no gilrs,
//! no Android keycodes), so this publishes on its own — the settings
//! overlay itself is driven by the window's touch-as-pointer path, so it
//! needs no nav pad.

use crate::touch::TouchPad;
use std::sync::Mutex;

/// The touch controls' latest state (window thread, every touch event).
/// Lock order: this mutex is taken and released before `SOURCES` (as
/// `android_pad`'s STATE is).
static TOUCH: Mutex<TouchPad> = Mutex::new(TouchPad::IDLE);

/// The touch state, as the pad sources merge it into controller 1.
pub fn touch() -> TouchPad {
    *TOUCH.lock().unwrap()
}

/// Drops the touch state without publishing: callers that clear every
/// source (`android_pad::release_all`, `input::release_all_keys` on iOS)
/// republish the whole pad themselves.
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
        // Android: one publish covers keys, axes and touch.
        #[cfg(target_os = "android")]
        super::android_pad::republish();
        // iOS: touch is the only pad source for now.
        #[cfg(target_os = "ios")]
        publish_ios();
    }
}

/// iOS only: controller 1 from the touch state alone.
#[cfg(target_os = "ios")]
fn publish_ios() {
    use super::{Pad, SOURCES, publish, stick_to_n64};
    let touch = *TOUCH.lock().unwrap();
    let (stick_x, stick_y) = stick_to_n64(touch.x, touch.y);
    let pad = Pad {
        button: touch.buttons,
        stick_x,
        stick_y,
    };
    let mut s = SOURCES.lock().unwrap();
    s.gamepads = vec![(0, "Touch controls".to_string(), pad)];
    publish(&mut s);
}

/// Window focus lost (iOS): drop the touch state and publish an idle pad —
/// the touch events of the fingers that were down never come. Called by
/// `input::release_all_keys` before it locks `SOURCES` (lock order: this
/// `TOUCH`, then `SOURCES`).
#[cfg(target_os = "ios")]
pub(crate) fn release_all() {
    clear();
    publish_ios();
}
