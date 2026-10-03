//! The game's own N64 controller graphic (the attract demo's input
//! display, `hudDemoController` in `decomp/src/app/hud.c`), loaded at run
//! time from the player's ROM for the settings Controls page (`pad_art`).
//!
//! The demo draws UVBT blits as sprites: the controller body (blit 0x0B),
//! the stick knob (0x0C, moved 4 px per unit of stick) and, per pressed
//! button, a cyan overlay at a fixed offset from the body's top-left: C
//! buttons 0x16, A/B 0x17, D-pad 0x18, R 0x19, L 0x1A and a yellow "Z"
//! burst 0x1B below the left prong. START has no overlay in the game; we
//! reuse the A/B one on the Start button. Nothing is embedded in the exe.

use anyhow::{Context, Result, ensure};
use egui::ColorImage;
use pw64_formats::Uvbt;
use pw64_rom::Form;
use pw64_rom::fs::{FS_BASE_US, INDEX_OFFSET_US};
use std::sync::OnceLock;

/// UVBT type indices (US `BLIT_ID_*`, `uv_sprite.h`).
const BODY: usize = 0x0B;
const KNOB: usize = 0x0C;
const HL_C: usize = 0x16;
const HL_AB: usize = 0x17;
const HL_DPAD: usize = 0x18;
const HL_R: usize = 0x19;
const HL_L: usize = 0x1A;
const HL_Z: usize = 0x1B;
const IDS: [usize; 8] = [BODY, KNOB, HL_C, HL_AB, HL_DPAD, HL_R, HL_L, HL_Z];

/// Transparent border around the 80×79 drawing (body + Z burst) so the
/// highlight glow is not clipped, in native pixels.
const MARGIN: usize = 3;
/// Native canvas: body 80×77, the Z burst (26×27 at y 52) reaches y 79.
const CANVAS_W: usize = 80 + 2 * MARGIN;
const CANVAS_H: usize = 79 + 2 * MARGIN;
/// Stick knob rest position (`hudDemoController`: `stick * 4 + 37/39`).
const KNOB_X: i32 = 37;
const KNOB_Y: i32 = 39;
const KNOB_TRAVEL: i32 = 4;

/// The decoded blits (straight RGBA8), indexed like `IDS`.
pub struct Sprites {
    imgs: Vec<Sprite>,
}

struct Sprite {
    w: usize,
    h: usize,
    rgba: Vec<u8>,
}

/// The sprites from the loaded ROM, decoded once. `None` while no ROM is
/// loaded (tests) or if the assets don't parse: callers fall back to the
/// SVG drawing.
pub fn get() -> Option<&'static Sprites> {
    static S: OnceLock<Option<Sprites>> = OnceLock::new();
    if let Some(s) = S.get() {
        return s.as_ref();
    }
    // Don't cache "no ROM yet": only decide once the ROM is there.
    let rom = pw64_platform::pi::rom_bytes()?;
    S.get_or_init(|| {
        Sprites::from_rom(rom)
            .inspect_err(|e| eprintln!("pad art: game controller sprites unavailable: {e:#}"))
            .ok()
    })
    .as_ref()
}

/// The overlay sprite and its offset from the body's top-left for a Bind
/// slot (`hudDemoContButton` positions), or the knob displacement for a
/// stick slot.
enum Mark {
    Overlay(usize, i32, i32),
    Stick(i32, i32),
}

fn mark(slot: &str) -> Option<Mark> {
    use Mark::*;
    Some(match slot {
        "A" => Overlay(HL_AB, 56, 27),
        "B" => Overlay(HL_AB, 50, 21),
        // Not in the demo: the A/B overlay centred on the red Start button
        // (centre measured at 40,26; the A/B overlay is 10×10).
        "START" => Overlay(HL_AB, 35, 21),
        "C_UP" => Overlay(HL_C, 62, 14),
        "C_DOWN" => Overlay(HL_C, 62, 23),
        "C_LEFT" => Overlay(HL_C, 57, 18),
        "C_RIGHT" => Overlay(HL_C, 66, 18),
        "UP" => Overlay(HL_DPAD, 14, 18),
        "DOWN" => Overlay(HL_DPAD, 14, 28),
        "LEFT" => Overlay(HL_DPAD, 9, 23),
        "RIGHT" => Overlay(HL_DPAD, 18, 23),
        "L" => Overlay(HL_L, 7, 6),
        "R" => Overlay(HL_R, 56, 6),
        "Z" => Overlay(HL_Z, 3, 52),
        "STICK_UP" => Stick(0, -KNOB_TRAVEL),
        "STICK_DOWN" => Stick(0, KNOB_TRAVEL),
        "STICK_LEFT" => Stick(-KNOB_TRAVEL, 0),
        "STICK_RIGHT" => Stick(KNOB_TRAVEL, 0),
        _ => return None,
    })
}

