//! On-screen touch controls (Android): a floating analog stick on the left,
//! A/B/Z/R, the four C buttons and Start on the right, and a menu button
//! that opens the settings overlay. The result is one more N64 pad source
//! ([`TouchPad`]), merged into controller 1 by `input/android_pad.rs`, so
//! the game sees a normal controller.
//!
//! Layout, hit-testing and the multi-touch state machine are pure (unit
//! tests run on desktop); only [`Painter`] (egui, own pass after the
//! toasts and the `PW64_WIN_SHOT` readback) is Android-only. PNG frame
//! dumps use the game thread's own renderer and never see the controls.
//!
//! Stick: **floating**. A finger that lands in the left part of the screen
//! (and not on a button) puts the stick base under it; dragging moves the
//! knob up to one base radius, the vector feeding `input::stick_to_n64`
//! (same radial deadzone and octagon gate as a real pad). When idle the
//! base is drawn at a fixed rest spot as a hint. Floating avoids the
//! "missed the stick" problem on phones of every size.
//!
//! L is left out: the game reads it only for a demo-HUD indicator.
//!
//! Edit mode (pause menu "Move touch controls"): the game stays paused,
//! a drag moves a control ([`Group`]; the C diamond as one), Reset / Done
//! at the top. [`Offsets`] are fractions of the screen, saved as `[touch]`
//! in pw64.toml.
//! TODO(android): opacity and size sliders in the settings screen
//! ([`OPACITY`], the `scale` argument of [`Layout::new`]).

#![cfg_attr(not(target_os = "android"), allow(dead_code))]

use crate::input::{
    CONT_A, CONT_B, CONT_C_DOWN, CONT_C_LEFT, CONT_C_RIGHT, CONT_C_UP, CONT_R, CONT_START, CONT_Z,
};

/// Fill opacity of the controls (pressed ones draw brighter).
pub const OPACITY: f32 = 0.35;

/// The player's "Touch buttons: only menu" setting (pause menu, saved as
/// `[touch] hidden`); the window copies it into [`Controls`] each frame.
static HIDDEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn hidden_setting() -> bool {
    HIDDEN.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn set_hidden_setting(hidden: bool) {
    HIDDEN.store(hidden, std::sync::atomic::Ordering::Relaxed);
}

/// The touch controls' N64 state: button bits and the stick (-1..1, y up).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TouchPad {
    pub buttons: u16,
    pub x: f32,
    pub y: f32,
}

impl TouchPad {
    pub const IDLE: TouchPad = TouchPad {
        buttons: 0,
        x: 0.0,
        y: 0.0,
    };
}

/// One on-screen button.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    A,
    B,
    Z,
    R,
    CUp,
    CDown,
    CLeft,
    CRight,
    Start,
    /// Opens the settings overlay (never reaches the game).
    Menu,
}

impl Control {
    /// The N64 button bit (0 for the menu button).
    pub fn bit(self) -> u16 {
        match self {
            Control::A => CONT_A,
            Control::B => CONT_B,
            Control::Z => CONT_Z,
            Control::R => CONT_R,
            Control::CUp => CONT_C_UP,
            Control::CDown => CONT_C_DOWN,
            Control::CLeft => CONT_C_LEFT,
            Control::CRight => CONT_C_RIGHT,
            Control::Start => CONT_START,
            Control::Menu => 0,
        }
    }

    /// A finger sliding across the cluster moves between these (A ↔ B, the
    /// C buttons); Start and Menu need their own press.
    fn slides(self) -> bool {
        !matches!(self, Control::Start | Control::Menu)
    }
}

/// What the player can move in the edit mode, each with its own offset:
/// the stick's rest spot, the single buttons, and the C diamond as a unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    Stick,
    A,
    B,
    Z,
    R,
    C,
    Start,
    Menu,
}

impl Group {
    pub const ALL: [Group; 8] = [
        Group::Stick,
        Group::A,
        Group::B,
        Group::Z,
        Group::R,
        Group::C,
        Group::Start,
        Group::Menu,
    ];

    /// The `[touch]` key in pw64.toml.
    pub fn name(self) -> &'static str {
        match self {
            Group::Stick => "stick",
            Group::A => "a",
            Group::B => "b",
            Group::Z => "z",
            Group::R => "r",
            Group::C => "c",
            Group::Start => "start",
            Group::Menu => "menu",
        }
    }

    fn of(c: Control) -> Group {
        match c {
            Control::A => Group::A,
            Control::B => Group::B,
            Control::Z => Group::Z,
            Control::R => Group::R,
            Control::CUp | Control::CDown | Control::CLeft | Control::CRight => Group::C,
            Control::Start => Group::Start,
            Control::Menu => Group::Menu,
        }
    }

    fn index(self) -> usize {
        Group::ALL.iter().position(|&g| g == self).unwrap_or(0)
    }
}

/// The player's moves: per [`Group`], (dx, dy) as fractions of the surface
/// width / height, so they survive a resolution or density change.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Offsets([(f32, f32); 8]);

impl Offsets {
    /// From the `[touch]` table (unknown names are skipped).
    pub fn from_config(entries: &[(String, (f32, f32))]) -> Offsets {
        let mut o = Offsets::default();
        for (name, v) in entries {
            if let Some(g) = Group::ALL.iter().find(|g| g.name() == name) {
                o.0[g.index()] = *v;
            }
        }
        o
    }

    /// The moved groups only, for `config::save_touch`.
    pub fn to_config(self) -> Vec<(&'static str, (f32, f32))> {
        Group::ALL
            .iter()
            .filter(|g| self.get(**g) != (0.0, 0.0))
            .map(|g| (g.name(), self.get(*g)))
            .collect()
    }

    pub fn get(&self, g: Group) -> (f32, f32) {
        self.0[g.index()]
    }

    fn set(&mut self, g: Group, v: (f32, f32)) {
        self.0[g.index()] = v;
    }
}

