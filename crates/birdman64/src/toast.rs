//! Transient notifications in the window's bottom-left corner (ga-polish
//! S2, ux-1.0 U7/U8): the first-launch welcome card and the controller
//! connect/disconnect toasts; later banners (save/restart, software-GPU
//! preset) go through the same pipe.
//!
//! Rendered like the settings overlay (egui + egui-wgpu over the frame
//! already in the surface, own pass with `LoadOp::Load`), but with no
//! messages it does no egui work at all: closed, game rendering is
//! untouched.

use crate::settings;
use crate::window::Gpu;
use std::time::{Duration, Instant};

pub struct Toast {
    ctx: egui::Context,
    renderer: Option<egui_wgpu::Renderer>,
    /// (text, deadline); expired entries drop on the next render.
    msgs: Vec<(String, Instant)>,
    /// (title, lines, deadline) welcome cards (U8): taller than a toast,
    /// same corner; expired entries drop on the next render.
    cards: Vec<(String, Vec<String>, Instant)>,
    /// egui's time source (elapsed since this process started).
    started: Instant,
}

impl Default for Toast {
    fn default() -> Self {
        Self::new()
    }
}

impl Toast {
    pub fn new() -> Self {
        Self {
            ctx: egui::Context::default(),
            renderer: None,
            msgs: Vec::new(),
            cards: Vec::new(),
            started: Instant::now(),
        }
    }

    /// Queues a message for `secs` seconds (it fades out over the last one).
    pub fn show(&mut self, text: &str, secs: u64) {
        self.msgs
            .push((text.to_string(), Instant::now() + Duration::from_secs(secs)));
    }

    /// Queues a card (U8): a bold title plus lines, for `secs` seconds (it
    /// fades out over the last one). Same corner as the plain toasts, which
    /// stack above it while it is live.
    pub fn show_card(&mut self, title: &str, lines: &[String], secs: u64) {
        self.cards.push((
            title.to_string(),
            lines.to_vec(),
            Instant::now() + Duration::from_secs(secs),
        ));
    }

    /// Draws the live messages over the frame already in `view`. Nothing to
    /// show → returns before any egui or GPU work.
    pub fn render(&mut self, gpu: &Gpu, view: &wgpu::TextureView) {
        let now = Instant::now();
        self.msgs.retain(|(_, deadline)| *deadline > now);
        self.cards.retain(|(_, _, deadline)| *deadline > now);
        if self.msgs.is_empty() && self.cards.is_empty() {
            return;
        }
        let ppp = gpu.window.scale_factor() as f32;
        let zoom = settings::ui_zoom(gpu.config.height, ppp);
        // Same pattern as the settings overlay: only on change, because a
        // pending zoom makes egui keep the previous pass's screen rect.
        if (self.ctx.zoom_factor() - zoom).abs() > f32::EPSILON {
            self.ctx.set_zoom_factor(zoom);
        }
        let raw = settings::raw_input_zoom(
            gpu,
            ppp,
            zoom,
            self.started.elapsed().as_secs_f64(),
            Vec::new(),
        );
        let ctx = self.ctx.clone();
        let out = ctx.run(raw, |ctx| self.ui(ctx, now));
        settings::paint(
            gpu,
            &self.ctx,
            &mut self.renderer,
            out,
            view,
            wgpu::LoadOp::Load,
        );
    }

    /// The message frame(s): bottom-left, small dark translucent card.
    /// Cards (U8) sit at the bottom, plain toasts stack above them. Later
    /// messages stack above earlier ones.
    fn ui(&mut self, ctx: &egui::Context, now: Instant) {
        // Cards first; the plain toasts' base offset moves up by the stack
        // (heights estimated: margins + title line + one per content line).
        let mut used = 0.0_f32;
        for (i, (title, lines, deadline)) in self.cards.iter().enumerate() {
            let a = fade(deadline, now);
            card_area(
                ctx,
                "pw64 card",
                i,
                egui::vec2(12.0, -12.0 - used),
                a,
                |ui, fg| {
                    ui.set_max_width(360.0);
                    ui.strong(egui::RichText::new(title).size(17.0).color(fg));
                    for line in lines {
                        ui.label(egui::RichText::new(line).size(14.0).color(fg));
                    }
                },
            );
            used += 42.0 + lines.len() as f32 * 18.0;
        }
        for (i, (text, deadline)) in self.msgs.iter().enumerate() {
            let a = fade(deadline, now);
            card_area(
                ctx,
                "pw64 toast",
                i,
                egui::vec2(12.0, -12.0 - used - 44.0 * i as f32),
                a,
                |ui, fg| {
                    ui.label(egui::RichText::new(text).size(14.0).color(fg));
                },
            );
        }
    }
}

/// Alpha 1.0 until the final second of a message, then a linear fade out.
fn fade(deadline: &Instant, now: Instant) -> f32 {
    deadline.duration_since(now).as_secs_f32().clamp(0.0, 1.0)
}

/// One bottom-left dark translucent card: `id` + `i` key, anchored at
/// `offset` (its bottom-left corner), content built by `add` with the
/// message color for alpha `a` (the text fades with the card).
fn card_area(
    ctx: &egui::Context,
    id: &str,
    i: usize,
    offset: egui::Vec2,
    a: f32,
    add: impl FnOnce(&mut egui::Ui, egui::Color32),
) {
    let bg = egui::Color32::from_rgba_unmultiplied(15, 18, 24, (190.0 * a) as u8);
    let fg = egui::Color32::from_rgba_unmultiplied(235, 238, 245, (255.0 * a) as u8);
    egui::Area::new(egui::Id::new(id).with(i))
        .anchor(egui::Align2::LEFT_BOTTOM, offset)
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(bg)
                .inner_margin(egui::Margin::same(8))
                .show(ui, |ui| {
                    ui.set_max_width(360.0);
                    add(ui, fg);
                });
        });
}
