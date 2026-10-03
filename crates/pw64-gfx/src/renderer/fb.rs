//! N64 framebuffer memory: one persistent render target per framebuffer
//! address (renderer.md "Framebuffer persistence").
//!
//! The N64 keeps a framebuffer's pixels until something overwrites them:
//! the game clears with fill rectangles (not implicitly per task), and some
//! screens draw once and then only update part of the image (snap.c photo
//! grid / single photo, replay_screen.c `screen_fadeout`). So a gfx task is
//! drawn onto its framebuffer's target with the old contents loaded,
//! `uvCopyFrameBuf` copies one target into another, and presenting shows the
//! target the VI swapped to.
//!
//! - Targets are the output area only (4:3 or the widescreen aspect, no
//!   pillar/letterbox bars) at [`Renderer::set_fb_size`]; a size change
//!   resamples the old contents (so a resize or a live scale change keeps a
//!   static screen), and [`Renderer::fb_present`] letterboxes into the
//!   window.
//! - MSAA: the multisampled attachment is shared by all targets (as for
//!   plain renders); a task first "reloads" the resolved target into it
//!   (fullscreen `textureLoad`, all samples = the resolved colour), then
//!   draws and resolves back. Resolve(all samples = c) = c, so contents
//!   persist exactly; the cost is one fullscreen pass per task instead of a
//!   second MSAA texture per framebuffer (up to 265 MB each at 4K × 8).
//! - Depth is still cleared per task: the z image is shared on the N64, but
//!   the game clears it itself before every z-buffered channel
//!   (`uvGfx_80222A98`, full-screen fill of the z image; no channel sets the
//!   skip flag 4) and in `dobj.c`, so no task reads an older task's z.
//! - Widescreen side margins are not N64 memory. A task that draws nothing
//!   into them (menus, text-only updates) gets black margins, as before
//!   persistence; tasks that do draw there (world view, full-width fills,
//!   edge-anchored HUD) keep the old margin contents like the rest.

use super::Renderer;
use crate::frame::Frame;
use std::collections::HashMap;
use std::sync::Arc;

/// One framebuffer operation, in game order (the `pw64` game thread records
/// them; the window thread replays them on its own renderer).
#[derive(Clone)]
pub enum FbOp {
    /// A gfx task drawn onto framebuffer `fb` (physical address).
    Draw { fb: u32, frame: Arc<Frame> },
    /// `uvCopyFrameBuf`: `src`'s pixels into `dst`.
    Copy { dst: u32, src: u32 },
}

struct Target {
    tex: wgpu::Texture,
    view: wgpu::TextureView,
    /// The last non-empty task drawn here had a fill-view draw spanning the
    /// main view vertically (its scissor covers the output area's full
    /// height): the top row / right column are drawn, so `vi_border` skips
    /// them. `fb_copy` carries the flag; an empty task keeps it, a
    /// non-empty one without such a draw clears it.
    filled: bool,
}

/// The framebuffer targets of one [`Renderer`].
#[derive(Default)]
pub(super) struct FbStore {
    size: (u32, u32),
    targets: HashMap<u32, Target>,
    /// Fullscreen helper pipelines by (reload?, sample count): reload =
    /// `fs_load` (resolved target → MSAA attachment), else `fs_black`.
    pipelines: HashMap<(bool, u32), wgpu::RenderPipeline>,
}

impl Renderer {
    /// Size of every framebuffer target (the output area at render
    /// resolution, see [`Renderer::output_rect`]). A new size resamples the
    /// existing targets' contents.
    pub fn set_fb_size(&mut self, size: (u32, u32)) {
        let size = (size.0.max(1), size.1.max(1));
        if self.fbs.size == size {
            return;
        }
        self.fbs.size = size;
        let old = std::mem::take(&mut self.fbs.targets);
        for (fb, t) in old {
            // The picture survives the resample, so its classification
            // does too.
            let filled = t.filled;
            let new = self.new_fb_target(size);
            self.blit_into(&t.view, &new.view, wgpu::FilterMode::Linear, None);
            self.fbs.targets.insert(fb, Target { filled, ..new });
        }
    }