/// The edit mode's two buttons (top centre).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EditAction {
    /// Leave the edit mode; the window saves the offsets.
    #[default]
    Done,
    /// Every control back to its default spot (the mode stays on).
    Reset,
}

/// A rounded-rectangle button: center and half extents, physical px.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Pill {
    pub x: f32,
    pub y: f32,
    pub half_w: f32,
    pub half_h: f32,
}

impl Pill {
    fn contains(&self, (x, y): (f32, f32)) -> bool {
        (x - self.x).abs() <= self.half_w * HIT_SLACK
            && (y - self.y).abs() <= self.half_h * HIT_SLACK
    }
}

/// A round control: center and radius, physical px.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Circle {
    pub x: f32,
    pub y: f32,
    pub r: f32,
}

impl Circle {
    fn dist2(&self, (x, y): (f32, f32)) -> f32 {
        (x - self.x).powi(2) + (y - self.y).powi(2)
    }
}

/// Insets of the area the controls must stay inside (system bars, display
/// cutouts), physical px.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Insets {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

/// Where everything sits for one surface size. Physical px throughout.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Layout {
    /// The idle stick base (drawn as a hint; a touch moves it).
    pub stick_rest: (f32, f32),
    /// Base radius = full knob travel.
    pub stick_radius: f32,
    /// A touch left of this (not on a button) grabs the stick.
    pub stick_zone_right: f32,
    pub buttons: Vec<(Control, Circle)>,
    /// Surface size (offsets are fractions of it).
    pub size: (f32, f32),
    /// The edit mode's Reset / Done buttons.
    pub edit_buttons: [(EditAction, Pill); 2],
}

/// Minimum distance from the screen edges, dp. Horizontal is larger: in
/// landscape a notch or punch-hole sits on a short edge.
const EDGE_X_DP: f32 = 36.0;
const EDGE_Y_DP: f32 = 12.0;
/// Gap between an inset (cutout, system bar) and the controls, dp.
const INSET_GAP_DP: f32 = 8.0;
/// Hit radius as a multiple of the drawn radius (fingers are imprecise).
const HIT_SLACK: f32 = 1.2;

impl Layout {
    /// The layout for a `w`×`h` px surface with `dp` px per dp (winit's
    /// scale factor = Android's display density), `insets` kept clear and
    /// `scale` the player's size factor (1.0 = default). Sizes are in dp,
    /// shrunk on small screens so the right cluster fits in landscape.
    pub fn new(w: f32, h: f32, dp: f32, insets: Insets, scale: f32) -> Layout {
        if w < 1.0 || h < 1.0 || dp <= 0.0 {
            return Layout::default();
        }
        let (w_dp, h_dp) = (w / dp, h / dp);
        // The right column (R down to A) is ~300 units tall plus the menu
        // button: on short surfaces (split screen) that height decides.
        let avail_dp =
            h_dp - insets.top.max(0.0) / dp - insets.bottom.max(0.0) / dp - 2.0 * EDGE_Y_DP;
        let fit = (h_dp / 380.0)
            .min(w_dp / 700.0)
            .min(avail_dp / 340.0)
            .clamp(0.3, 1.3);
        let u = dp * fit * scale.clamp(0.5, 2.0);
        // The safe rectangle: the edge margins, or the insets (cutout,
        // bars) plus a small gap where those are larger.
        let edge =
            |inset: f32, margin_dp: f32| (inset.max(0.0) + INSET_GAP_DP * dp).max(margin_dp * dp);
        let left = edge(insets.left, EDGE_X_DP);
        let right = w - edge(insets.right, EDGE_X_DP);
        let top = edge(insets.top, EDGE_Y_DP);
        let bottom = h - edge(insets.bottom, EDGE_Y_DP);
        let c = |x: f32, y: f32, r: f32| Circle { x, y, r: r * u };
        let stick_radius = 56.0 * u;
        let stick_rest = (left + stick_radius, bottom - stick_radius - 12.0 * u);
        // Right cluster, N64-like: A low, B up-left of it, the C diamond
        // above-right, R above the diamond.
        let (cx, cy, cd) = (right - 62.0 * u, bottom - 170.0 * u, 40.0 * u);
        let buttons = vec![
            (Control::A, c(right - 104.0 * u, bottom - 44.0 * u, 34.0)),
            (Control::B, c(right - 186.0 * u, bottom - 92.0 * u, 30.0)),
            (Control::CUp, c(cx, cy - cd, 22.0)),
            (Control::CDown, c(cx, cy + cd, 22.0)),
            (Control::CLeft, c(cx - cd, cy, 22.0)),
            (Control::CRight, c(cx + cd, cy, 22.0)),
            (Control::R, c(right - 34.0 * u, cy - cd - 64.0 * u, 26.0)),
            // Z: the N64's trigger under the stick hand, here above the
            // stick's rest spot.
            (
                Control::Z,
                c(
                    left + 30.0 * u,
                    stick_rest.1 - stick_radius - 52.0 * u,
                    28.0,
                ),
            ),
            (Control::Start, c(w / 2.0, bottom - 20.0 * u, 20.0)),
            (Control::Menu, c(left + 18.0 * u, top + 18.0 * u, 18.0)),
        ];
        let pill = |x: f32| Pill {
            x,
            y: top + 24.0 * u,
            half_w: 56.0 * u,
            half_h: 20.0 * u,
        };
        Layout {
            stick_rest,
            stick_radius,
            stick_zone_right: w * 0.45,
            buttons,
            size: (w, h),
            edit_buttons: [
                (EditAction::Reset, pill(w / 2.0 - 70.0 * u)),
                (EditAction::Done, pill(w / 2.0 + 70.0 * u)),
            ],
        }
    }

