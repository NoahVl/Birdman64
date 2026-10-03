//! Matrices and the F3D matrix stack.
//!
//! N64 matrices use the row-vector convention (`v' = v * M`, translation in
//! row 3), which is also how the engine's `Mtx4F` is laid out, so a matrix
//! is stored exactly as in memory: `m[row][col]`.

use pw64_formats::gbi::mtx;

pub type Mat4 = [[f32; 4]; 4];

pub const IDENTITY: Mat4 = [
    [1.0, 0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0, 0.0],
    [0.0, 0.0, 1.0, 0.0],
    [0.0, 0.0, 0.0, 1.0],
];

/// Row-vector product: the result applies `a` first, then `b`.
pub fn mul(a: &Mat4, b: &Mat4) -> Mat4 {
    let mut m = [[0.0; 4]; 4];
    for (i, row) in m.iter_mut().enumerate() {
        for (j, v) in row.iter_mut().enumerate() {
            *v = (0..4).map(|k| a[i][k] * b[k][j]).sum();
        }
    }
    m
}

/// `v * m` for a point (w = 1).
pub fn transform(v: [f32; 3], m: &Mat4) -> [f32; 4] {
    let v = [v[0], v[1], v[2], 1.0];
    [0, 1, 2, 3].map(|j| (0..4).map(|k| v[k] * m[k][j]).sum())
}

/// `n * m` for a direction (upper 3×3 only).
pub fn transform_dir(n: [f32; 3], m: &Mat4) -> [f32; 3] {
    [0, 1, 2].map(|j| (0..3).map(|k| n[k] * m[k][j]).sum())
}

/// Encodes a float matrix as a libultra fixed-point `Mtx` (s15.16; 16
/// integer halves, then 16 fraction halves; big-endian). Mirrors
/// `uvMat4CopyF2L` / `guMtxF2L`.
pub fn to_fixed(m: &Mat4) -> [u8; 64] {
    let mut b = [0u8; 64];
    for (i, row) in m.iter().enumerate() {
        for (j, &e) in row.iter().enumerate() {
            let v = (e as f64 * 65536.0) as i64 as i32;
            let k = (i * 4 + j) * 2;
            b[k..k + 2].copy_from_slice(&((v >> 16) as u16).to_be_bytes());
            b[32 + k..34 + k].copy_from_slice(&(v as u16).to_be_bytes());
        }
    }
    b
}

/// Decodes a fixed-point `Mtx` (see [`to_fixed`]).
pub fn from_fixed(b: &[u8; 64]) -> Mat4 {
    pw64_formats::mtx_fixed_to_f32(b)
}

/// F3D's modelview stack depth: the current matrix (kept in DMEM) plus 10
/// saved ones. Fast3D pushes the current matrix to the RDRAM `dram_stack`
/// and stops at a hard-coded end of `dram_stack + 0x280` (10 × 64 bytes):
/// once full, a `G_MTX_PUSH` skips the save but still loads/multiplies the
/// current matrix (the next `G_POPMTX` then restores one level too high).
/// The demo-pilot models (Kiwi/Ibis, UVMD 0x15D/0x15E) nest 10 parts deep,
/// i.e. exactly 10 pushes, which fit on hardware.
pub const MODELVIEW_DEPTH: usize = 11;

/// Set after the first overflow warning, so a scene that overflows every
/// frame logs once per session (later ones go to `debug`).
static OVERFLOW_WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The RSP matrix state: one projection matrix and a modelview stack.
#[derive(Debug, Clone)]
pub struct MatrixStack {
    pub projection: Mat4,
    /// Bottom..top; never empty.
    modelview: Vec<Mat4>,
    /// Cached `modelview * projection`.
    mvp: Option<Mat4>,
}

impl Default for MatrixStack {
    fn default() -> Self {
        Self {
            projection: IDENTITY,
            modelview: vec![IDENTITY],
            mvp: None,
        }
    }
}

impl MatrixStack {
    pub fn modelview(&self) -> &Mat4 {
        self.modelview.last().unwrap()
    }

    pub fn depth(&self) -> usize {
        self.modelview.len()
    }