    /// Current framebuffer target size.
    pub fn fb_size(&self) -> (u32, u32) {
        self.fbs.size
    }

    /// The output area (4:3, or the widescreen aspect) fitted and centred in
    /// a target of `size`, in whole pixels: (x, y, w, h).
    pub fn output_rect(&self, size: (u32, u32)) -> [u32; 4] {
        let aspect = Self::aspect(self.options.widescreen, size);
        let (sw, sh) = (size.0.max(1), size.1.max(1));
        let ([x, y, w, h], _) = Self::region((sw, sh), aspect);
        let w = (w.round() as u32).clamp(1, sw);
        let h = (h.round() as u32).clamp(1, sh);
        [
            (x.round() as u32).min(sw - w),
            (y.round() as u32).min(sh - h),
            w,
            h,
        ]
    }

    /// Applies one [`FbOp`].
    pub fn fb_apply(&mut self, op: &FbOp) {
        match op {
            FbOp::Draw { fb, frame } => self.fb_draw(*fb, frame),
            FbOp::Copy { dst, src } => self.fb_copy(*dst, *src),
        }
    }

    /// VI overscan over an output area `[x, y, w, h]`: the two strips to
    /// black — the outermost top row and right column (one N64 pixel; the
    /// output area is uniformly scaled, so the N64 pixel is `h / 240`
    /// target px). The game never draws them: its letterbox bars are
    /// texrects y1..8 and x..319 (RDP rects fill pixels whose *top-left
    /// corner* is inside the rect, so the 319.0 edge excludes column 319),
    /// and the TV's overscan hides the rest on hardware. Without this, our
    /// persistent framebuffer targets keep their boot-time contents there
    /// (white flash remnants). This is the game's own geometry
    /// (`drawScreenBorder` → `uvVtxRect` with `SCREEN_HEIGHT-1` and
    /// `SCREEN_WIDTH-1`: 1-cycle texrects, exclusive lower-right edges), not a
    /// rasterization off-by-one of ours. `right` = false (widescreen) skips
    /// the column: the extended view and the stretched top/bottom bars
    /// reach the output's right edge there. `filled` (fill view: the last
    /// non-empty task had a world draw spanning the view vertically, see
    /// `Renderer::extend_y`) skips both strips: the world reaches the true
    /// edges and nothing is left to hide.
    fn vi_border(rect: [u32; 4], right: bool, filled: bool) -> [[u32; 4]; 2] {
        // Rounded up: at fractional scales (e.g. 1080 / 240 = 4.5) the
        // undrawn N64 pixel can touch one more target pixel.
        let b = rect[3].div_ceil(240).max(1);
        let top = if filled {
            [rect[0], rect[1], 0, 0]
        } else {
            [rect[0], rect[1], rect[2], b.min(rect[3])]
        };
        let col = if right && !filled {
            [
                rect[0] + rect[2].saturating_sub(b),
                rect[1],
                b.min(rect[2]),
                rect[3],
            ]
        } else {
            [rect[0] + rect[2].saturating_sub(b), rect[1], 0, rect[3]]
        };
        [top, col]
    }

    /// Whether `vi_border` blacks the right column (4:3 output only).
    fn vi_border_right(&self) -> bool {
        matches!(self.options.widescreen, crate::Widescreen::Off)
    }

    /// Draws a gfx task's `frame` onto framebuffer `fb`'s target, over its
    /// previous contents (created black on first use; an empty task changes
    /// nothing else). A non-empty task sets `filled` from its fill-view
    /// draws (an empty task keeps the old value).
    pub fn fb_draw(&mut self, fb: u32, frame: &Frame) {
        let view = self.fb_target(fb).view.clone();
        if !frame.draws.is_empty() {
            let size = self.fbs.size;
            let filled = frame.draws.iter().any(|d| self.extend_y(d, size));
            self.draw_frame(frame, &view, size, true);
            self.fbs.targets.get_mut(&fb).unwrap().filled = filled;
        }
    }

