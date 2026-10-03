//! N64 controller drawing for the settings screen's Controls page
//! (`pad_art::draw`). With a ROM loaded it is the game's own controller
//! graphic from the attract demo (`pad_sprites`: UVBT blits read from the
//! player's ROM at run time, the game's cyan "pressed" overlay plus an
//! accent glow for the highlight). Without one (tests, or assets that
//! don't parse) it falls back to an original hand-authored SVG
//! (`assets/n64_pad.svg`: no logos, wordmarks or trademarks) rasterized
//! with resvg into an egui texture. Every input in it has an id
//! `btn-<slot>` (the Bind slot names of `input::*_SLOTS`; the four
//! `STICK_*` slots share `btn-STICK` and each adds an `ind-STICK_*` arrow).
//! A highlight is a second rasterization with a `<style>` appended that
//! restyles that element (accent fill, white outline, glow). Rasterized
//! images are cached per (slot, pixel width, accent), so a frame only draws
//! a cached texture.

use egui::{Color32, ColorImage, Sense, TextureHandle, TextureOptions, vec2};
use resvg::{tiny_skia, usvg};

/// The SVG source (viewBox 0 0 440 360).
const SVG: &str = include_str!("../assets/n64_pad.svg");
/// SVG viewBox size: the drawing's aspect ratio.
const VIEW_W: f32 = 440.0;
const VIEW_H: f32 = 360.0;
/// Widest the drawing gets on the panel, in points.
const MAX_W: f32 = 380.0;
/// Rasterized variants kept (most recently used first).
const CACHE_LEN: usize = 6;

/// Paints the controller into `ui`, scaled to the panel width (at most
/// `MAX_W`). `highlight` is the selected Bind row's slot name (`"A"`,
/// `"C_UP"`, `"STICK_LEFT"`, ...): that element gets the accent fill, a
/// white outline and a glow; a `STICK_*` slot also shows its direction
/// arrow. Unknown names draw the plain controller.
pub fn draw(ui: &mut egui::Ui, highlight: Option<&str>) {
    let w = ui.available_width().clamp(1.0, MAX_W);
    // The box keeps the SVG's aspect whichever art is shown, so the page
    // layout doesn't depend on whether a ROM is loaded.
    let (rect, _) = ui.allocate_exact_size(vec2(w, w * VIEW_H / VIEW_W), Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }
    let rom = crate::pad_sprites::get().is_some();
    // The ROM art is near-square: fit it (centred) in the box.
    let rect = if rom {
        let h = rect.height().min(rect.width() / crate::pad_sprites::ASPECT);
        egui::Rect::from_center_size(rect.center(), vec2(h * crate::pad_sprites::ASPECT, h))
    } else {
        rect
    };
    let px_w = (rect.width() * ui.ctx().pixels_per_point())
        .round()
        .max(1.0) as u32;
    let slot = highlight
        .map(|h| h.trim().to_ascii_uppercase())
        .filter(|h| element_ids(h).is_some());
    let a = ui.visuals().selection.bg_fill;
    let key = Key {
        slot,
        px_w,
        accent: [a.r(), a.g(), a.b()],
        rom,
    };
    if let Some(tex) = texture(ui.ctx(), key) {
        let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
        ui.painter().image(tex.id(), rect, uv, Color32::WHITE);
    }
}

/// Every Bind slot name both art sources highlight.
#[cfg(test)]
pub const SLOTS: [&str; 18] = [
    "A",
    "B",
    "Z",
    "L",
    "R",
    "START",
    "C_UP",
    "C_DOWN",
    "C_LEFT",
    "C_RIGHT",
    "UP",
    "DOWN",
    "LEFT",
    "RIGHT",
    "STICK_UP",
    "STICK_DOWN",
    "STICK_LEFT",
    "STICK_RIGHT",
];

#[derive(Clone, PartialEq, Eq)]
struct Key {
    slot: Option<String>,
    px_w: u32,
    accent: [u8; 3],
    /// Game sprites from the ROM (else the SVG).
    rom: bool,
}

/// The image for `key`: the ROM sprites upscaled to at least `px_w`
/// (whole-pixel nearest; egui's linear filter takes it down to the exact
/// size), else the rasterized SVG.
fn image(key: &Key) -> Option<ColorImage> {
    if key.rom {
        let s = crate::pad_sprites::get()?;
        let k = (key.px_w as usize).div_ceil(s.native_width()).clamp(1, 8);
        return Some(s.compose(key.slot.as_deref(), key.accent, k));
    }
    rasterize(&svg_source(key.slot.as_deref(), key.accent), key.px_w)
}

