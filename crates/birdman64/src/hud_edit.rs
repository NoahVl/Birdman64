//! "Move HUD" (pause menu): the player drags the flight-HUD elements and
//! the performance overlay to where they want them. Same model as the touch
//! controls' edit mode (`touch.rs`): the game stays paused on its frame, a
//! drag moves one thing, Reset / Done at the top centre, Done saves.
//!
//! - HUD elements: ids tagged by the native game (`PW64_HUD_*` in
//!   `pw64_native.h`, renderer.md "Movable HUD"); offsets in N64 px
//!   ([`pw64_gfx::hud`]), saved as `[hud] <name> = [dx, dy]`. The paused
//!   frame is redrawn with the new offsets (`Renderer::fb_redraw`), so the
//!   real gauge moves under the finger. Bounds come from the renderer
//!   (`Renderer::fb_hud_bounds`), clamped there and here alike.
//! - Performance overlay: one more draggable thing; its position is a
//!   fraction of the window (`perf::Cfg::pos`, `[perf] pos`), which wins
//!   over the corner setting; picking a corner in the settings clears it.
//!
//! The state machine ([`Editor`]) is pure; [`Painter`] draws the outlines,
//! hint and buttons with egui.

use pw64_gfx::hud::{HUD_ELEMS, HudBounds, HudOffsets, clamp_offset};

/// The movable elements: id (must match `PW64_HUD_*` in the native game's
/// `pw64_native.h`), `[hud]` key, label.
pub const ELEMENTS: [(u8, &str, &str); 13] = [
    (1, "speed", "Speed"),
    (2, "timer", "Timer"),
    (3, "throttle", "Throttle"),
    (4, "fuel", "Fuel"),
    (5, "altimeter", "Altimeter"),
    (6, "radar", "Radar"),
    (7, "sea_level", "Altitude"),
    (8, "photos", "Photos"),
    (9, "messages", "Messages"),
    (10, "cb_power", "Power"),
    (11, "cb_heading", "Heading"),
    (12, "cb_elevation", "Elevation"),
    (13, "flaps", "Flaps"),
];

/// The id of a `[hud]` key.
pub fn element_id(name: &str) -> Option<u8> {
    ELEMENTS.iter().find(|e| e.1 == name).map(|e| e.0)
}

fn label(id: usize) -> &'static str {
    ELEMENTS
        .iter()
        .find(|e| e.0 as usize == id)
        .map_or("", |e| e.2)
}

/// Offsets from `[hud]` entries (unknown names skipped).
pub fn offsets_from(entries: &[(String, (f32, f32))]) -> HudOffsets {
    let mut o = [[0.0; 2]; HUD_ELEMS];
    for (name, (x, y)) in entries {
        if let Some(id) = element_id(name) {
            o[id as usize] = [*x, *y];
        }
    }
    o
}

/// The moved elements only, for `config::save_hud`.
pub fn to_config(o: &HudOffsets) -> Vec<(&'static str, (f32, f32))> {
    ELEMENTS
        .iter()
        .filter(|e| o[e.0 as usize] != [0.0; 2])
        .map(|e| (e.1, (o[e.0 as usize][0], o[e.0 as usize][1])))
        .collect()
}

/// `PW64_HUD_OFFSETS="speed=0,-40;radar=10,5"` (dev override, wins over
/// `[hud]`, `opts::hud_offsets`); bad entries are skipped with a note.
pub fn parse_env(s: &str) -> HudOffsets {
    let mut entries = Vec::new();
    for part in s.split(';').map(str::trim).filter(|p| !p.is_empty()) {
        let parsed = part.split_once('=').and_then(|(name, v)| {
            let (x, y) = v.split_once(',')?;
            let (x, y) = (x.trim().parse().ok()?, y.trim().parse().ok()?);
            element_id(name.trim())?;
            Some((name.trim().to_string(), (x, y)))
        });
        match parsed {
            Some(e) => entries.push(e),
            None => eprintln!("[hud] PW64_HUD_OFFSETS: skipped {part:?} (use name=dx,dy)"),
        }
    }
    offsets_from(&entries)
}

/// The edit mode's two buttons.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Leave the edit mode; the window saves.
    Done,
    /// Everything back to its default spot (the mode stays on).
    Reset,
}

/// Pointer phases (touch or left mouse button).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Start,
    Move,
    End,
}

/// What a drag moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// A HUD element (its id).
    Elem(usize),
    /// The performance overlay card.
    Perf,
}