    /// `uvCopyFrameBuf`: `dst` becomes a copy of `src` (black if `src` was
    /// never drawn): the `filled` flag copies with the pixels.
    pub fn fb_copy(&mut self, dst: u32, src: u32) {
        if dst == src {
            return;
        }
        let filled = self.fbs.targets.get(&src).is_some_and(|t| t.filled);
        self.fb_target(dst);
        self.fbs.targets.get_mut(&dst).unwrap().filled = filled;
        let size = self.fbs.size;
        let mut enc = self.device.create_command_encoder(&Default::default());
        match self.fbs.targets.get(&src) {
            Some(s) => enc.copy_texture_to_texture(
                s.tex.as_image_copy(),
                self.fbs.targets[&dst].tex.as_image_copy(),
                wgpu::Extent3d {
                    width: size.0,
                    height: size.1,
                    depth_or_array_layers: 1,
                },
            ),
            None => clear_pass(&mut enc, &self.fbs.targets[&dst].view),
        }
        self.queue.submit([enc.finish()]);
    }

    /// Whether `fb` has a target (was drawn or copied into).
    pub fn fb_exists(&self, fb: u32) -> bool {
        self.fbs.targets.contains_key(&fb)
    }

    /// Framebuffer `fb`'s target, if it exists (e.g. to blit it elsewhere).
    pub fn fb_view(&self, fb: u32) -> Option<&wgpu::TextureView> {
        self.fbs.targets.get(&fb).map(|t| &t.view)
    }

    /// Shows framebuffer `fb` in `target` (this renderer's format): scaled
    /// into [`Renderer::output_rect`], black around it (and everywhere
    /// when `fb` is `None` or was never drawn). `filter` applies when the
    /// sizes differ (the window's `PW64_SCALE` downsample); equal sizes copy
    /// pixel for pixel. The VI overscan (`vi_border`) is blacked on top.
    pub fn fb_present(
        &mut self,
        fb: Option<u32>,
        target: &wgpu::TextureView,
        filter: wgpu::FilterMode,
    ) {
        let t = target.texture();
        let rect = self.output_rect((t.width(), t.height()));
        match fb.and_then(|fb| self.fbs.targets.get(&fb)) {
            Some(src) => {
                let filled = src.filled;
                let src = src.view.clone();
                let filter = if (rect[2], rect[3]) == self.fbs.size {
                    wgpu::FilterMode::Nearest
                } else {
                    filter
                };
                self.blit_into(&src, target, filter, Some(rect));
                // VI overscan: black the outermost top row / right column
                // (see `vi_border`): the game leaves them undrawn, unless
                // the fill view drew over them.
                self.fs_pipeline(false, 1);
                let mut enc = self.device.create_command_encoder(&Default::default());
                let mut pass = color_pass(&mut enc, target, wgpu::LoadOp::Load);
                pass.set_pipeline(&self.fbs.pipelines[&(false, 1)]);
                let mut bordered = false;
                for [x, y, w, h] in Self::vi_border(rect, self.vi_border_right(), filled) {
                    if w > 0 && h > 0 {
                        pass.set_scissor_rect(x, y, w, h);
                        pass.draw(0..3, 0..1);
                        bordered = true;
                    }
                }
                drop(pass);
                if bordered {
                    self.queue.submit([enc.finish()]);
                }
            }
            None => {
                let mut enc = self.device.create_command_encoder(&Default::default());
                clear_pass(&mut enc, target);
                self.queue.submit([enc.finish()]);
            }
        }
    }