    /// This (default) layout with the player's offsets applied. Each group
    /// moves as a unit and is clamped to stay fully on screen; the clamped
    /// offsets come back too (an edit drag then can't run away).
    pub fn with_offsets(&self, o: &Offsets) -> (Layout, Offsets) {
        let (w, h) = self.size;
        let mut out = self.clone();
        let mut clamped = Offsets::default();
        if w < 1.0 || h < 1.0 {
            return (out, clamped);
        }
        for g in Group::ALL {
            let members: Vec<Circle> = if g == Group::Stick {
                vec![Circle {
                    x: self.stick_rest.0,
                    y: self.stick_rest.1,
                    r: self.stick_radius,
                }]
            } else {
                self.buttons
                    .iter()
                    .filter(|(c, _)| Group::of(*c) == g)
                    .map(|(_, c)| *c)
                    .collect()
            };
            if members.is_empty() {
                continue;
            }
            // Allowed shift range in px: every member stays inside.
            let (mut x0, mut x1, mut y0, mut y1) = (f32::MIN, f32::MAX, f32::MIN, f32::MAX);
            for c in &members {
                x0 = x0.max(c.r - c.x);
                x1 = x1.min(w - c.r - c.x);
                y0 = y0.max(c.r - c.y);
                y1 = y1.min(h - c.r - c.y);
            }
            let (fx, fy) = o.get(g);
            let clampf = |v: f32, lo: f32, hi: f32| if lo > hi { 0.0 } else { v.clamp(lo, hi) };
            let (dx, dy) = (clampf(fx * w, x0, x1), clampf(fy * h, y0, y1));
            clamped.set(g, (dx / w, dy / h));
            if g == Group::Stick {
                out.stick_rest = (self.stick_rest.0 + dx, self.stick_rest.1 + dy);
                // A rest spot moved right keeps a grab zone around it.
                out.stick_zone_right = self
                    .stick_zone_right
                    .max(out.stick_rest.0 + self.stick_radius);
            } else {
                for (c, circle) in &mut out.buttons {
                    if Group::of(*c) == g {
                        circle.x += dx;
                        circle.y += dy;
                    }
                }
            }
        }
        (out, clamped)
    }

    /// Does a member of `g` overlap the edit mode's Reset / Done buttons?
    fn covers_edit_bar(&self, g: Group) -> bool {
        let members: Vec<Circle> = if g == Group::Stick {
            vec![Circle {
                x: self.stick_rest.0,
                y: self.stick_rest.1,
                r: self.stick_radius,
            }]
        } else {
            self.buttons
                .iter()
                .filter(|(c, _)| Group::of(*c) == g)
                .map(|(_, c)| *c)
                .collect()
        };
        members.iter().any(|c| {
            self.edit_buttons.iter().any(|(_, p)| {
                let dx = ((c.x - p.x).abs() - p.half_w * HIT_SLACK).max(0.0);
                let dy = ((c.y - p.y).abs() - p.half_h * HIT_SLACK).max(0.0);
                dx * dx + dy * dy < c.r * c.r
            })
        })
    }

    /// The edit mode's target under `p`: a button's group, else the stick
    /// base at its rest spot.
    fn hit_group(&self, p: (f32, f32)) -> Option<Group> {
        if let Some(c) = self.hit(p) {
            return Some(Group::of(c));
        }
        let rest = Circle {
            x: self.stick_rest.0,
            y: self.stick_rest.1,
            r: self.stick_radius,
        };
        (rest.dist2(p) <= rest.r.powi(2)).then_some(Group::Stick)
    }

    /// The button under `p` (within its hit radius; the nearest center wins
    /// where hit areas overlap).
    pub fn hit(&self, p: (f32, f32)) -> Option<Control> {
        self.buttons
            .iter()
            .map(|(ctl, c)| (*ctl, c.dist2(p), (c.r * HIT_SLACK).powi(2)))
            .filter(|&(_, d2, r2)| d2 <= r2)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(ctl, _, _)| ctl)
    }
}

/// Touch phases as the window reports them (winit `TouchPhase`, with
/// Cancelled folded into End).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Start,
    Move,
    End,
}

/// What one finger is doing.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Finger {
    /// Holding the stick: base at `origin`, knob toward `pos`.
    Stick { origin: (f32, f32), pos: (f32, f32) },
    /// On a button (None: slid off every button).
    Button(Option<Control>),
    /// Landed on nothing (or the menu button): ignored until lifted.
    Ignored,
}

/// The touch controls' state: layout, visibility and the fingers down.
#[derive(Debug)]
pub struct Controls {
    /// The default layout for this surface (`set_layout`).
    base: Layout,
    /// `base` with `offsets` applied: what is drawn and hit-tested.
    layout: Layout,
    offsets: Offsets,
    fingers: Vec<(u64, Finger)>,
    /// False after a pad key: only the menu button shows until a touch.
    visible: bool,
    /// The player's setting: only the menu button, always.
    hidden: bool,
    /// The edit mode (the game is paused): drags move controls.
    editing: bool,
    /// The edit finger: id, the group it drags, its last position.
    drag: Option<(u64, Group, (f32, f32))>,
}

impl Default for Controls {
    fn default() -> Self {
        Self::new()
    }
}

impl Controls {
    /// Visible by default: no pad is known to be connected at startup.
    pub fn new() -> Self {
        Self {
            base: Layout::default(),
            layout: Layout::default(),
            offsets: Offsets::default(),
            fingers: Vec::new(),
            visible: true,
            hidden: false,
            editing: false,
            drag: None,
        }
    }

    /// The "only menu" setting. Turning it on releases what was held.
    /// Returns true when fingers were dropped (republish [`Self::pad`]).
    #[must_use]
    pub fn set_hidden(&mut self, hidden: bool) -> bool {
        if hidden == self.hidden {
            return false;
        }
        self.hidden = hidden;
        let dropped = hidden && !self.fingers.is_empty();
        if hidden {
            self.fingers.clear();
        }
        dropped
    }