/// Width / height of the composed image.
pub const ASPECT: f32 = CANVAS_W as f32 / CANVAS_H as f32;

impl Sprites {
    /// Reads the UVBT files by walking the filesystem index (like
    /// `pw64_rom::Filesystem::open`, without copying/rehashing the ROM).
    pub fn from_rom(rom: &[u8]) -> Result<Self> {
        let index = Form::parse(rom, INDEX_OFFSET_US).context("filesystem index")?;
        let tabl = index.block(b"TABL").context("index has no TABL")?;
        let mut found: Vec<Option<Sprite>> = IDS.iter().map(|_| None).collect();
        let (mut off, mut n_uvbt) = (FS_BASE_US, 0usize);
        for pair in tabl.data.as_chunks::<8>().0 {
            let tag = &pair[..4];
            let size = u32::from_be_bytes(pair[4..8].try_into().unwrap()) as usize;
            if tag == b"UVBT" {
                if let Some(i) = IDS.iter().position(|&id| id == n_uvbt) {
                    let form = Form::parse(rom, off).with_context(|| format!("UVBT {n_uvbt}"))?;
                    let img = Uvbt::parse(&form)?.decode()?;
                    found[i] = Some(Sprite {
                        w: img.width as usize,
                        h: img.height as usize,
                        rgba: img.rgba,
                    });
                }
                n_uvbt += 1;
            }
            off += size;
        }
        let imgs = found
            .into_iter()
            .zip(IDS)
            .map(|(s, id)| s.with_context(|| format!("UVBT {id} missing")))
            .collect::<Result<Vec<_>>>()?;
        ensure!(
            imgs[0].w == 80 && imgs[0].h == 77,
            "controller blit is {}x{}, expected 80x77",
            imgs[0].w,
            imgs[0].h
        );
        Ok(Self { imgs })
    }

    /// Width of the composed image at scale 1.
    pub fn native_width(&self) -> usize {
        CANVAS_W
    }

    fn sprite(&self, id: usize) -> &Sprite {
        &self.imgs[IDS.iter().position(|&i| i == id).unwrap()]
    }

    /// The controller with `slot` highlighted (upper-case Bind slot name;
    /// unknown or `None` = plain), `scale`× nearest-neighbour upscaled.
    /// The highlight is the game's own overlay (or the moved stick knob)
    /// over a soft `accent` glow.
    pub fn compose(&self, slot: Option<&str>, accent: [u8; 3], scale: usize) -> ColorImage {
        let k = scale.max(1);
        let (w, h) = (CANVAS_W * k, CANVAS_H * k);
        let mut base = vec![0u8; w * h * 4];
        blit(&mut base, w, self.sprite(BODY), 0, 0, k);
        let m = slot.and_then(mark);
        let (kx, ky) = match m {
            Some(Mark::Stick(dx, dy)) => (KNOB_X + dx, KNOB_Y + dy),
            _ => (KNOB_X, KNOB_Y),
        };
        // The highlighted layer goes on a separate canvas so its glow can be
        // drawn between the body and it.
        let mut top = vec![0u8; w * h * 4];
        let (canvas, sprite, x, y) = match m {
            Some(Mark::Overlay(id, x, y)) => {
                blit(&mut base, w, self.sprite(KNOB), kx, ky, k);
                (&mut top, self.sprite(id), x, y)
            }
            Some(Mark::Stick(..)) => (&mut top, self.sprite(KNOB), kx, ky),
            None => (&mut base, self.sprite(KNOB), kx, ky),
        };
        blit(canvas, w, sprite, x, y, k);
        if m.is_some() {
            glow(&mut base, &top, w, h, accent, k);
            over(&mut base, &top);
        }
        ColorImage::from_rgba_unmultiplied([w, h], &base)
    }
}

/// Copies `s` (straight alpha, 1-bit in practice) onto `dst` at native
/// position (`x`,`y`) relative to the body, scaled `k`× (nearest).
fn blit(dst: &mut [u8], dst_w: usize, s: &Sprite, x: i32, y: i32, k: usize) {
    let dst_h = dst.len() / 4 / dst_w;
    for sy in 0..s.h {
        for sx in 0..s.w {
            let p = &s.rgba[(sy * s.w + sx) * 4..][..4];
            if p[3] == 0 {
                continue;
            }
            let nx = x + MARGIN as i32 + sx as i32;
            let ny = y + MARGIN as i32 + sy as i32;
            if nx < 0 || ny < 0 {
                continue;
            }
            for dy in 0..k {
                for dx in 0..k {
                    let (px, py) = (nx as usize * k + dx, ny as usize * k + dy);
                    if px < dst_w && py < dst_h {
                        let o = (py * dst_w + px) * 4;
                        blend(&mut dst[o..o + 4], p);
                    }
                }
            }
        }
    }
}