    /// Reads framebuffer `fb` back as tightly packed RGBA8 at
    /// [`Renderer::fb_size`] (renderer created with `Rgba8Unorm`). The VI
    /// overscan (`vi_border`) is blacked, so dumps show what the window shows.
    pub fn fb_read_rgba(&self, fb: u32) -> Option<Vec<u8>> {
        assert_eq!(self.format, wgpu::TextureFormat::Rgba8Unorm);
        let t = self.fbs.targets.get(&fb)?;
        let filled = t.filled;
        let mut rgba = super::read_rgba(&self.device, &self.queue, &t.tex);
        let (w, h) = self.fbs.size;
        let [top, right] = Self::vi_border([0, 0, w, h], self.vi_border_right(), filled);
        let black = |rgba: &mut [u8], x: usize, y: usize| {
            let o = (y * w as usize + x) * 4;
            rgba[o..o + 4].fill(0);
        };
        // Top strip (full width) and right strip (its own rect).
        let [tx, ty, tw, th] = top;
        for y in (ty as usize)..(ty + th) as usize {
            for x in (tx as usize)..(tx + tw) as usize {
                black(&mut rgba, x, y);
            }
        }
        let [rx, ry, rw, rh] = right;
        for y in (ry as usize)..(ry + rh) as usize {
            for x in (rx as usize)..(rx + rw) as usize {
                black(&mut rgba, x, y);
            }
        }
        Some(rgba)
    }

    /// `fb`'s target, created black on first use.
    fn fb_target(&mut self, fb: u32) -> &Target {
        if !self.fbs.targets.contains_key(&fb) {
            if self.fbs.size == (0, 0) {
                self.fbs.size = (1, 1);
            }
            let t = self.new_fb_target(self.fbs.size);
            self.fbs.targets.insert(fb, t);
        }
        &self.fbs.targets[&fb]
    }

    fn new_fb_target(&self, size: (u32, u32)) -> Target {
        let tex = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("pw64 framebuffer"),
            size: wgpu::Extent3d {
                width: size.0,
                height: size.1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: self.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = tex.create_view(&Default::default());
        // Opaque black (wgpu's zero-init would leave alpha 0).
        let mut enc = self.device.create_command_encoder(&Default::default());
        clear_pass(&mut enc, &view);
        self.queue.submit([enc.finish()]);
        Target {
            tex,
            view,
            filled: false,
        }
    }

    /// Before a persistent draw of `frame` into `target` (a framebuffer
    /// target of `size`, scissors = the draws' target scissors): puts the
    /// kept contents where the main pass will load them and returns that
    /// pass's color load op. MSAA: reload the resolved target into the
    /// multisampled attachment (its kept part; the rest black). 1×: the
    /// target is the attachment; only black out the margins if needed.
    pub(super) fn prepare_persistent(
        &mut self,
        enc: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        size: (u32, u32),
        frame: &Frame,
        scissors: &[[u32; 4]],
    ) -> wgpu::LoadOp<wgpu::Color> {
        // The 4:3 area in target pixels (edge columns with partial 4:3
        // content count as kept).
        let aspect = Self::aspect(self.options.widescreen, size);
        let (_, inner) = Self::region(size, aspect);
        let l = (inner[0].floor().max(0.0) as u32).min(size.0);
        let r = ((inner[0] + inner[2]).ceil() as u32).clamp(l, size.0);
        let in_margins =
            frame.draws.iter().zip(scissors).any(|(d, &[x, _, w, h])| {
                d.vertex_count > 0 && w > 0 && h > 0 && (x < l || x + w > r)
            });
        let keep = if in_margins || (l == 0 && r == size.0) {
            [0, 0, size.0, size.1]
        } else {
            [l, 0, r - l, size.1]
        };
        let n = self.options.msaa;
        if n > 1 {
            self.fs_pipeline(true, n);
            let sampler = self.nearest_sampler();
            let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("pw64 fb reload"),
                layout: &self.blit_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(target),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&sampler),
                    },
                ],
            });
            let msaa = self.targets.as_ref().and_then(|t| t.msaa.as_ref());
            let msaa = msaa.expect("MSAA attachment (ensure_targets)");
            let mut pass = color_pass(enc, msaa, wgpu::LoadOp::Clear(wgpu::Color::BLACK));
            if keep[2] > 0 {
                pass.set_pipeline(&self.fbs.pipelines[&(true, n)]);
                pass.set_bind_group(0, &bind, &[]);
                pass.set_scissor_rect(keep[0], keep[1], keep[2], keep[3]);
                pass.draw(0..3, 0..1);
            }
        } else if keep[2] < size.0 {
            self.fs_pipeline(false, 1);
            let mut pass = color_pass(enc, target, wgpu::LoadOp::Load);
            pass.set_pipeline(&self.fbs.pipelines[&(false, 1)]);
            for [x, w] in [
                [0, keep[0]],
                [keep[0] + keep[2], size.0 - keep[0] - keep[2]],
            ] {
                if w > 0 {
                    pass.set_scissor_rect(x, 0, w, size.1);
                    pass.draw(0..3, 0..1);
                }
            }
        }
        wgpu::LoadOp::Load
    }

    fn nearest_sampler(&mut self) -> wgpu::Sampler {
        let filter = wgpu::FilterMode::Nearest;
        self.blit_samplers
            .entry(filter)
            .or_insert_with(|| {
                self.device.create_sampler(&wgpu::SamplerDescriptor {
                    label: Some("pw64 blit"),
                    address_mode_u: wgpu::AddressMode::ClampToEdge,
                    address_mode_v: wgpu::AddressMode::ClampToEdge,
                    mag_filter: filter,
                    min_filter: filter,
                    ..Default::default()
                })
            })
            .clone()
    }

    /// Creates the fullscreen helper pipeline (see [`FbStore::pipelines`]).
    fn fs_pipeline(&mut self, reload: bool, samples: u32) {
        if self.fbs.pipelines.contains_key(&(reload, samples)) {
            return;
        }
        let groups: &[&wgpu::BindGroupLayout] = if reload { &[&self.blit_layout] } else { &[] };
        let layout = self
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("pw64 fb helper"),
                bind_group_layouts: groups,
                push_constant_ranges: &[],
            });
        let pipeline = self
            .device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("pw64 fb helper"),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &self.blit_shader,
                    entry_point: Some("vs_main"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState {
                    count: samples,
                    ..Default::default()
                },
                fragment: Some(wgpu::FragmentState {
                    module: &self.blit_shader,
                    entry_point: Some(if reload { "fs_load" } else { "fs_black" }),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: self.format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview: None,
                cache: None,
            });
        self.fbs.pipelines.insert((reload, samples), pipeline);
    }
}