    /// `G_MTX` with F3D `params` ([`mtx`] bits). As on the RSP, `MUL`
    /// pre-multiplies: the new matrix is applied before the current one.
    pub fn load(&mut self, params: u8, m: &Mat4) {
        self.mvp = None;
        let load = params & mtx::G_MTX_LOAD != 0;
        if params & mtx::G_MTX_PROJECTION != 0 {
            // PUSH is ignored for the projection (it has no stack in F3D).
            self.projection = if load { *m } else { mul(m, &self.projection) };
            return;
        }
        let top = *self.modelview();
        if params & mtx::G_MTX_PUSH != 0 {
            if self.modelview.len() < MODELVIEW_DEPTH {
                self.modelview.push(top);
            } else if !OVERFLOW_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                log::warn!(
                    "G_MTX push beyond depth {MODELVIEW_DEPTH} ignored, like F3D \
                     (warned once per session)"
                );
            } else {
                log::debug!("G_MTX push beyond depth {MODELVIEW_DEPTH} ignored");
            }
        }
        *self.modelview.last_mut().unwrap() = if load { *m } else { mul(m, &top) };
    }

    /// `G_POPMTX` (modelview). Popping the last entry is a no-op, like the RSP.
    pub fn pop(&mut self) {
        if self.modelview.len() > 1 {
            self.modelview.pop();
            self.mvp = None;
        }
    }

    /// Combined modelview × projection.
    pub fn mvp(&mut self) -> Mat4 {
        *self
            .mvp
            .get_or_insert_with(|| mul(self.modelview.last().unwrap(), &self.projection))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pw64_formats::gbi::mtx::*;

    fn translate(x: f32, y: f32, z: f32) -> Mat4 {
        let mut m = IDENTITY;
        m[3] = [x, y, z, 1.0];
        m
    }

    fn scale(s: f32) -> Mat4 {
        let mut m = IDENTITY;
        for (i, row) in m.iter_mut().enumerate().take(3) {
            row[i] = s;
        }
        m
    }

    #[test]
    fn push_mul_pop() {
        let mut s = MatrixStack::default();
        s.load(G_MTX_LOAD, &translate(10.0, 0.0, 0.0));
        // Push + mul: the child's scale applies before the parent translation.
        s.load(G_MTX_PUSH, &scale(2.0));
        assert_eq!(s.depth(), 2);
        assert_eq!(transform([1.0, 0.0, 0.0], &s.mvp()), [12.0, 0.0, 0.0, 1.0]);
        s.pop();
        assert_eq!(transform([1.0, 0.0, 0.0], &s.mvp()), [11.0, 0.0, 0.0, 1.0]);
        // Popping the bottom entry is ignored.
        s.pop();
        assert_eq!(s.depth(), 1);
    }

    #[test]
    fn projection_is_separate_and_premultiplied() {
        let mut s = MatrixStack::default();
        s.load(G_MTX_PROJECTION | G_MTX_LOAD, &scale(3.0));
        s.load(G_MTX_PROJECTION, &translate(1.0, 0.0, 0.0));
        s.load(G_MTX_LOAD, &translate(0.0, 5.0, 0.0));
        // v * MV * (T * S)
        assert_eq!(transform([0.0; 3], &s.mvp()), [3.0, 15.0, 0.0, 1.0]);
        // PUSH on the projection does not touch the modelview stack.
        s.load(G_MTX_PROJECTION | G_MTX_PUSH | G_MTX_LOAD, &IDENTITY);
        assert_eq!(s.depth(), 1);
    }

    #[test]
    fn push_depth_is_bounded() {
        let mut s = MatrixStack::default();
        for _ in 0..20 {
            s.load(G_MTX_PUSH, &IDENTITY);
        }
        assert_eq!(s.depth(), MODELVIEW_DEPTH);
    }

    #[test]
    fn ten_pushes_fit_and_overflow_still_loads() {
        // Like Fast3D: 10 saved matrices + the current one.
        let mut s = MatrixStack::default();
        for i in 0..10 {
            s.load(G_MTX_PUSH, &translate(1.0, 0.0, 0.0));
            assert_eq!(s.depth(), i + 2);
        }
        assert_eq!(transform([0.0; 3], &s.mvp())[0], 10.0);
        // The 11th push is not saved, but the multiply still applies.
        s.load(G_MTX_PUSH, &translate(1.0, 0.0, 0.0));
        assert_eq!(s.depth(), MODELVIEW_DEPTH);
        assert_eq!(transform([0.0; 3], &s.mvp())[0], 11.0);
        // So the pop restores the 9-push level, one too high (as on hardware).
        s.pop();
        assert_eq!(transform([0.0; 3], &s.mvp())[0], 9.0);
    }

    #[test]
    fn fixed_point_round_trip() {
        let mut m = translate(-1234.5, 0.25, 7.0);
        m[0][1] = -0.5;
        assert_eq!(from_fixed(&to_fixed(&m)), m);
    }
}