/// Where things are on the window this frame, physical px.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Geometry {
    /// Window size.
    pub surface: (f32, f32),
    /// The game's output area (`[x0, y0, x1, y1]`): HUD elements stay in it.
    pub area: [f32; 4],
    /// Window px per N64 px (output area height / 240).
    pub k: f32,
    /// HUD element bounds drawn without offsets (window px).
    pub base: HudBounds,
    /// The performance overlay card, if shown.
    pub perf: Option<[f32; 4]>,
    /// px per dp (button sizes, hit slack).
    pub dp: f32,
}

impl Default for Geometry {
    fn default() -> Self {
        Self {
            surface: (0.0, 0.0),
            area: [0.0; 4],
            k: 0.0,
            base: [None; HUD_ELEMS],
            perf: None,
            dp: 1.0,
        }
    }
}

/// Rect helpers (`[x0, y0, x1, y1]`).
fn contains(r: [f32; 4], (x, y): (f32, f32), slack: f32) -> bool {
    x >= r[0] - slack && x <= r[2] + slack && y >= r[1] - slack && y <= r[3] + slack
}

fn overlaps(a: [f32; 4], b: [f32; 4]) -> bool {
    a[0] < b[2] && b[0] < a[2] && a[1] < b[3] && b[1] < a[3]
}

impl Geometry {
    /// Element `e` where it is drawn with offsets `o` (clamped like the
    /// renderer does).
    pub fn elem_rect(&self, e: usize, o: &HudOffsets) -> Option<[f32; 4]> {
        let b = self.base.get(e).copied().flatten()?;
        let [dx, dy] = clamp_offset(b, o[e], self.k, self.area).map(|v| (v * self.k).round());
        Some([b[0] + dx, b[1] + dy, b[2] + dx, b[3] + dy])
    }

    /// The perf card at `pos` (fractions; `None` = where it is now).
    fn perf_rect(&self, pos: Option<(f32, f32)>) -> Option<[f32; 4]> {
        let r = self.perf?;
        let Some((fx, fy)) = pos else {
            return Some(r);
        };
        let (w, h) = (r[2] - r[0], r[3] - r[1]);
        let (sw, sh) = self.surface;
        let x = (fx * sw).clamp(0.0, (sw - w).max(0.0));
        let y = (fy * sh).clamp(0.0, (sh - h).max(0.0));
        Some([x, y, x + w, y + h])
    }

    /// Reset / Done, top centre (`[x0, y0, x1, y1]`).
    pub fn buttons(&self) -> [(Action, [f32; 4]); 2] {
        let u = self.dp.max(0.5);
        let (cx, top) = (self.surface.0 / 2.0, 12.0 * u);
        let (hw, h) = (56.0 * u, 40.0 * u);
        let pill = |x: f32| [x - hw, top, x + hw, top + h];
        [
            (Action::Reset, pill(cx - 70.0 * u)),
            (Action::Done, pill(cx + 70.0 * u)),
        ]
    }

    fn covers_bar(&self, r: [f32; 4]) -> bool {
        self.buttons().iter().any(|(_, b)| overlaps(r, *b))
    }
}

/// One drag: who, what, where it started.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Drag {
    id: u64,
    target: Target,
    start: (f32, f32),
    /// The element's offset (N64 px) or the card's top-left (px) at the
    /// start.
    from: (f32, f32),
}

/// The edit mode's state.
#[derive(Clone, Debug, PartialEq)]
pub struct Editor {
    pub offsets: HudOffsets,
    /// `perf::Cfg::pos`.
    pub perf_pos: Option<(f32, f32)>,
    /// Geometry of the last drawn frame (pointer events use it).
    pub geo: Geometry,
    drag: Option<Drag>,
    /// Offsets or the card moved since the last [`Self::take_dirty`].
    dirty: bool,
}

impl Editor {
    pub fn new(offsets: HudOffsets, perf_pos: Option<(f32, f32)>) -> Self {
        Self {
            offsets,
            perf_pos,
            geo: Geometry::default(),
            drag: None,
            dirty: false,
        }
    }

    /// Something moved (the window redraws the paused frame).
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// What is being dragged (drawn highlighted).
    pub fn dragging(&self) -> Option<Target> {
        self.drag.map(|d| d.target)
    }

    /// Everything movable on screen now, with its rect (window px).
    pub fn targets(&self) -> Vec<(Target, [f32; 4])> {
        let mut v: Vec<_> = (1..HUD_ELEMS)
            .filter_map(|e| Some((Target::Elem(e), self.geo.elem_rect(e, &self.offsets)?)))
            .collect();
        if let Some(r) = self.geo.perf_rect(self.perf_pos) {
            v.push((Target::Perf, r));
        }
        v
    }