/// A color-only render pass over `view`.
fn color_pass<'e>(
    enc: &'e mut wgpu::CommandEncoder,
    view: &wgpu::TextureView,
    load: wgpu::LoadOp<wgpu::Color>,
) -> wgpu::RenderPass<'e> {
    enc.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("pw64 fb helper"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load,
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
    })
}

/// Clears `view` to opaque black.
fn clear_pass(enc: &mut wgpu::CommandEncoder, view: &wgpu::TextureView) {
    color_pass(enc, view, wgpu::LoadOp::Clear(wgpu::Color::BLACK));
}

#[cfg(test)]
mod tests {
    use super::super::tests::{device, red_green_frame};
    use super::*;
    use crate::frame::Wide;
    use crate::{RenderOptions, Widescreen};

    const RED: [u8; 3] = [255, 0, 0];
    const GREEN: [u8; 3] = [0, 255, 0];
    const BLUE: [u8; 3] = [0, 0, 255];
    const BLACK: [u8; 3] = [0, 0, 0];
    const WHITE: [u8; 3] = [255, 255, 255];

    fn renderer(msaa: u32, widescreen: Widescreen) -> Option<Renderer> {
        let (device, queue) = device()?;
        let options = RenderOptions {
            msaa,
            widescreen,
            fill_view: false,
            ..Default::default()
        };
        Some(Renderer::new(
            &device,
            &queue,
            wgpu::TextureFormat::Rgba8Unorm,
            options,
        ))
    }

    /// One Fill draw of `color` inside the N64 `scissor` (the frame's
    /// vertices cover the whole screen), classed `wide`.
    fn rect(scissor: [f32; 4], color: [f32; 3], wide: Wide) -> Frame {
        let mut f = red_green_frame();
        f.draws.truncate(1);
        let d = &mut f.draws[0];
        d.scissor = scissor;
        d.uniforms.fill = [color[0], color[1], color[2], 1.0];
        d.wide = wide;
        f
    }