/// Straight-alpha "over".
fn blend(d: &mut [u8], s: &[u8]) {
    let sa = s[3] as f32 / 255.0;
    let da = d[3] as f32 / 255.0;
    let oa = sa + da * (1.0 - sa);
    if oa <= 0.0 {
        return;
    }
    for c in 0..3 {
        let v = (s[c] as f32 * sa + d[c] as f32 * da * (1.0 - sa)) / oa;
        d[c] = v.round() as u8;
    }
    d[3] = (oa * 255.0).round() as u8;
}

fn over(dst: &mut [u8], top: &[u8]) {
    for (d, s) in dst
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(top.as_chunks::<4>().0)
    {
        if s[3] != 0 {
            blend(d, s);
        }
    }
}

/// A soft `accent` halo around the opaque pixels of `top`, painted onto
/// `dst`: the alpha mask blurred by three box passes (≈ Gaussian) with a
/// radius of ~1.5 native pixels, boosted so the halo hugs the sprite.
fn glow(dst: &mut [u8], top: &[u8], w: usize, h: usize, accent: [u8; 3], k: usize) {
    let mut m: Vec<f32> = top
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| p[3] as f32 / 255.0)
        .collect();
    let r = (k * 3 / 4).max(1);
    for _ in 0..3 {
        box_blur(&mut m, w, h, r);
    }
    let [ar, ag, ab] = accent;
    for (d, a) in dst.as_chunks_mut::<4>().0.iter_mut().zip(m) {
        let a = (a * 2.2).min(0.95);
        if a > 0.004 {
            blend(d, &[ar, ag, ab, (a * 255.0) as u8]);
        }
    }
}

/// Separable box blur of radius `r` (in place, clamped edges).
fn box_blur(m: &mut [f32], w: usize, h: usize, r: usize) {
    let n = (2 * r + 1) as f32;
    let mut line = Vec::new();
    for y in 0..h {
        line.clear();
        line.extend_from_slice(&m[y * w..(y + 1) * w]);
        for x in 0..w {
            let (a, b) = (x.saturating_sub(r), (x + r).min(w - 1));
            m[y * w + x] = line[a..=b].iter().sum::<f32>() / n;
        }
    }
    for x in 0..w {
        line.clear();
        line.extend((0..h).map(|y| m[y * w + x]));
        for y in 0..h {
            let (a, b) = (y.saturating_sub(r), (y + r).min(h - 1));
            m[y * w + x] = line[a..=b].iter().sum::<f32>() / n;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ROM next to the repo, if any (tests skip without one).
    fn rom() -> Option<pw64_rom::Rom> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let cand = std::fs::read_dir(root.join("rom"))
            .ok()?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .chain([root.join("decomp/baserom.us.z64")]);
        cand.into_iter().find_map(|p| pw64_rom::Rom::load(&p).ok())
    }

    /// With a ROM: the sprites load, the decoded body has a clean
    /// (non-zigzag) left edge, and every slot's highlight changes pixels.
    #[test]
    fn sprites_from_rom() {
        let Some(rom) = rom() else { return };
        let s = Sprites::from_rom(rom.bytes()).expect("sprites");
        let body = s.sprite(BODY);
        // Left edge: first opaque x per row changes smoothly once
        // `Uvbt::decode` undoes the TMEM shuffle (swizzled rows jump by 2+ px).
        let first = |y: usize| (0..body.w).find(|&x| body.rgba[(y * body.w + x) * 4 + 3] != 0);
        let jumps = (31..60)
            .filter(|&y| match (first(y), first(y + 1)) {
                (Some(a), Some(b)) => a.abs_diff(b) > 2,
                _ => false,
            })
            .count();
        assert!(jumps <= 2, "zigzag left edge: {jumps} jumps");
        let base = s.compose(None, [0, 92, 128], 2);
        for slot in crate::pad_art::SLOTS {
            let img = s.compose(Some(slot), [0, 92, 128], 2);
            assert_eq!(img.size, base.size);
            assert_ne!(img.pixels, base.pixels, "{slot}");
        }
    }

    /// `PW64_PAD_ART_PREVIEW=1 cargo test -p birdman64 pad_sprites` writes
    /// `tmp/pad_rom_<slot>.png` (needs a ROM; ROM-derived, never commit).
    #[test]
    fn preview_pngs() {
        if std::env::var_os("PW64_PAD_ART_PREVIEW").is_none() {
            return;
        }
        let Some(rom) = rom() else { return };
        let s = Sprites::from_rom(rom.bytes()).unwrap();
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tmp");
        std::fs::create_dir_all(&dir).unwrap();
        for slot in [
            None,
            Some("A"),
            Some("START"),
            Some("C_RIGHT"),
            Some("STICK_LEFT"),
            Some("Z"),
            Some("L"),
            Some("UP"),
        ] {
            let img = s.compose(slot, [0, 92, 128], 4);
            let name = format!("pad_rom_{}.png", slot.unwrap_or("base"));
            crate::pad_art::tests::write_preview(&dir.join(name), &img);
        }
    }
}