    /// Only the menu button draws and reacts (the setting, or a pad was
    /// used). Never nothing: without a pad the menu is the only way out of
    /// a hidden layout. The edit mode shows everything.
    pub fn menu_only(&self) -> bool {
        !self.editing && (self.hidden || !self.visible)
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// A new default layout (size / density / inset change): fingers are
    /// dropped, their positions meant other controls. Returns true when
    /// fingers were dropped: the caller must publish [`Self::pad`] again,
    /// or a held button / stick would stick in the game.
    #[must_use]
    pub fn set_layout(&mut self, base: Layout) -> bool {
        if base == self.base {
            return false;
        }
        self.base = base;
        // Drag ends first: a relayout mid-drag would store offsets clamped
        // to this (maybe smaller) surface for good.
        self.drag = None;
        self.relayout();
        let dropped = !self.fingers.is_empty();
        self.fingers.clear();
        dropped
    }

    /// The player's moves (from pw64.toml at startup).
    pub fn set_offsets(&mut self, o: Offsets) {
        self.offsets = o;
        self.relayout();
    }

    pub fn offsets(&self) -> &Offsets {
        &self.offsets
    }

    /// Recomputes `layout` from `base` + `offsets`. Offsets are kept as the
    /// player set them (a later, bigger surface may fit them again), except
    /// while dragging, where the clamp keeps the control under the finger.
    fn relayout(&mut self) {
        let (layout, clamped) = self.base.with_offsets(&self.offsets);
        self.layout = layout;
        if self.drag.is_some() {
            self.offsets = clamped;
        }
    }

    pub fn editing(&self) -> bool {
        self.editing
    }

    /// Enters the edit mode: everything released, controls shown.
    pub fn begin_edit(&mut self) {
        self.editing = true;
        self.visible = true;
        self.fingers.clear();
        self.drag = None;
    }

    /// Leaves the edit mode (Done, Back, the app going to the background).
    pub fn end_edit(&mut self) {
        self.editing = false;
        self.drag = None;
    }

    /// One touch in the edit mode: a finger on a control drags it (one at a
    /// time); Reset / Done act on press. Done also leaves the mode.
    pub fn edit_touch(&mut self, id: u64, phase: Phase, p: (f32, f32)) -> Option<EditAction> {
        match phase {
            Phase::Start => {
                let pill = self.layout.edit_buttons.iter().find(|(_, b)| b.contains(p));
                if let Some(&(action, _)) = pill {
                    self.drag = None;
                    match action {
                        EditAction::Done => self.end_edit(),
                        EditAction::Reset => self.set_offsets(Offsets::default()),
                    }
                    return Some(action);
                }
                if self.drag.is_none()
                    && let Some(g) = self.layout.hit_group(p)
                {
                    self.drag = Some((id, g, p));
                }
            }
            Phase::Move => {
                if let Some((d, g, last)) = self.drag
                    && d == id
                {
                    let (w, h) = self.base.size;
                    let mut anchor = p;
                    if w >= 1.0 && h >= 1.0 {
                        let prev = self.offsets;
                        let was_over = self.layout.covers_edit_bar(g);
                        let (ox, oy) = self.offsets.get(g);
                        let want = (ox + (p.0 - last.0) / w, oy + (p.1 - last.1) / h);
                        self.offsets.set(g, want);
                        self.relayout();
                        // Never under the Reset / Done bar: there it could
                        // not be grabbed again (the bar wins the hit test).
                        if !was_over && self.layout.covers_edit_bar(g) {
                            self.offsets = prev;
                            self.relayout();
                        }
                        // Held back (screen edge, bar): the anchor stays at
                        // the control, so it follows again on the way back.
                        let (gx, gy) = self.offsets.get(g);
                        anchor = (p.0 - (want.0 - gx) * w, p.1 - (want.1 - gy) * h);
                    }
                    self.drag = Some((d, g, anchor));
                }
            }
            Phase::End => {
                if self.drag.is_some_and(|(d, _, _)| d == id) {
                    self.drag = None;
                }
            }
        }
        None
    }

    /// The group being dragged in the edit mode (drawn highlighted).
    pub fn dragging(&self) -> Option<Group> {
        self.drag.map(|(_, g, _)| g)
    }

    pub fn visible(&self) -> bool {
        self.visible
    }

    /// A physical pad was used: hide and release everything. The next
    /// touch shows the controls again.
    pub fn hide(&mut self) {
        self.visible = false;
        self.fingers.clear();
    }

    /// Releases every finger (the overlay opened, the app went to the
    /// background).
    pub fn reset(&mut self) {
        self.fingers.clear();
    }

    /// One touch event. Returns true when the menu button was pressed (the
    /// window opens the settings overlay).
    pub fn on_touch(&mut self, id: u64, phase: Phase, p: (f32, f32)) -> bool {
        match phase {
            Phase::Start => {
                self.visible = true;
                // A repeated id (a lost End): the new touch replaces it.
                self.fingers.retain(|(f, _)| *f != id);
                let finger = match self.layout.hit(p) {
                    Some(Control::Menu) => {
                        self.fingers.push((id, Finger::Ignored));
                        return true;
                    }
                    _ if self.hidden => Finger::Ignored,
                    Some(c) => Finger::Button(Some(c)),
                    None if p.0 < self.layout.stick_zone_right && self.stick_finger().is_none() => {
                        Finger::Stick { origin: p, pos: p }
                    }
                    None => Finger::Ignored,
                };
                self.fingers.push((id, finger));
            }
            Phase::Move => {
                let hit = self.layout.hit(p).filter(|c| c.slides());
                if let Some((_, f)) = self.fingers.iter_mut().find(|(f, _)| *f == id) {
                    match f {
                        Finger::Stick { pos, .. } => *pos = p,
                        Finger::Button(cur) if cur.is_none_or(|c| c.slides()) => *cur = hit,
                        _ => {}
                    }
                }
            }
            Phase::End => self.fingers.retain(|(f, _)| *f != id),
        }
        false
    }

    fn stick_finger(&self) -> Option<((f32, f32), (f32, f32))> {
        self.fingers.iter().find_map(|(_, f)| match *f {
            Finger::Stick { origin, pos } => Some((origin, pos)),
            _ => None,
        })
    }

    /// The stick for drawing: (base center, knob center), px.
    pub fn stick(&self) -> ((f32, f32), (f32, f32)) {
        match self.stick_finger() {
            Some((origin, pos)) => {
                let (x, y) = self.stick_vector(origin, pos);
                let r = self.layout.stick_radius;
                (origin, (origin.0 + x * r, origin.1 - y * r))
            }
            None => (self.layout.stick_rest, self.layout.stick_rest),
        }
    }

    /// Knob offset / radius, clamped to the unit circle, y up.
    fn stick_vector(&self, origin: (f32, f32), pos: (f32, f32)) -> (f32, f32) {
        let r = self.layout.stick_radius.max(1.0);
        let (x, y) = ((pos.0 - origin.0) / r, (origin.1 - pos.1) / r);
        let mag = (x * x + y * y).sqrt();
        if mag > 1.0 {
            (x / mag, y / mag)
        } else {
            (x, y)
        }
    }

    pub fn pressed(&self, c: Control) -> bool {
        self.fingers
            .iter()
            .any(|(_, f)| *f == Finger::Button(Some(c)))
    }

    /// The N64 state of the fingers down.
    pub fn pad(&self) -> TouchPad {
        let buttons = self
            .fingers
            .iter()
            .filter_map(|(_, f)| match f {
                Finger::Button(Some(c)) => Some(c.bit()),
                _ => None,
            })
            .fold(0, |a, b| a | b);
        let (x, y) = self
            .stick_finger()
            .map_or((0.0, 0.0), |(o, p)| self.stick_vector(o, p));
        TouchPad { buttons, x, y }
    }
}

/// Draws the controls with egui over the frame (own pass, `LoadOp::Load`),
/// like the toasts.
#[cfg(target_os = "android")]
pub struct Painter {
    ctx: egui::Context,
    renderer: Option<egui_wgpu::Renderer>,
    started: std::time::Instant,
}

#[cfg(target_os = "android")]
impl Default for Painter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(target_os = "android")]
impl Painter {
    pub fn new() -> Self {
        Self {
            ctx: egui::Context::default(),
            renderer: None,
            started: std::time::Instant::now(),
        }
    }