    fn px(img: &[u8], w: u32, x: u32, y: u32) -> [u8; 3] {
        let o = ((y * w + x) * 4) as usize;
        [img[o], img[o + 1], img[o + 2]]
    }

    /// A second task that only draws a small rect (no clear) keeps the
    /// first task's picture around it — the snap.c photo grid pattern —
    /// with and without MSAA (reload path). Tasks alternate between two
    /// framebuffers (double buffering), so the shared MSAA attachment never
    /// happens to hold the right picture already.
    #[test]
    fn draw_keeps_previous_contents() {
        for msaa in [1, 4] {
            let Some(mut r) = renderer(msaa, Widescreen::Off) else {
                eprintln!("no GPU adapter; skipped");
                return;
            };
            let (w, h) = (64, 48);
            r.set_fb_size((w, h));
            assert!(!r.fb_exists(1));
            r.fb_draw(1, &red_green_frame());
            // fb 2: separate, starts black.
            r.fb_draw(
                2,
                &rect([0.0, 0.0, 10.0, 10.0], [0.0, 0.0, 1.0], Wide::Fixed),
            );
            r.fb_draw(
                1,
                &rect([100.0, 100.0, 140.0, 140.0], [0.0, 0.0, 1.0], Wide::Fixed),
            );
            r.fb_draw(1, &Frame::default()); // empty task: no change
            let img = r.fb_read_rgba(1).unwrap();
            assert_eq!(px(&img, w, 4, 4), RED, "msaa {msaa}: left kept");
            assert_eq!(px(&img, w, 60, 44), GREEN, "msaa {msaa}: right kept");
            assert_eq!(px(&img, w, 24, 24), BLUE, "msaa {msaa}: new rect");
            let img2 = r.fb_read_rgba(2).unwrap();
            assert_eq!(px(&img2, w, 40, 40), BLACK, "msaa {msaa}: fresh fb black");
            // (0,0) is the VI overscan (black); the rect shows from y1 on.
            assert_eq!(px(&img2, w, 0, 0), BLACK, "msaa {msaa}: overscan black");
            assert_eq!(px(&img2, w, 0, 1), BLUE, "msaa {msaa}: rect below overscan");
        }
    }

    /// `uvCopyFrameBuf`: the copy carries the source picture; later draws
    /// into either target stay separate.
    #[test]
    fn copy_duplicates_a_target() {
        let Some(mut r) = renderer(1, Widescreen::Off) else {
            eprintln!("no GPU adapter; skipped");
            return;
        };
        let (w, h) = (64, 48);
        r.set_fb_size((w, h));
        r.fb_draw(1, &red_green_frame());
        r.fb_apply(&FbOp::Copy { dst: 2, src: 1 });
        r.fb_apply(&FbOp::Draw {
            fb: 2,
            frame: Arc::new(rect(
                [100.0, 100.0, 140.0, 140.0],
                [0.0, 0.0, 1.0],
                Wide::Fixed,
            )),
        });
        let (a, b) = (r.fb_read_rgba(1).unwrap(), r.fb_read_rgba(2).unwrap());
        assert_eq!(px(&a, w, 24, 24), RED, "source untouched");
        assert_eq!(px(&b, w, 24, 24), BLUE);
        assert_eq!(px(&b, w, 4, 4), RED, "copied");
        assert_eq!(px(&b, w, 60, 44), GREEN, "copied");
        // Copy from a never-drawn fb = black.
        r.fb_copy(1, 9);
        assert_eq!(px(&r.fb_read_rgba(1).unwrap(), w, 4, 4), BLACK);
    }