    /// The target under `p`: the perf card (drawn on top), else the
    /// smallest element whose rect (plus a little slack) holds `p`.
    fn hit(&self, p: (f32, f32)) -> Option<(Target, [f32; 4])> {
        let slack = 10.0 * self.geo.dp;
        let targets = self.targets();
        if let Some(&t) = targets
            .iter()
            .find(|(t, r)| *t == Target::Perf && contains(*r, p, 0.0))
        {
            return Some(t);
        }
        targets
            .into_iter()
            .filter(|(_, r)| contains(*r, p, slack))
            .min_by(|a, b| {
                let area = |r: [f32; 4]| (r[2] - r[0]) * (r[3] - r[1]);
                area(a.1).total_cmp(&area(b.1))
            })
    }

    /// One pointer event (window px). Reset / Done act on press; a press on
    /// a target drags it (one at a time).
    pub fn pointer(&mut self, id: u64, phase: Phase, p: (f32, f32)) -> Option<Action> {
        match phase {
            Phase::Start => {
                let button = self.geo.buttons().into_iter().find(|(_, r)| {
                    let slack = 4.0 * self.geo.dp;
                    contains(*r, p, slack)
                });
                if let Some((action, _)) = button {
                    self.drag = None;
                    if action == Action::Reset {
                        self.offsets = [[0.0; 2]; HUD_ELEMS];
                        self.perf_pos = None;
                        self.dirty = true;
                    }
                    return Some(action);
                }
                if self.drag.is_none()
                    && let Some((target, r)) = self.hit(p)
                {
                    let from = match target {
                        Target::Elem(e) => (self.offsets[e][0], self.offsets[e][1]),
                        Target::Perf => (r[0], r[1]),
                    };
                    self.drag = Some(Drag {
                        id,
                        target,
                        start: p,
                        from,
                    });
                }
            }
            Phase::Move => {
                if let Some(d) = self.drag.filter(|d| d.id == id) {
                    self.drag_to(d, p);
                }
            }
            Phase::End => {
                if self.drag.is_some_and(|d| d.id == id) {
                    self.drag = None;
                }
            }
        }
        None
    }

    /// Moves `d`'s target to follow the pointer at `p`: clamped on screen,
    /// and never newly under the Reset / Done bar (it could not be grabbed
    /// there again; the bar wins the hit test).
    fn drag_to(&mut self, d: Drag, p: (f32, f32)) {
        let (dx, dy) = (p.0 - d.start.0, p.1 - d.start.1);
        match d.target {
            Target::Elem(e) => {
                let Some(b) = self.geo.base[e] else { return };
                let k = self.geo.k;
                if k <= 0.0 {
                    return;
                }
                let before = self.geo.elem_rect(e, &self.offsets);
                let want = [d.from.0 + dx / k, d.from.1 + dy / k];
                let mut next = self.offsets;
                next[e] = clamp_offset(b, want, k, self.geo.area);
                let after = self.geo.elem_rect(e, &next);
                if let (Some(a), Some(bf)) = (after, before)
                    && self.geo.covers_bar(a)
                    && !self.geo.covers_bar(bf)
                {
                    return;
                }
                if next != self.offsets {
                    self.offsets = next;
                    self.dirty = true;
                }
            }
            Target::Perf => {
                let (sw, sh) = self.geo.surface;
                if sw < 1.0 || sh < 1.0 {
                    return;
                }
                let before = self.geo.perf_rect(self.perf_pos);
                let next = Some(((d.from.0 + dx) / sw, (d.from.1 + dy) / sh));
                // Stored clamped, so the card can't run away off screen.
                let Some(r) = self.geo.perf_rect(next) else {
                    return;
                };
                if let Some(bf) = before
                    && self.geo.covers_bar(r)
                    && !self.geo.covers_bar(bf)
                {
                    return;
                }
                let pos = Some((r[0] / sw, r[1] / sh));
                if pos != self.perf_pos {
                    self.perf_pos = pos;
                    self.dirty = true;
                }
            }
        }
    }
}

/// Draws the edit mode over the frame (egui, own pass, like the toasts).
pub struct Painter {
    ctx: egui::Context,
    renderer: Option<egui_wgpu::Renderer>,
    started: std::time::Instant,
}

impl Default for Painter {
    fn default() -> Self {
        Self::new()
    }
}

impl Painter {
    pub fn new() -> Self {
        Self {
            ctx: egui::Context::default(),
            renderer: None,
            started: std::time::Instant::now(),
        }
    }