/// Texture cache in egui's temp memory (MRU list, `CACHE_LEN` entries).
#[derive(Clone, Default)]
struct Cache(Vec<(Key, TextureHandle)>);

fn texture(ctx: &egui::Context, key: Key) -> Option<TextureHandle> {
    let id = egui::Id::new("pad_art_cache");
    let hit = ctx.memory_mut(|m| {
        let c = &mut m.data.get_temp_mut_or_default::<Cache>(id).0;
        let i = c.iter().position(|(k, _)| *k == key)?;
        let e = c.remove(i);
        let tex = e.1.clone();
        c.insert(0, e);
        Some(tex)
    });
    if hit.is_some() {
        return hit;
    }
    let img = image(&key)?;
    let tex = ctx.load_texture("pad_art", img, TextureOptions::LINEAR);
    ctx.memory_mut(|m| {
        let c = &mut m.data.get_temp_mut_or_default::<Cache>(id).0;
        c.insert(0, (key, tex.clone()));
        c.truncate(CACHE_LEN);
    });
    Some(tex)
}

/// The SVG element ids a Bind slot restyles: its `btn-` element, plus the
/// direction arrow for the stick slots. `None` for unknown slot names.
fn element_ids(slot: &str) -> Option<(&'static str, Option<&'static str>)> {
    Some(match slot {
        "A" => ("btn-A", None),
        "B" => ("btn-B", None),
        "Z" => ("btn-Z", None),
        "L" => ("btn-L", None),
        "R" => ("btn-R", None),
        "START" => ("btn-START", None),
        "C_UP" => ("btn-C_UP", None),
        "C_DOWN" => ("btn-C_DOWN", None),
        "C_LEFT" => ("btn-C_LEFT", None),
        "C_RIGHT" => ("btn-C_RIGHT", None),
        "UP" => ("btn-UP", None),
        "DOWN" => ("btn-DOWN", None),
        "LEFT" => ("btn-LEFT", None),
        "RIGHT" => ("btn-RIGHT", None),
        "STICK_UP" => ("btn-STICK", Some("ind-STICK_UP")),
        "STICK_DOWN" => ("btn-STICK", Some("ind-STICK_DOWN")),
        "STICK_LEFT" => ("btn-STICK", Some("ind-STICK_LEFT")),
        "STICK_RIGHT" => ("btn-STICK", Some("ind-STICK_RIGHT")),
        _ => return None,
    })
}

/// The SVG with a highlight `<style>` for `slot` appended (CSS beats the
/// presentation attributes; ids beat the `.ind` class rule).
fn svg_source(slot: Option<&str>, accent: [u8; 3]) -> String {
    let Some((btn, ind)) = slot.and_then(element_ids) else {
        return SVG.to_owned();
    };
    let [r, g, b] = accent;
    let fill = format!("#{r:02x}{g:02x}{b:02x}");
    // Accent face + white outline and glow; the face's arrow marks and
    // letters turn white so they stay readable on the accent.
    let mut css = format!(
        "#{btn} .face {{ fill: {fill}; fill-opacity: 1; stroke: #ffffff; stroke-width: 2.5 }} \
         #{btn} .mark {{ fill: #ffffff }} #{btn} .glyph {{ stroke: #ffffff }} \
         #{btn} {{ filter: url(#hl-glow) }}"
    );
    if let Some(ind) = ind {
        css.push_str(&format!(
            " #{ind} {{ opacity: 1; filter: url(#hl-glow) }} \
             #{ind} .mark {{ fill: {fill}; stroke: #ffffff }}"
        ));
    }
    SVG.replacen("</svg>", &format!("<style>{css}</style>\n</svg>"), 1)
}