    /// VI overscan (`vi_border`): the game's letterbox bars end one N64
    /// pixel short of the top/right edges (RDP top-left-corner coverage);
    /// those pixels keep boot garbage in our persistent targets, so the
    /// readback blacks them (like the TV's overscan) while the interior —
    /// including the game-drawn left/bottom edges — survives.
    #[test]
    fn vi_overscan_black() {
        let Some(mut r) = renderer(1, Widescreen::Off) else {
            eprintln!("no GPU adapter; skipped");
            return;
        };
        let (w, h) = (64, 48);
        r.set_fb_size((w, h));
        // White full-screen fill, like the boot/flash screens that leave the
        // garbage the user sees.
        r.fb_draw(
            1,
            &rect([0.0, 0.0, 320.0, 240.0], [1.0, 1.0, 1.0], Wide::Fixed),
        );
        let img = r.fb_read_rgba(1).unwrap();
        assert_eq!(px(&img, w, 0, 0), BLACK, "top overscan row");
        assert_eq!(px(&img, w, 32, 0), BLACK, "top overscan row (middle)");
        assert_eq!(px(&img, w, 63, 24), BLACK, "right overscan column");
        assert_eq!(px(&img, w, 63, 0), BLACK, "overscan corner");
        assert_eq!(px(&img, w, 32, 1), WHITE, "row 1 drawn");
        assert_eq!(px(&img, w, 62, 24), WHITE, "column 62 drawn");
        assert_eq!(px(&img, w, 0, 24), WHITE, "left edge drawn by the game");
        assert_eq!(px(&img, w, 32, 47), WHITE, "bottom edge drawn by the game");
        // The border matches one N64 pixel at the fb's scale: 48/240 = 0 → 1.
        assert_eq!(
            Renderer::vi_border([0, 0, w, h], true, false)[0],
            [0, 0, 64, 1]
        );
        assert_eq!(
            Renderer::vi_border([0, 0, w, h], true, false)[1],
            [63, 0, 1, 48]
        );
        // Fractional scale rounds up (1080 / 240 = 4.5); widescreen keeps
        // the right column.
        assert_eq!(
            Renderer::vi_border([0, 0, 1440, 1080], true, false)[1][2],
            5
        );
        assert_eq!(
            Renderer::vi_border([0, 0, 1920, 1080], false, false)[1][2],
            0
        );
        // Filled (fill view: a world draw spanned the view vertically):
        // nothing left to hide.
        assert_eq!(
            Renderer::vi_border([0, 0, w, h], true, true)[0],
            [0, 0, 0, 0]
        );
        assert_eq!(Renderer::vi_border([0, 0, w, h], true, true)[1][2], 0);
        assert_eq!(Renderer::vi_border([0, 0, w, h], false, true)[1][2], 0);
    }

    /// Fill view (S17): a world draw spanning the view vertically marks the
    /// framebuffer "filled": the world covers the VI overscan strips, so
    /// the readback keeps the top row / right column. The flag survives
    /// empty tasks and copies and is cleared by a later non-extend task.
    #[test]
    fn fill_view_marks_a_framebuffer_filled() {
        let Some((device, queue)) = device() else {
            eprintln!("no GPU adapter; skipped");
            return;
        };
        let mut r = Renderer::new(
            &device,
            &queue,
            wgpu::TextureFormat::Rgba8Unorm,
            RenderOptions {
                msaa: 1,
                widescreen: Widescreen::Off,
                fill_view: true,
                ..Default::default()
            },
        );
        let (w, h) = (64, 48);
        r.set_fb_size((w, h));
        let full = [0.0, 0.0, 320.0, 240.0];
        r.fb_draw(1, &rect(full, [1.0, 0.0, 0.0], Wide::Extend));
        let img = r.fb_read_rgba(1).unwrap();
        assert_eq!(px(&img, w, 0, 0), RED, "filled: top row drawn over");
        assert_eq!(px(&img, w, 63, 0), RED, "filled: right column drawn over");
        // An empty task keeps the flag...
        r.fb_draw(1, &Frame::default());
        let img = r.fb_read_rgba(1).unwrap();
        assert_eq!(px(&img, w, 0, 0), RED, "empty task keeps filled");
        // ...a non-extend task clears it (the strips hide again).
        r.fb_draw(
            1,
            &rect([100.0, 100.0, 140.0, 140.0], [0.0, 0.0, 1.0], Wide::Fixed),
        );
        let img = r.fb_read_rgba(1).unwrap();
        assert_eq!(px(&img, w, 0, 0), BLACK, "non-extend task clears filled");
        assert_eq!(px(&img, w, 24, 24), BLUE);
        // The copy carries the flag with the pixels.
        r.fb_draw(1, &rect(full, [1.0, 0.0, 0.0], Wide::Extend));
        r.fb_apply(&FbOp::Copy { dst: 2, src: 1 });
        let img2 = r.fb_read_rgba(2).unwrap();
        assert_eq!(px(&img2, w, 0, 0), RED, "copy carries filled");
    }