    pub fn render(&mut self, gpu: &crate::window::Gpu, view: &wgpu::TextureView, e: &Editor) {
        let ppp = gpu.window.scale_factor() as f32;
        let raw = crate::settings::raw_input_zoom(
            gpu,
            ppp,
            1.0,
            self.started.elapsed().as_secs_f64(),
            Vec::new(),
        );
        let ctx = self.ctx.clone();
        let out = ctx.run(raw, |ctx| draw(ctx, ppp, e));
        crate::settings::paint(
            gpu,
            &self.ctx,
            &mut self.renderer,
            out,
            view,
            wgpu::LoadOp::Load,
        );
    }
}

/// Outlines around everything movable, the hint and Reset / Done, in egui
/// points (px / `ppp`).
fn draw(ctx: &egui::Context, ppp: f32, e: &Editor) {
    use egui::{Color32, Pos2, Rect, Stroke};
    let painter = ctx.layer_painter(egui::LayerId::background());
    let pt = |x: f32, y: f32| Pos2::new(x / ppp, y / ppp);
    let rect = |r: [f32; 4]| Rect::from_min_max(pt(r[0], r[1]), pt(r[2], r[3]));
    let fg = Color32::from_rgba_unmultiplied(255, 255, 255, 220);
    let targets = e.targets();
    for &(t, r) in &targets {
        let active = e.dragging() == Some(t);
        let r = rect(r).expand(3.0);
        let (fill, stroke) = if active {
            (
                Color32::from_rgba_unmultiplied(60, 120, 255, 60),
                Color32::from_rgb(90, 150, 255),
            )
        } else {
            (
                Color32::from_rgba_unmultiplied(255, 255, 255, 24),
                Color32::from_rgba_unmultiplied(255, 255, 255, 170),
            )
        };
        painter.rect(
            r,
            4.0,
            fill,
            Stroke::new(1.5_f32, stroke),
            egui::StrokeKind::Outside,
        );
        if active {
            let name = match t {
                Target::Elem(id) => label(id),
                Target::Perf => "Performance",
            };
            painter.text(
                r.center_top() - egui::vec2(0.0, 4.0),
                egui::Align2::CENTER_BOTTOM,
                name,
                egui::FontId::proportional(14.0),
                fg,
            );
        }
    }
    let (w, h) = e.geo.surface;
    let hint = if targets.is_empty() {
        "No HUD on screen: start a flight, then come back here"
    } else {
        "Drag the HUD to move it"
    };
    let bar = e.geo.buttons();
    let hint_y = (bar[0].1[3] + 16.0 * e.geo.dp).min(h * 0.4);
    painter.text(
        pt(w / 2.0, hint_y),
        egui::Align2::CENTER_TOP,
        hint,
        egui::FontId::proportional(16.0),
        fg,
    );
    for (action, r) in bar {
        let r = rect(r);
        let fill = match action {
            Action::Done => Color32::from_rgba_unmultiplied(40, 90, 230, 230),
            Action::Reset => Color32::from_rgba_unmultiplied(90, 90, 100, 230),
        };
        painter.rect_filled(r, r.height() / 2.0, fill);
        painter.text(
            r.center(),
            egui::Align2::CENTER_CENTER,
            match action {
                Action::Done => "Done",
                Action::Reset => "Reset",
            },
            egui::FontId::proportional(r.height() * 0.45),
            fg,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1280×720 window, 4:3 output area centred (160..1120), k = 3.
    fn geo() -> Geometry {
        let mut base = [None; HUD_ELEMS];
        base[1] = Some([200.0, 600.0, 300.0, 660.0]); // speed, bottom left
        base[6] = Some([900.0, 40.0, 1100.0, 200.0]); // radar, top right
        Geometry {
            surface: (1280.0, 720.0),
            area: [160.0, 0.0, 1120.0, 720.0],
            k: 3.0,
            base,
            perf: Some([1000.0, 300.0, 1260.0, 380.0]),
            dp: 1.0,
        }
    }

    fn editor() -> Editor {
        let mut e = Editor::new([[0.0; 2]; HUD_ELEMS], None);
        e.geo = geo();
        e
    }

    #[test]
    fn names_ids_and_config_round_trip() {
        let mut ids: Vec<u8> = ELEMENTS.iter().map(|e| e.0).collect();
        ids.dedup();
        assert_eq!(ids.len(), ELEMENTS.len());
        assert!(ids.iter().all(|&i| i > 0 && (i as usize) < HUD_ELEMS));
        assert_eq!(element_id("radar"), Some(6));
        assert_eq!(element_id("nope"), None);
        let o = offsets_from(&[("speed".into(), (0.0, -40.0)), ("bogus".into(), (1.0, 1.0))]);
        assert_eq!(o[1], [0.0, -40.0]);
        assert_eq!(to_config(&o), vec![("speed", (0.0, -40.0))]);
        let env = parse_env("speed=0,-40; radar = 10,5;junk;fuel=x,1");
        assert_eq!(env[1], [0.0, -40.0]);
        assert_eq!(env[6], [10.0, 5.0]);
        assert_eq!(env[4], [0.0, 0.0]);
    }

    #[test]
    fn drag_moves_an_element_in_n64_px_and_clamps() {
        let mut e = editor();
        assert_eq!(e.pointer(1, Phase::Start, (250.0, 630.0)), None);
        assert_eq!(e.dragging(), Some(Target::Elem(1)));
        e.pointer(1, Phase::Move, (250.0, 330.0)); // 300 px up = 100 N64 px
        assert_eq!(e.offsets[1], [0.0, -100.0]);
        assert!(e.take_dirty() && !e.take_dirty());
        // Far down: clamped at the bottom edge (60 px = 20 N64 px).
        e.pointer(1, Phase::Move, (250.0, 2000.0));
        assert_eq!(e.offsets[1], [0.0, 20.0]);
        // Back: follows again at once (start-relative, no stored excess).
        e.pointer(1, Phase::Move, (250.0, 600.0));
        assert_eq!(e.offsets[1], [0.0, -10.0]);
        // Left past the output area's edge (40 px of room).
        e.pointer(1, Phase::Move, (-500.0, 600.0));
        assert!((e.offsets[1][0] + 40.0 / 3.0).abs() < 1e-4);
        e.pointer(1, Phase::End, (0.0, 0.0));
        assert_eq!(e.dragging(), None);
        // A second finger can't grab while one drags; other ids ignored.
        e.pointer(2, Phase::Start, (1000.0, 100.0));
        e.pointer(3, Phase::Start, (250.0, 600.0));
        e.pointer(3, Phase::Move, (250.0, 0.0));
        assert_eq!(e.dragging(), Some(Target::Elem(6)));
        assert!((e.offsets[1][0] + 40.0 / 3.0).abs() < 1e-4);
    }

    #[test]
    fn elements_never_slide_under_the_buttons() {
        let mut e = editor();
        // Radar (top right) dragged left into the Reset / Done bar.
        e.pointer(1, Phase::Start, (1000.0, 100.0));
        e.pointer(1, Phase::Move, (700.0, 100.0));
        let r = e.geo.elem_rect(6, &e.offsets).unwrap();
        assert!(!e.geo.covers_bar(r), "held back before the bar");
    }

    #[test]
    fn perf_card_drags_as_a_fraction_and_stays_on_screen() {
        let mut e = editor();
        // The card is on top of everything (hit first).
        e.pointer(1, Phase::Start, (1100.0, 340.0));
        assert_eq!(e.dragging(), Some(Target::Perf));
        e.pointer(1, Phase::Move, (900.0, 440.0));
        let (fx, fy) = e.perf_pos.unwrap();
        assert!((fx * 1280.0 - 800.0).abs() < 1e-3 && (fy * 720.0 - 400.0).abs() < 1e-3);
        // Off the right / bottom edge: stored clamped.
        e.pointer(1, Phase::Move, (5000.0, 5000.0));
        let (fx, fy) = e.perf_pos.unwrap();
        assert!((fx * 1280.0 - 1020.0).abs() < 1e-3 && (fy * 720.0 - 640.0).abs() < 1e-3);
        e.pointer(1, Phase::End, (0.0, 0.0));
    }

    #[test]
    fn reset_and_done() {
        let mut e = editor();
        e.offsets[1] = [5.0, -30.0];
        e.perf_pos = Some((0.1, 0.1));
        let center = |r: [f32; 4]| ((r[0] + r[2]) / 2.0, (r[1] + r[3]) / 2.0);
        let [(_, reset), (_, done)] = e.geo.buttons();
        assert_eq!(
            e.pointer(1, Phase::Start, center(reset)),
            Some(Action::Reset)
        );
        assert_eq!(e.offsets, [[0.0; 2]; HUD_ELEMS]);
        assert_eq!(e.perf_pos, None);
        assert!(e.take_dirty());
        assert_eq!(e.pointer(2, Phase::Start, center(done)), Some(Action::Done));
    }

    #[test]
    fn nothing_on_screen_means_no_targets() {
        let mut e = Editor::new([[0.0; 2]; HUD_ELEMS], None);
        e.geo.surface = (800.0, 600.0);
        assert!(e.targets().is_empty());
        assert_eq!(e.pointer(1, Phase::Start, (400.0, 300.0)), None);
        assert_eq!(e.dragging(), None);
    }
}