/// Rasterizes `svg` to `px_w` pixels wide (height from the viewBox).
fn rasterize(svg: &str, px_w: u32) -> Option<ColorImage> {
    let tree = usvg::Tree::from_str(svg, &usvg::Options::default()).ok()?;
    let size = tree.size();
    let scale = px_w as f32 / size.width();
    let px_h = (size.height() * scale).round().max(1.0) as u32;
    let mut pm = tiny_skia::Pixmap::new(px_w, px_h)?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pm.as_mut(),
    );
    Some(ColorImage::from_rgba_premultiplied(
        [px_w as usize, px_h as usize],
        pm.data(),
    ))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::input::{GAMEPAD_SLOTS, KEYBOARD_SLOTS};

    fn all_slots() -> impl Iterator<Item = &'static str> {
        let kb = KEYBOARD_SLOTS.iter().map(|(_, n, _)| *n);
        kb.chain(GAMEPAD_SLOTS.iter().map(|(_, n, _)| *n))
    }

    /// Every Bind slot of both tables names elements that exist exactly
    /// once in the SVG.
    #[test]
    fn every_slot_has_an_element() {
        for slot in all_slots() {
            let (btn, ind) = element_ids(slot).unwrap_or_else(|| panic!("slot {slot}"));
            for id in std::iter::once(btn).chain(ind) {
                let n = SVG.matches(&format!("id=\"{id}\"")).count();
                assert_eq!(n, 1, "slot {slot}: id {id} appears {n} times");
            }
        }
    }

    /// The SVG parses; the plain drawing is opaque at the stick centre and
    /// transparent in the corner; every highlight changes pixels (so its
    /// CSS took effect) and an arrow appears for a stick slot.
    #[test]
    fn highlights_restyle_their_element() {
        let base = rasterize(SVG, 220).expect("svg renders");
        assert_eq!(base.size, [220, 180]);
        let px = |img: &ColorImage, x: usize, y: usize| img.pixels[y * img.size[0] + x];
        assert_eq!(px(&base, 110, 94).a(), 255, "stick is opaque");
        assert_eq!(px(&base, 1, 178).a(), 0, "corner is transparent");
        for slot in all_slots() {
            let img = rasterize(&svg_source(Some(slot), [200, 40, 160]), 220).unwrap();
            assert_ne!(img.pixels, base.pixels, "{slot} highlight changed nothing");
        }
        // The STICK_UP arrow sits above the stick gate (viewBox 220,143).
        let up = rasterize(&svg_source(Some("STICK_UP"), [200, 40, 160]), 220).unwrap();
        assert_ne!(px(&up, 110, 72), px(&base, 110, 72), "STICK_UP arrow");
    }

    /// `draw` lays out at the panel width and paints one image (no window).
    #[test]
    fn draw_paints_an_image() {
        for highlight in [None, Some("z"), Some("STICK_UP"), Some("bogus")] {
            let ctx = egui::Context::default();
            let out = ctx.run(Default::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| draw(ui, highlight));
            });
            let meshes = out
                .shapes
                .iter()
                .filter(|c| matches!(&c.shape, egui::Shape::Mesh(m) if m.texture_id != egui::TextureId::default()))
                .count();
            assert_eq!(meshes, 1, "highlight {highlight:?}");
        }
    }

    /// Preview PNGs for eyeballing the art: `PW64_PAD_ART_PREVIEW=1 cargo
    /// test -p birdman64 pad_art` writes `tmp/pad_art_<slot>.png` (420 px wide,
    /// on the settings panel's dark grey).
    #[test]
    fn preview_pngs() {
        if std::env::var_os("PW64_PAD_ART_PREVIEW").is_none() {
            return;
        }
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tmp");
        std::fs::create_dir_all(&dir).unwrap();
        for slot in [
            None,
            Some("A"),
            Some("C_RIGHT"),
            Some("STICK_LEFT"),
            Some("Z"),
            Some("L"),
            Some("UP"),
        ] {
            let img = rasterize(&svg_source(slot, [0, 92, 128]), 420).unwrap();
            let path = dir.join(format!("pad_art_{}.png", slot.unwrap_or("base")));
            write_preview(&path, &img);
        }
    }

    /// Writes `img` (premultiplied) as an RGB PNG on the settings panel's
    /// dark grey, for the preview tests.
    pub(crate) fn write_preview(path: &std::path::Path, img: &ColorImage) {
        let bg = [27u8, 27, 27];
        let mut rgb = Vec::with_capacity(img.pixels.len() * 3);
        for p in &img.pixels {
            let a = p.a() as u16;
            for (c, b) in [p.r(), p.g(), p.b()].into_iter().zip(bg) {
                rgb.push((c as u16 + b as u16 * (255 - a) / 255).min(255) as u8);
            }
        }
        let f = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
        let mut enc = png::Encoder::new(f, img.size[0] as u32, img.size[1] as u32);
        enc.set_color(png::ColorType::Rgb);
        enc.write_header().unwrap().write_image_data(&rgb).unwrap();
    }
}
