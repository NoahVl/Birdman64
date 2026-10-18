//! Movable flight-HUD elements (renderer.md "Movable HUD").
//!
//! The native game tags each HUD gauge with an element id (`G_NOOP` "PWE",
//! [`crate::interp::HUD_ELEM_TAG`]); the renderer moves those draws by a
//! per-element offset ([`crate::RenderOptions::hud_offsets`]) and reports
//! each element's on-screen bounds per framebuffer
//! ([`crate::Renderer::hud_bounds`]) so a window can hit-test and drag
//! them.
//!
//! Offsets are in N64 pixels of the 4:3 area (target px per unit =
//! output height / 240), relative to the element's anchored spot: they
//! survive resolution and scale changes, and with widescreen/fill an
//! element keeps its edge anchor and moves from there. The renderer clamps
//! them so the element stays inside the output area (a narrower aspect
//! later can't push it off screen).

/// Element id slots (id 0 = not movable).
pub const HUD_ELEMS: usize = 16;

/// Per element id: (dx, dy) in N64 px (y down).
pub type HudOffsets = [[f32; 2]; HUD_ELEMS];

/// Per element id: its bounds `[x0, y0, x1, y1]` in target px as drawn
/// with no offset (`None`: not drawn).
pub type HudBounds = [Option<[f32; 4]>; HUD_ELEMS];

/// `off` (N64 px) clamped so `base` (target px, drawn without offset)
/// moved by `off · k` (`k` = target px per N64 px) stays inside `area`
/// (`[x0, y0, x1, y1]`, target px). An axis on which the element is larger
/// than the area gets 0.
pub fn clamp_offset(base: [f32; 4], off: [f32; 2], k: f32, area: [f32; 4]) -> [f32; 2] {
    if k <= 0.0 {
        return [0.0; 2];
    }
    let axis = |v: f32, lo: f32, hi: f32| {
        if lo > hi {
            0.0
        } else {
            (v * k).clamp(lo, hi) / k
        }
    };
    [
        axis(off[0], area[0] - base[0], area[2] - base[2]),
        axis(off[1], area[1] - base[1], area[3] - base[3]),
    ]
}

/// Union of two rects (`[x0, y0, x1, y1]`).
pub(crate) fn union(a: Option<[f32; 4]>, b: [f32; 4]) -> [f32; 4] {
    match a {
        Some(a) => [
            a[0].min(b[0]),
            a[1].min(b[1]),
            a[2].max(b[2]),
            a[3].max(b[3]),
        ],
        None => b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_keeps_elements_inside() {
        let area = [0.0, 0.0, 1280.0, 720.0];
        let base = [40.0, 600.0, 140.0, 660.0]; // a bottom-left gauge
        let k = 3.0; // 720 / 240
        assert_eq!(clamp_offset(base, [0.0, 0.0], k, area), [0.0, 0.0]);
        assert_eq!(clamp_offset(base, [10.0, -50.0], k, area), [10.0, -50.0]);
        // 100 N64 px down = 300 px: only 60 px of room.
        assert_eq!(clamp_offset(base, [0.0, 100.0], k, area), [0.0, 20.0]);
        // Left: 40 px of room.
        let c = clamp_offset(base, [-100.0, 0.0], k, area);
        assert!((c[0] + 40.0 / 3.0).abs() < 1e-4);
        // Too big for the area: that axis stays put.
        let wide = [-10.0, 0.0, 1300.0, 10.0];
        assert_eq!(clamp_offset(wide, [5.0, 5.0], k, area)[0], 0.0);
        assert_eq!(clamp_offset(base, [5.0, 5.0], 0.0, area), [0.0, 0.0]);
    }

    #[test]
    fn union_grows() {
        let a = union(None, [1.0, 2.0, 3.0, 4.0]);
        assert_eq!(a, [1.0, 2.0, 3.0, 4.0]);
        assert_eq!(union(Some(a), [0.0, 3.0, 2.0, 9.0]), [0.0, 2.0, 3.0, 9.0]);
    }
}
