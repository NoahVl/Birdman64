//! CPU-written framebuffer pixels (snow) as a FILL-rect display list.
//!
//! Snowy levels draw snowflakes straight into RDRAM: `snowDraw`
//! (decomp/src/app/snow.c) is called via `uvGfxSetCallback` with the
//! previous, finished framebuffer and writes `0xFFFF` into `fb16[idx]`
//! for every index in its list (idx = y*320 + x). The HLE keeps
//! framebuffers as GPU targets, so those writes are invisible there;
//! [`fill_pixels_dl`] re-encodes the same pixels as a display list that
//! [`crate::Interpreter`] turns into 1-pixel FILL rectangles, which
//! `Renderer::fb_draw` composites onto that framebuffer's target.

use crate::memory::VecMemory;

/// A display list that fills one pixel per entry of `idx` (y*320 + x,
/// entries ≥ 320*240 skipped) in `color` on the RGBA16 framebuffer at
/// address `fb`. Pushes the words onto `mem` and returns the list
/// address for [`crate::Interpreter::run`].
///
/// Word layouts are the real F3D encodings (decomp gbi.h, verified in
/// `interp.rs` tests): `G_SETCIMG`/`G_SETSCISSOR` field orders, the
/// `G_SETOTHERMODE_H` cycle-type word from
/// `fill_rect_on_depth_image_clears_depth`, and `G_FILLRECT` with the
/// FILL mode's inclusive lower-right edge (so (x,y)-(x,y) covers exactly
/// one pixel).
pub fn fill_pixels_dl(mem: &mut VecMemory, fb: u32, idx: &[u32], color: u16) -> u32 {
    let mut words: Vec<u32> = Vec::with_capacity(10 + idx.len() * 2);
    // G_SETCIMG: RGBA (fmt 0), 16 bpp (siz 2), width 320, address `fb`.
    // Required: `fill_rect` turns a FILL rect into a *depth clear* when
    // the colour image equals the z image — a fresh interpreter has both
    // at 0, so the cimg must be set (the z image stays 0).
    words.push(0xFF10_013F);
    words.push(fb);
    words.push(0xE700_0000); // G_RDPPIPESYNC
    words.push(0);
    // G_SETOTHERMODE_H: cycle type = FILL (the exact word the
    // `fill_rect_on_depth_image_clears_depth` test uses).
    words.push(0xBA00_1402);
    words.push(0x0030_0000);
    // G_SETSCISSOR: mode 0, (0,0)-(320,240) in quarter pixels (10.2).
    words.push(0xED00_0000);
    words.push(((320 * 4) << 12) | (240 * 4));
    // G_SETFILLCOLOR: the RGBA5551 fill word is the high half.
    words.push(0xF700_0000);
    words.push(((color as u32) << 16) | color as u32);
    for &i in idx {
        let (x, y) = (i % 320, i / 320);
        if y >= 240 {
            continue;
        }
        // G_FILLRECT (x,y)-(x,y): w0 carries the lower-right corner,
        // w1 the upper-left, in quarter pixels. FILL rects include the
        // lower-right edge (`fill_rect` adds 1.0), so this covers
        // exactly the one pixel.
        let q = ((x * 4) << 12) | (y * 4);
        words.push(0xF600_0000 | q);
        words.push(q);
    }
    words.push(0xB800_0000); // G_ENDDL
    words.push(0);
    mem.push_words(&words)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::ShaderMode;
    use crate::interp::Interpreter;

    /// Pixels {(0,0), (5,7), (319,239)} plus one index with y = 240 draw
    /// exactly 3 one-pixel FILL squares; the y = 240 index draws nothing.
    #[test]
    fn fill_pixels_draw_one_pixel_squares() {
        let idx = [0, 7 * 320 + 5, 239 * 320 + 319, 240 * 320 + 5];
        let mut mem = VecMemory::default();
        let dl = fill_pixels_dl(&mut mem, 0x1000, &idx, 0xFFFF);
        let f = Interpreter::new().run(&mem, dl);
        // Every draw is a FILL-mode fill, not a depth clear.
        assert!(!f.draws.is_empty());
        for d in &f.draws {
            assert_eq!(d.pipeline.shader.mode, ShaderMode::Fill);
            assert_ne!(d.pipeline.shader.mode, ShaderMode::DepthClear);
            assert_eq!(d.scissor, [0.0, 0.0, 320.0, 240.0]);
        }
        // 3 rects × 6 vertices: the y = 240 index produced nothing.
        let verts: u32 = f.draws.iter().map(|d| d.vertex_count).sum();
        assert_eq!(verts, 3 * 6);
        // Every vertex corner lies in one of the 3 squares (x = pos.xy /
        // pos.w covers [x, x+1] × [y, y+1] for a FILL rect).
        let squares = [(0.0, 0.0), (5.0, 7.0), (319.0, 239.0)];
        let mut covered = squares.map(|_| false);
        for v in &f.vertices {
            let (x, y) = (v.pos[0] / v.pos[3], v.pos[1] / v.pos[3]);
            let mut inside = false;
            for (n, &(sx, sy)) in squares.iter().enumerate() {
                if (sx..=sx + 1.0).contains(&x) && (sy..=sy + 1.0).contains(&y) {
                    inside = true;
                    // A square's own corner marks it as drawn.
                    if (x == sx || x == sx + 1.0) && (y == sy || y == sy + 1.0) {
                        covered[n] = true;
                    }
                }
            }
            assert!(inside, "vertex at ({x}, {y}) outside every square");
        }
        assert!(covered.iter().all(|c| *c), "a square has no corners");
    }
}