    pub fn render(&mut self, gpu: &crate::window::Gpu, view: &wgpu::TextureView, c: &Controls) {
        if c.layout().buttons.is_empty() {
            return;
        }
        let ppp = gpu.window.scale_factor() as f32;
        let raw = crate::settings::raw_input_zoom(
            gpu,
            ppp,
            1.0,
            self.started.elapsed().as_secs_f64(),
            Vec::new(),
        );
        let ctx = self.ctx.clone();
        let out = ctx.run(raw, |ctx| draw(ctx, ppp, c));
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

/// The shapes, in egui points (px / `ppp`).
#[cfg(target_os = "android")]
fn draw(ctx: &egui::Context, ppp: f32, c: &Controls) {
    use egui::{Color32, Pos2, Shape, Stroke};
    let painter = ctx.layer_painter(egui::LayerId::background());
    let pt = |(x, y): (f32, f32)| Pos2::new(x / ppp, y / ppp);
    let rgba = |r: u8, g: u8, b: u8, a: f32| {
        Color32::from_rgba_unmultiplied(r, g, b, (a.clamp(0.0, 1.0) * 255.0) as u8)
    };
    let fg = rgba(255, 255, 255, 0.8);
    let editing = c.editing();
    // Edit mode: the paused frame dimmed, controls more opaque.
    let base_a = if editing { 0.6 } else { OPACITY };
    if editing {
        let (w, h) = c.layout().size;
        painter.rect_filled(
            egui::Rect::from_min_max(Pos2::ZERO, pt((w, h))),
            0.0,
            rgba(0, 0, 0, 0.45),
        );
        painter.text(
            pt((w / 2.0, h * 0.42)),
            egui::Align2::CENTER_CENTER,
            "Drag the controls to move them",
            egui::FontId::proportional(18.0),
            fg,
        );
    }
    let menu_only = c.menu_only();
    // Stick: base ring + knob.
    let (base, knob) = c.stick();
    let sr = c.layout().stick_radius / ppp;
    let stick_a = if c.dragging() == Some(Group::Stick) {
        base_a + 0.3
    } else {
        base_a
    };
    if !menu_only {
        painter.circle(
            pt(base),
            sr,
            rgba(20, 20, 24, stick_a * 0.6),
            Stroke::new(2.0_f32, rgba(255, 255, 255, stick_a)),
        );
        painter.circle_filled(pt(knob), sr * 0.45, rgba(200, 200, 210, stick_a + 0.15));
    }
    if editing {
        for &(action, pill) in &c.layout().edit_buttons {
            let rect = egui::Rect::from_center_size(
                pt((pill.x, pill.y)),
                egui::vec2(2.0 * pill.half_w / ppp, 2.0 * pill.half_h / ppp),
            );
            let fill = match action {
                EditAction::Done => rgba(40, 90, 230, 0.9),
                EditAction::Reset => rgba(90, 90, 100, 0.9),
            };
            painter.rect_filled(rect, pill.half_h / ppp, fill);
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                match action {
                    EditAction::Done => "Done",
                    EditAction::Reset => "Reset",
                },
                egui::FontId::proportional(pill.half_h / ppp),
                fg,
            );
        }
    }
    for &(ctl, circle) in &c.layout().buttons {
        if menu_only && ctl != Control::Menu {
            continue;
        }
        let down = c.pressed(ctl) || c.dragging() == Some(Group::of(ctl));
        let a = if down { base_a + 0.3 } else { base_a };
        let fill = match ctl {
            Control::A => rgba(40, 90, 230, a),
            Control::B => rgba(40, 170, 80, a),
            Control::CUp | Control::CDown | Control::CLeft | Control::CRight => {
                rgba(235, 190, 30, a)
            }
            _ => rgba(90, 90, 100, a),
        };
        let center = pt((circle.x, circle.y));
        let r = circle.r / ppp;
        painter.circle(
            center,
            r,
            fill,
            Stroke::new(1.5_f32, rgba(255, 255, 255, a)),
        );
        let label = |text: &str, size: f32| {
            painter.text(
                center,
                egui::Align2::CENTER_CENTER,
                text,
                egui::FontId::proportional(size),
                fg,
            );
        };
        // C buttons: a triangle pointing their way; the menu: three bars.
        let tri = |dx: f32, dy: f32| {
            let s = r * 0.45;
            let (px, py) = (-dy, dx); // perpendicular
            Shape::convex_polygon(
                vec![
                    center + egui::vec2(dx * s, dy * s),
                    center + egui::vec2(-dx * s * 0.6 + px * s, -dy * s * 0.6 + py * s),
                    center + egui::vec2(-dx * s * 0.6 - px * s, -dy * s * 0.6 - py * s),
                ],
                fg,
                Stroke::NONE,
            )
        };
        match ctl {
            Control::A => label("A", r),
            Control::B => label("B", r),
            Control::Z => label("Z", r),
            Control::R => label("R", r),
            Control::Start => label("START", r * 0.55),
            Control::CUp => {
                painter.add(tri(0.0, -1.0));
            }
            Control::CDown => {
                painter.add(tri(0.0, 1.0));
            }
            Control::CLeft => {
                painter.add(tri(-1.0, 0.0));
            }
            Control::CRight => {
                painter.add(tri(1.0, 0.0));
            }
            Control::Menu => {
                for k in [-1.0, 0.0, 1.0] {
                    let y = center.y + k * r * 0.35;
                    painter.line_segment(
                        [
                            Pos2::new(center.x - r * 0.5, y),
                            Pos2::new(center.x + r * 0.5, y),
                        ],
                        Stroke::new(2.0_f32, fg),
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 2400×1080 landscape phone at density 2.75 (≈ 873×393 dp).
    fn phone() -> Layout {
        Layout::new(2400.0, 1080.0, 2.75, Insets::default(), 1.0)
    }

    fn center(l: &Layout, c: Control) -> (f32, f32) {
        let (_, circle) = l.buttons.iter().find(|(k, _)| *k == c).unwrap();
        (circle.x, circle.y)
    }

    #[test]
    fn layout_fits_and_buttons_do_not_overlap() {
        for (w, h, dp) in [
            (2400.0, 1080.0, 2.75),
            (1600.0, 720.0, 2.0),
            (2960.0, 1440.0, 4.0),
            (1280.0, 800.0, 1.0),
        ] {
            let l = Layout::new(w, h, dp, Insets::default(), 1.0);
            for (i, (a, ca)) in l.buttons.iter().enumerate() {
                assert!(
                    ca.x - ca.r >= 0.0
                        && ca.x + ca.r <= w
                        && ca.y - ca.r >= 0.0
                        && ca.y + ca.r <= h,
                    "{a:?} off screen at {w}x{h}"
                );
                for (b, cb) in &l.buttons[i + 1..] {
                    let d = ((ca.x - cb.x).powi(2) + (ca.y - cb.y).powi(2)).sqrt();
                    assert!(d > ca.r + cb.r, "{a:?} overlaps {b:?} at {w}x{h}");
                }
            }
            let (sx, sy) = l.stick_rest;
            assert!(sx - l.stick_radius >= 0.0 && sy + l.stick_radius <= h);
            assert!(sx + l.stick_radius < l.stick_zone_right);
        }
    }

    #[test]
    fn insets_push_controls_inward() {
        let plain = phone();
        let inset = Layout::new(
            2400.0,
            1080.0,
            2.75,
            Insets {
                left: 100.0,
                right: 100.0,
                ..Default::default()
            },
            1.0,
        );
        // 100 px inset + 8 dp gap replaces the 36 dp edge margin.
        let shift = 100.0 + 8.0 * 2.75 - 36.0 * 2.75;
        assert!((inset.stick_rest.0 - plain.stick_rest.0 - shift).abs() < 1e-3);
        assert!((center(&inset, Control::A).0 - center(&plain, Control::A).0 + shift).abs() < 1e-3);
        // A small inset stays inside the edge margin: no shift.
        let small = Insets {
            left: 20.0,
            ..Default::default()
        };
        assert_eq!(
            Layout::new(2400.0, 1080.0, 2.75, small, 1.0).stick_rest,
            plain.stick_rest
        );
    }

    #[test]
    fn hit_test_slack_and_nearest() {
        let l = phone();
        let (ax, ay) = center(&l, Control::A);
        assert_eq!(l.hit((ax, ay)), Some(Control::A));
        let (_, a) = l.buttons[0];
        // Inside the slack ring, outside the drawn circle.
        assert_eq!(l.hit((ax + a.r * 1.1, ay)), Some(Control::A));
        assert_eq!(l.hit((ax + a.r * 1.3, ay)), None);
        assert_eq!(l.hit((1200.0, 400.0)), None, "the screen middle is free");
    }

    #[test]
    fn floating_stick_vector_clamped_y_up() {
        let mut c = Controls::new();
        let _ = c.set_layout(phone());
        let r = c.layout().stick_radius;
        let o = (500.0, 600.0);
        assert!(!c.on_touch(1, Phase::Start, o));
        assert_eq!(c.pad(), TouchPad::IDLE, "the base lands under the finger");
        c.on_touch(1, Phase::Move, (o.0 + r / 2.0, o.1));
        assert!((c.pad().x - 0.5).abs() < 1e-5);
        // Drag up past the rim: full up, clamped.
        c.on_touch(1, Phase::Move, (o.0, o.1 - 3.0 * r));
        let p = c.pad();
        assert!(p.x.abs() < 1e-5 && (p.y - 1.0).abs() < 1e-5);
        assert_eq!(c.stick().1, (o.0, o.1 - r), "knob drawn on the rim");
        c.on_touch(1, Phase::End, o);
        assert_eq!(c.pad(), TouchPad::IDLE);
        assert_eq!(c.stick(), (c.layout().stick_rest, c.layout().stick_rest));
    }

    #[test]
    fn multi_touch_stick_and_buttons_together() {
        let mut c = Controls::new();
        let _ = c.set_layout(phone());
        let l = c.layout().clone();
        c.on_touch(7, Phase::Start, (400.0, 700.0));
        c.on_touch(7, Phase::Move, (400.0 - l.stick_radius, 700.0));
        c.on_touch(8, Phase::Start, center(&l, Control::A));
        c.on_touch(9, Phase::Start, center(&l, Control::Z));
        let p = c.pad();
        assert_eq!(p.buttons, CONT_A | CONT_Z);
        assert!((p.x + 1.0).abs() < 1e-5);
        // A second finger in the stick zone doesn't steal the stick.
        c.on_touch(10, Phase::Start, (300.0, 500.0));
        assert!((c.pad().x + 1.0).abs() < 1e-5);
        c.on_touch(8, Phase::End, (0.0, 0.0));
        assert_eq!(c.pad().buttons, CONT_Z);
        c.on_touch(7, Phase::End, (0.0, 0.0));
        assert_eq!(c.pad().x, 0.0);
        assert_eq!(c.pad().buttons, CONT_Z);
    }

    #[test]
    fn sliding_between_buttons_but_not_onto_start() {
        let mut c = Controls::new();
        let _ = c.set_layout(phone());
        let l = c.layout().clone();
        c.on_touch(1, Phase::Start, center(&l, Control::A));
        c.on_touch(1, Phase::Move, center(&l, Control::B));
        assert_eq!(c.pad().buttons, CONT_B, "A → B slide");
        c.on_touch(1, Phase::Move, (1200.0, 300.0));
        assert_eq!(c.pad().buttons, 0, "slid off");
        c.on_touch(1, Phase::Move, center(&l, Control::Start));
        assert_eq!(c.pad().buttons, 0, "Start needs its own press");
        // A press on Start stays Start while the finger wanders.
        c.on_touch(2, Phase::Start, center(&l, Control::Start));
        c.on_touch(2, Phase::Move, center(&l, Control::A));
        assert_eq!(c.pad().buttons, CONT_START);
    }

    #[test]
    fn menu_button_requests_settings_and_never_reaches_the_game() {
        let mut c = Controls::new();
        let _ = c.set_layout(phone());
        let m = center(c.layout(), Control::Menu);
        assert!(c.on_touch(3, Phase::Start, m));
        assert_eq!(c.pad(), TouchPad::IDLE);
        assert!(!c.on_touch(3, Phase::End, m));
    }

    #[test]
    fn pad_use_hides_and_next_touch_shows() {
        let mut c = Controls::new();
        let _ = c.set_layout(phone());
        let a = center(c.layout(), Control::A);
        c.on_touch(1, Phase::Start, a);
        c.hide();
        assert!(!c.visible());
        assert_eq!(c.pad(), TouchPad::IDLE, "hiding releases");
        c.on_touch(1, Phase::End, a);
        c.on_touch(2, Phase::Start, a);
        assert!(c.visible());
        assert_eq!(c.pad().buttons, CONT_A);
    }

    #[test]
    fn relayout_drops_fingers_and_empty_layout_ignores_touches() {
        let mut c = Controls::new();
        c.on_touch(1, Phase::Start, (10.0, 10.0));
        assert_eq!(c.pad(), TouchPad::IDLE, "no layout yet: nothing to grab");
        // Only the ignored finger was down: nothing held, but it is dropped.
        assert!(c.set_layout(phone()));
        assert!(!c.set_layout(phone()), "same layout: nothing dropped");
        c.on_touch(2, Phase::Start, center(c.layout(), Control::A));
        assert_eq!(c.pad().buttons, CONT_A);
        assert!(
            c.set_layout(Layout::new(1600.0, 720.0, 2.0, Insets::default(), 1.0)),
            "the caller must republish"
        );
        assert_eq!(c.pad(), TouchPad::IDLE);
        assert!(!c.set_layout(Layout::new(1280.0, 720.0, 2.0, Insets::default(), 1.0)));
    }

    #[test]
    fn offsets_move_groups_and_clamp_on_screen() {
        let base = phone();
        let mut o = Offsets::default();
        o.set(Group::Z, (0.1, 0.0));
        o.set(Group::C, (-0.05, 0.02));
        let (l, clamped) = base.with_offsets(&o);
        assert_eq!(clamped, o, "in range: unchanged");
        let z0 = center(&base, Control::Z);
        assert!((center(&l, Control::Z).0 - (z0.0 + 240.0)).abs() < 1e-3);
        for c in [
            Control::CUp,
            Control::CDown,
            Control::CLeft,
            Control::CRight,
        ] {
            let (a, b) = (center(&base, c), center(&l, c));
            assert!((b.0 - a.0 + 120.0).abs() < 1e-3 && (b.1 - a.1 - 21.6).abs() < 1e-3);
        }
        assert_eq!(center(&l, Control::A), center(&base, Control::A));
        // Far off screen: clamped so every circle stays visible.
        o.set(Group::C, (1.0, -1.0));
        o.set(Group::Stick, (-1.0, 1.0));
        let (l, clamped) = base.with_offsets(&o);
        for (ctl, c) in &l.buttons {
            assert!(c.x + c.r <= 2400.0 + 1e-3 && c.y - c.r >= -1e-3, "{ctl:?}");
        }
        assert!(clamped.get(Group::C).0 < 1.0);
        assert!(l.stick_rest.0 - l.stick_radius >= -1e-3);
        assert!(l.stick_rest.1 + l.stick_radius <= 1080.0 + 1e-3);
        let cfg = Offsets::from_config(&[("z".into(), (0.1, 0.0)), ("bogus".into(), (0.5, 0.5))]);
        assert_eq!(cfg.to_config(), vec![("z", (0.1, 0.0))]);
    }

    #[test]
    fn hidden_setting_keeps_only_the_menu() {
        let mut c = Controls::new();
        let _ = c.set_layout(phone());
        let l = c.layout().clone();
        c.on_touch(1, Phase::Start, center(&l, Control::A));
        assert!(c.set_hidden(true), "held A released: republish");
        assert!(c.menu_only());
        c.on_touch(2, Phase::Start, center(&l, Control::B));
        c.on_touch(3, Phase::Start, (400.0, 700.0));
        c.on_touch(3, Phase::Move, (300.0, 700.0));
        assert_eq!(c.pad(), TouchPad::IDLE, "no buttons, no stick");
        assert!(c.on_touch(4, Phase::Start, center(&l, Control::Menu)));
        // A pad press alone also leaves the menu showing.
        let _ = c.set_hidden(false);
        c.hide();
        assert!(c.menu_only());
        c.begin_edit();
        assert!(!c.menu_only(), "the edit mode shows everything");
    }

    #[test]
    fn edit_mode_drags_resets_and_done() {
        let mut c = Controls::new();
        let _ = c.set_layout(phone());
        let z = center(c.layout(), Control::Z);
        c.begin_edit();
        assert!(c.editing());
        assert_eq!(c.edit_touch(1, Phase::Start, z), None);
        assert_eq!(c.dragging(), Some(Group::Z));
        c.edit_touch(1, Phase::Move, (z.0 + 120.0, z.1 + 54.0));
        let moved = center(c.layout(), Control::Z);
        assert!((moved.0 - z.0 - 120.0).abs() < 1e-3 && (moved.1 - z.1 - 54.0).abs() < 1e-3);
        assert!((c.offsets().get(Group::Z).0 - 0.05).abs() < 1e-5);
        c.edit_touch(1, Phase::End, moved);
        assert_eq!(c.dragging(), None);
        assert_eq!(c.pad(), TouchPad::IDLE, "edits never press buttons");
        // The stick's rest spot drags too.
        let rest = c.layout().stick_rest;
        c.edit_touch(2, Phase::Start, rest);
        c.edit_touch(2, Phase::Move, (rest.0 + 30.0, rest.1 - 30.0));
        c.edit_touch(2, Phase::End, rest);
        assert_eq!(c.layout().stick_rest, (rest.0 + 30.0, rest.1 - 30.0));
        let pill = |c: &Controls, a: EditAction| {
            let (_, p) = c
                .layout()
                .edit_buttons
                .iter()
                .find(|(k, _)| *k == a)
                .unwrap();
            (p.x, p.y)
        };
        // A control can't be dragged under the Reset / Done bar.
        let r = center(c.layout(), Control::R);
        let done = pill(&c, EditAction::Done);
        c.edit_touch(5, Phase::Start, r);
        c.edit_touch(5, Phase::Move, done);
        c.edit_touch(5, Phase::End, done);
        assert_eq!(center(c.layout(), Control::R), r, "blocked by the bar");
        // Past the screen edge and back: it waits at the edge until the
        // finger is back over it, then follows again.
        let a = center(c.layout(), Control::A);
        c.edit_touch(6, Phase::Start, a);
        c.edit_touch(6, Phase::Move, (a.0 + 2000.0, a.1));
        let edge = center(c.layout(), Control::A);
        c.edit_touch(6, Phase::Move, (a.0 + 1990.0, a.1));
        assert_eq!(center(c.layout(), Control::A), edge);
        c.edit_touch(6, Phase::Move, (edge.0 - 10.0, a.1));
        assert!((center(c.layout(), Control::A).0 - (edge.0 - 10.0)).abs() < 1e-2);
        c.edit_touch(6, Phase::End, a);
        assert_eq!(
            c.edit_touch(3, Phase::Start, pill(&c, EditAction::Reset)),
            Some(EditAction::Reset)
        );
        assert_eq!(*c.offsets(), Offsets::default());
        assert_eq!(center(c.layout(), Control::Z), z);
        assert!(c.editing());
        assert_eq!(
            c.edit_touch(4, Phase::Start, pill(&c, EditAction::Done)),
            Some(EditAction::Done)
        );
        assert!(!c.editing());
    }

    /// Split screen / small windows: the right column still fits.
    #[test]
    fn short_surface_keeps_controls_on_screen() {
        for (w, h, dp) in [
            (1080.0, 600.0, 2.75),
            (2400.0, 540.0, 2.75),
            (900.0, 400.0, 2.0),
        ] {
            let l = Layout::new(w, h, dp, Insets::default(), 1.0);
            for (ctl, c) in &l.buttons {
                assert!(
                    c.y - c.r >= 0.0 && c.y + c.r <= h && c.x - c.r >= 0.0 && c.x + c.r <= w,
                    "{ctl:?} off screen at {w}x{h}"
                );
            }
            for (i, (a, ca)) in l.buttons.iter().enumerate() {
                for (b, cb) in &l.buttons[i + 1..] {
                    let d = ((ca.x - cb.x).powi(2) + (ca.y - cb.y).powi(2)).sqrt();
                    assert!(d > ca.r + cb.r, "{a:?} overlaps {b:?} at {w}x{h}");
                }
            }
        }
    }
}