    /// Widescreen: margins keep what a task that draws there left, but a
    /// task drawing only inside 4:3 gets black margins (as before
    /// persistence); the 4:3 area persists either way.
    #[test]
    fn widescreen_margins() {
        for msaa in [1, 4] {
            let Some(mut r) = renderer(msaa, Widescreen::Aspect(16.0 / 9.0)) else {
                eprintln!("no GPU adapter; skipped");
                return;
            };
            let (w, h) = (128, 72); // 4:3 area = x 16..112
            r.set_fb_size((w, h));
            // Full-width fill (spans the main view → stretched).
            let full = [0.0, 0.0, 320.0, 240.0];
            r.fb_draw(1, &rect(full, [1.0, 0.0, 0.0], Wide::Stretch([0.0, 320.0])));
            let img = r.fb_read_rgba(1).unwrap();
            assert_eq!(px(&img, w, 2, 36), RED, "msaa {msaa}: stretched fill");
            // Edge-to-edge again (blue), then a small 4:3-only rect.
            r.fb_draw(1, &rect(full, [0.0, 0.0, 1.0], Wide::Stretch([0.0, 320.0])));
            r.fb_draw(
                1,
                &rect([150.0, 100.0, 170.0, 140.0], [0.0, 1.0, 0.0], Wide::Fixed),
            );
            let img = r.fb_read_rgba(1).unwrap();
            assert_eq!(px(&img, w, 2, 36), BLACK, "msaa {msaa}: margin cleared");
            assert_eq!(px(&img, w, 125, 36), BLACK, "msaa {msaa}: margin cleared");
            assert_eq!(px(&img, w, 20, 36), BLUE, "msaa {msaa}: 4:3 area kept");
            assert_eq!(px(&img, w, 64, 36), GREEN, "msaa {msaa}: new rect");
        }
    }

    /// Present letterboxes the target into the window; resizing the
    /// targets resamples their contents.
    #[test]
    fn present_and_resize() {
        let Some(mut r) = renderer(1, Widescreen::Off) else {
            eprintln!("no GPU adapter; skipped");
            return;
        };
        r.set_fb_size((64, 48));
        r.fb_draw(1, &red_green_frame());
        r.set_fb_size((128, 96));
        let img = r.fb_read_rgba(1).unwrap();
        assert_eq!(px(&img, 128, 32, 48), RED, "resampled");
        assert_eq!(px(&img, 128, 96, 48), GREEN, "resampled");
        // A 200×96 window: 4:3 area 128 wide at x 36.
        assert_eq!(r.output_rect((200, 96)), [36, 0, 128, 96]);
        let win = r.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("fb present test"),
            size: wgpu::Extent3d {
                width: 200,
                height: 96,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = win.create_view(&Default::default());
        r.fb_present(Some(1), &view, wgpu::FilterMode::Linear);
        let img = super::super::read_rgba(&r.device, &r.queue, &win);
        assert_eq!(px(&img, 200, 10, 48), BLACK, "pillarbox");
        assert_eq!(px(&img, 200, 40, 48), RED);
        assert_eq!(px(&img, 200, 160, 48), GREEN);
        r.fb_present(None, &view, wgpu::FilterMode::Linear);
        let img = super::super::read_rgba(&r.device, &r.queue, &win);
        assert_eq!(px(&img, 200, 40, 48), BLACK, "nothing shown");
    }
}
