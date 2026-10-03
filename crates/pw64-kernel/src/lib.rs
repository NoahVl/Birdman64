//! Rust ports of decomp functions, one `#[unsafe(no_mangle)] pub extern "C"`
//! per function, replacing the C definition in the final link.
//!
//! Mechanism: `crates/pw64-game/build.rs` reads `crates/pw64-game/ported.txt`
//! and compiles the decomp file that *defines* each listed function with
//! `-D<name>=pw64_c_<name>`, so the C original stays linked under the
//! `pw64_c_` name while every other C file keeps calling `<name>` — which
//! resolves to the Rust port here. `crates/pw64-game/tests/kernel_diff.rs` proves
//! each port bit-identical to the `pw64_c_` original. To port the next
//! function, follow docs/notes/rust-port.md.
//!
//! Ports mirror the C statement by statement: same operation order, same
//! intermediates, no algebraic simplification, no library reimplementation
//! (the C's `uvSqrtF` is called, not `f32::sqrt`). `-ffp-contract=off` in the
//! C flags and Rust's no-auto-fusion guarantee neither side contracts a*b+c.
//!
//! The ports are `pub unsafe extern "C" fn` (the repo convention, cf.
//! pw64-platform swap.rs): they dereference the C's raw pointers, so the Rust
//! safety contract is the C caller's — each argument a valid `Vec3F`/`Mtx4F`,
//! distinct buffers unless a port documents the aliasing it handles (as the
//! decomp `uvMat4Mul` does). The symbol and ABI are unchanged.

// The safety contract of every port is documented once above (C pointer
// semantics); repeating a `# Safety` section 12 times adds noise.
#![allow(clippy::missing_safety_doc)]
#![warn(clippy::undocumented_unsafe_blocks)]

/// Mirrors the decomp `Vec3F` union (decomp/include/kernel/uv_vector.h): the
/// anonymous struct `{ x, y, z }` overlaps `f32 f[3]`. Same 12-byte layout, so
/// the C's `f[i]` accesses agree with the Rust fields; asserted by the tests.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Vec3F {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

/// Mirrors the decomp `Mtx4F` union (decomp/include/kernel/uv_matrix.h): the
/// named fields (`xx, yx, zx, wx, xy, …`) are the anonymous struct's member
/// order and overlap `float m[4][4]`, row-major — `m[r][c]` is field
/// `[r*4 + c]` (`m[0][1]` = `yx`, `m[1][0]` = `xy`, …). Same 64-byte layout;
/// asserted by the tests.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Mtx4F {
    pub xx: f32,
    pub yx: f32,
    pub zx: f32,
    pub wx: f32,
    pub xy: f32,
    pub yy: f32,
    pub zy: f32,
    pub wy: f32,
    pub xz: f32,
    pub yz: f32,
    pub zz: f32,
    pub wz: f32,
    pub xw: f32,
    pub yw: f32,
    pub zw: f32,
    pub ww: f32,
}

unsafe extern "C" {
    /// decomp/src/kernel/math.c: a one-line `sqrtf` wrapper. Kept in C (we
    /// don't reimplement libm maths); links from the pw64-game archive.
    fn uvSqrtF(value: f32) -> f32;
}

/// Mirrors `uvVec3Len` (decomp/src/kernel/vector.c).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn uvVec3Len(v: *mut Vec3F) -> f32 {
    // SAFETY: `v` is a valid Vec3F (C caller contract, see the crate docs).
    let v = unsafe { &*v };
    let x = v.x;
    let y = v.y;
    let z = v.z;
    // SAFETY: `uvSqrtF` is the decomp's pure one-line sqrtf wrapper.
    unsafe { uvSqrtF((x * x) + (y * y) + (z * z)) }
}

/// Mirrors `uvVec2Dot` (decomp/src/kernel/vector.c).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn uvVec2Dot(v0: *mut Vec3F, v1: *mut Vec3F) -> f32 {
    // SAFETY: valid Vec3Fs (C caller contract, see the crate docs).
    let (v0, v1) = unsafe { (&*v0, &*v1) };
    (v0.x * v1.x) + (v0.y * v1.y)
}

/// Mirrors `uvVec3Dot` (decomp/src/kernel/vector.c).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn uvVec3Dot(v0: *mut Vec3F, v1: *mut Vec3F) -> f32 {
    // SAFETY: valid Vec3Fs (C caller contract, see the crate docs).
    let (v0, v1) = unsafe { (&*v0, &*v1) };
    (v0.x * v1.x) + (v0.y * v1.y) + (v0.z * v1.z)
}

/// Mirrors `uvVec3Copy` (decomp/src/kernel/vector.c).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn uvVec3Copy(vdst: *mut Vec3F, vsrc: *mut Vec3F) {
    // SAFETY: valid Vec3Fs; raw pointers (not references) because vdst may
    // alias vsrc: a live `&mut` plus `&` to one buffer would be UB and let
    // rustc reorder the loads.
    unsafe {
        (*vdst).x = (*vsrc).x;
        (*vdst).y = (*vsrc).y;
        (*vdst).z = (*vsrc).z;
    }
}

/// Mirrors `uvVec3ScalarProj` (decomp/src/kernel/vector.c).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn uvVec3ScalarProj(v0: *mut Vec3F, v1: *mut Vec3F) -> f32 {
    // SAFETY: the callees are the pure ported fns on the same valid pointers.
    unsafe {
        let dot = uvVec3Dot(v0, v1);
        let len = uvVec3Len(v0);
        dot / (uvVec3Len(v1) * len)
    }
}

/// Mirrors `uvVec3Cross` (decomp/src/kernel/vector.c).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn uvVec3Cross(vd: *mut Vec3F, va: *mut Vec3F, vb: *mut Vec3F) {
    // SAFETY: valid Vec3Fs (C caller contract, see the crate docs).
    let (va, vb) = unsafe { (&*va, &*vb) };
    let ax = va.x;
    let ay = va.y;
    let az = va.z;
    let bx = vb.x;
    let by = vb.y;
    let bz = vb.z;
    // SAFETY: va/vb were fully read into locals above, so a write through
    // `vd` (which may alias either) is fine.
    let vd = unsafe { &mut *vd };
    vd.x = ay * bz - az * by;
    vd.y = -(ax * bz - az * bx);
    vd.z = ax * by - ay * bx;
}

/// Mirrors `uvVec3Add` (decomp/src/kernel/vector.c).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn uvVec3Add(vd: *mut Vec3F, va: *mut Vec3F, vb: *mut Vec3F) {
    // SAFETY: valid Vec3Fs; raw pointers because the game calls this with
    // vd == va (see uvVec3Copy).
    unsafe {
        (*vd).x = (*va).x + (*vb).x;
        (*vd).y = (*va).y + (*vb).y;
        (*vd).z = (*va).z + (*vb).z;
    }
}

/// Mirrors `uvVec3Mul` (decomp/src/kernel/vector.c).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn uvVec3Mul(vd: *mut Vec3F, va: *mut Vec3F, sb: f32) {
    // SAFETY: valid Vec3Fs; raw pointers because the game calls this with
    // vd == va (see uvVec3Copy).
    unsafe {
        (*vd).x = (*va).x * sb;
        (*vd).y = (*va).y * sb;
        (*vd).z = (*va).z * sb;
    }
}

/// Mirrors `uvVec3Normal` (decomp/src/kernel/vector.c).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn uvVec3Normal(vd: *mut Vec3F, va: *mut Vec3F) -> i32 {
    // SAFETY: `va` is a valid Vec3F (C caller contract, see the crate docs).
    let va = unsafe { &*va };
    let ax = va.x;
    let ay = va.y;
    let az = va.z;
    // SAFETY: `uvSqrtF` is the decomp's pure one-line sqrtf wrapper.
    let len = unsafe { uvSqrtF(ax * ax + ay * ay + az * az) };
    if len == 0.0 {
        return 0;
    }
    let len_inv = 1.0 / len;
    // SAFETY: va was fully read into locals above, so a write through `vd`
    // (which may alias va) is fine.
    let vd = unsafe { &mut *vd };
    vd.x = ax * len_inv;
    vd.y = ay * len_inv;
    vd.z = az * len_inv;
    1
}

/// Mirrors `uvMat4Copy` (decomp/src/kernel/matrix.c).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn uvMat4Copy(dst: *mut Mtx4F, src: *mut Mtx4F) {
    // m[0][0] .. m[3][3] in the C's order, through raw pointers (no
    // references: dst may alias src).
    let (d, s) = (dst.cast::<f32>(), src.cast::<f32>());
    for i in 0..16 {
        // SAFETY: `i < 16` is in bounds of either Mtx4F's 16 f32s; raw
        // pointers because dst may alias src.
        unsafe { *d.add(i) = *s.add(i) };
    }
}

/// Mirrors `uvMat4SetIdentity` (decomp/src/kernel/matrix.c).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn uvMat4SetIdentity(dst: *mut Mtx4F) {
    // SAFETY: `dst` is a valid Mtx4F (C caller contract, see the crate docs).
    let dst = unsafe { &mut *dst };
    dst.xx = 1.0;
    dst.yx = 0.0;
    dst.zx = 0.0;
    dst.wx = 0.0;
    dst.xy = 0.0;
    dst.yy = 1.0;
    dst.zy = 0.0;
    dst.wy = 0.0;
    dst.xz = 0.0;
    dst.yz = 0.0;
    dst.zz = 1.0;
    dst.wz = 0.0;
    dst.xw = 0.0;
    dst.yw = 0.0;
    dst.zw = 0.0;
    dst.ww = 1.0;
}

/// Mirrors `uvMat4Mul` (decomp/src/kernel/matrix.c): dst = src1 x src2,
/// computed into a temporary when dst aliases src1/src2 (as the C does).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn uvMat4Mul(dst: *mut Mtx4F, src1: *mut Mtx4F, src2: *mut Mtx4F) {
    let aliased = std::ptr::eq(src1, dst) || std::ptr::eq(src2, dst);
    // SAFETY: the caller guarantees each argument is a valid Mtx4F (never
    // aliases another argument's buffer — this is C pointer semantics, not
    // Rust references).
    let (src1, src2) = unsafe { (&*src1, &*src2) };
    // SAFETY: all-zeros is a valid f32 bit pattern and every field below is
    // written before use.
    let mut temp: Mtx4F = unsafe { std::mem::zeroed() };
    // Row 0: m[0][0] .. m[0][3]
    temp.xx = src1.xx * src2.xx + src1.yx * src2.xy + src1.zx * src2.xz + src1.wx * src2.xw;
    temp.yx = src1.xx * src2.yx + src1.yx * src2.yy + src1.zx * src2.yz + src1.wx * src2.yw;
    temp.zx = src1.xx * src2.zx + src1.yx * src2.zy + src1.zx * src2.zz + src1.wx * src2.zw;
    temp.wx = src1.xx * src2.wx + src1.yx * src2.wy + src1.zx * src2.wz + src1.wx * src2.ww;
    // Row 1: m[1][0] .. m[1][3]
    temp.xy = src1.xy * src2.xx + src1.yy * src2.xy + src1.zy * src2.xz + src1.wy * src2.xw;
    temp.yy = src1.xy * src2.yx + src1.yy * src2.yy + src1.zy * src2.yz + src1.wy * src2.yw;
    temp.zy = src1.xy * src2.zx + src1.yy * src2.zy + src1.zy * src2.zz + src1.wy * src2.zw;
    temp.wy = src1.xy * src2.wx + src1.yy * src2.wy + src1.zy * src2.wz + src1.wy * src2.ww;
    // Row 2: m[2][0] .. m[2][3]
    temp.xz = src1.xz * src2.xx + src1.yz * src2.xy + src1.zz * src2.xz + src1.wz * src2.xw;
    temp.yz = src1.xz * src2.yx + src1.yz * src2.yy + src1.zz * src2.yz + src1.wz * src2.yw;
    temp.zz = src1.xz * src2.zx + src1.yz * src2.zy + src1.zz * src2.zz + src1.wz * src2.zw;
    temp.wz = src1.xz * src2.wx + src1.yz * src2.wy + src1.zz * src2.wz + src1.wz * src2.ww;
    // Row 3: m[3][0] .. m[3][3]
    temp.xw = src1.xw * src2.xx + src1.yw * src2.xy + src1.zw * src2.xz + src1.ww * src2.xw;
    temp.yw = src1.xw * src2.yx + src1.yw * src2.yy + src1.zw * src2.yz + src1.ww * src2.yw;
    temp.zw = src1.xw * src2.zx + src1.yw * src2.zy + src1.zw * src2.zz + src1.ww * src2.zw;
    temp.ww = src1.xw * src2.wx + src1.yw * src2.wy + src1.zw * src2.wz + src1.ww * src2.ww;
    // SAFETY: valid Mtx4Fs; every source read is complete, so `dst` may
    // alias src1/src2 (the aliased case copies back through uvMat4Copy).
    let dst = unsafe { &mut *dst };
    if aliased {
        // SAFETY: `dst` is valid; `temp` is a local.
        unsafe { uvMat4Copy(dst, &mut temp) };
    } else {
        *dst = temp;
    }
}

/// Mirrors `uvMat4MulBA` (decomp/src/kernel/matrix.c): same shape as
/// `uvMat4Mul` but with the operands swapped — the C's row-0 sums
/// `src2[0][k] * src1[k][0]`, i.e. dst = src2 × src1 despite the argument
/// names. dst = src2 x src1, computed into a temporary when dst aliases
/// src1/src2 (as the C does).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn uvMat4MulBA(dst: *mut Mtx4F, src1: *mut Mtx4F, src2: *mut Mtx4F) {
    let aliased = std::ptr::eq(src1, dst) || std::ptr::eq(src2, dst);
    // SAFETY: C pointer semantics (see the crate docs).
    let (src1, src2) = unsafe { (&*src1, &*src2) };
    // SAFETY: all-zeros is a valid f32 bit pattern and every field below is
    // written before use.
    let mut temp: Mtx4F = unsafe { std::mem::zeroed() };
    // Row 0: m[0][0] .. m[0][3] — the C reads src2's row 0 against src1's
    // column k.
    temp.xx = src2.xx * src1.xx + src2.yx * src1.xy + src2.zx * src1.xz + src2.wx * src1.xw;
    temp.yx = src2.xx * src1.yx + src2.yx * src1.yy + src2.zx * src1.yz + src2.wx * src1.yw;
    temp.zx = src2.xx * src1.zx + src2.yx * src1.zy + src2.zx * src1.zz + src2.wx * src1.zw;
    temp.wx = src2.xx * src1.wx + src2.yx * src1.wy + src2.zx * src1.wz + src2.wx * src1.ww;
    // Row 1: m[1][0] .. m[1][3]
    temp.xy = src2.xy * src1.xx + src2.yy * src1.xy + src2.zy * src1.xz + src2.wy * src1.xw;
    temp.yy = src2.xy * src1.yx + src2.yy * src1.yy + src2.zy * src1.yz + src2.wy * src1.yw;
    temp.zy = src2.xy * src1.zx + src2.yy * src1.zy + src2.zy * src1.zz + src2.wy * src1.zw;
    temp.wy = src2.xy * src1.wx + src2.yy * src1.wy + src2.zy * src1.wz + src2.wy * src1.ww;
    // Row 2: m[2][0] .. m[2][3]
    temp.xz = src2.xz * src1.xx + src2.yz * src1.xy + src2.zz * src1.xz + src2.wz * src1.xw;
    temp.yz = src2.xz * src1.yx + src2.yz * src1.yy + src2.zz * src1.yz + src2.wz * src1.yw;
    temp.zz = src2.xz * src1.zx + src2.yz * src1.zy + src2.zz * src1.zz + src2.wz * src1.zw;
    temp.wz = src2.xz * src1.wx + src2.yz * src1.wy + src2.zz * src1.wz + src2.wz * src1.ww;
    // Row 3: m[3][0] .. m[3][3]
    temp.xw = src2.xw * src1.xx + src2.yw * src1.xy + src2.zw * src1.xz + src2.ww * src1.xw;
    temp.yw = src2.xw * src1.yx + src2.yw * src1.yy + src2.zw * src1.yz + src2.ww * src1.yw;
    temp.zw = src2.xw * src1.zx + src2.yw * src1.zy + src2.zw * src1.zz + src2.ww * src1.zw;
    temp.ww = src2.xw * src1.wx + src2.yw * src1.wy + src2.zw * src1.wz + src2.ww * src1.ww;
    // SAFETY: valid Mtx4Fs; every source read is complete, so `dst` may
    // alias src1/src2 (the aliased case copies back through uvMat4Copy).
    let dst = unsafe { &mut *dst };
    if aliased {
        // SAFETY: `dst` is valid; `temp` is a local.
        unsafe { uvMat4Copy(dst, &mut temp) };
    } else {
        *dst = temp;
    }
}

/// Mirrors `uvMat4CopyXYZ` (decomp/src/kernel/matrix.c): the top-left 3x3
/// only; row 3 and the w column are left alone.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn uvMat4CopyXYZ(dst: *mut Mtx4F, src: *mut Mtx4F) {
    // m[r][0..3] for r in 0..3, raw pointers (dst may alias src).
    let (d, s) = (dst.cast::<f32>(), src.cast::<f32>());
    for r in 0..3 {
        for c in 0..3 {
            // SAFETY: `r * 4 + c < 12` is in bounds of either Mtx4F's 16
            // f32s; raw pointers because dst may alias src.
            unsafe { *d.add(r * 4 + c) = *s.add(r * 4 + c) };
        }
    }
}

unsafe extern "C" {
    /// decomp/src/kernel/math.c: table-based polynomial sin/cos. Kept in C
    /// (bit-identical f64 polynomials with per-target rounding subtleties;
    /// we don't reimplement libm maths); links from the pw64-game archive.
    fn uvSinF(x: f32) -> f32;
    fn uvCosF(x: f32) -> f32;
}

/// Mirrors `uvMat4RotateAxis` (decomp/src/kernel/matrix.c): rotates dst about
/// the x/y/z axis by `angle`; a zero angle leaves dst unchanged. Another axis
/// letter (with a non-zero angle) copies the C's untouched `temp` into dst —
/// all zeros, since the C is built with `-ftrivial-auto-var-init=zero` — so
/// the zeroed temp here matches. `uvSinF`/`uvCosF` stay in C (see above).
/// `axis` is a C `char`, unsigned in this build (`-J`/`-funsigned-char`);
/// every caller passes 'x'/'y'/'z' (< 0x80), so `c_char`'s sign is moot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn uvMat4RotateAxis(dst: *mut Mtx4F, angle: f32, axis: core::ffi::c_char) {
    if angle != 0.0 {
        // SAFETY: `uvSinF`/`uvCosF` are the decomp's pure math wrappers.
        let sin = unsafe { uvSinF(angle) };
        // SAFETY: as `uvSinF` above.
        let cos = unsafe { uvCosF(angle) };
        // SAFETY: `dst` is a valid Mtx4F (C caller contract, see the crate
        // docs).
        let dst = unsafe { &mut *dst };
        // SAFETY: all-zeros is a valid f32 bit pattern; every field written
        // below is read from dst first.
        let mut temp: Mtx4F = unsafe { std::mem::zeroed() };
        match axis as u8 {
            b'x' => {
                temp.xx = dst.xx;
                temp.yx = dst.yx;
                temp.zx = dst.zx;
                temp.wx = dst.wx;
                temp.xy = cos * dst.xy + sin * dst.xz;
                temp.yy = cos * dst.yy + sin * dst.yz;
                temp.zy = cos * dst.zy + sin * dst.zz;
                temp.wy = cos * dst.wy + sin * dst.wz;
                temp.xz = cos * dst.xz - sin * dst.xy;
                temp.yz = cos * dst.yz - sin * dst.yy;
                temp.zz = cos * dst.zz - sin * dst.zy;
                temp.wz = cos * dst.wz - sin * dst.wy;
                temp.xw = dst.xw;
                temp.yw = dst.yw;
                temp.zw = dst.zw;
                temp.ww = dst.ww;
            }
            b'y' => {
                temp.xx = cos * dst.xx - sin * dst.xz;
                temp.yx = cos * dst.yx - sin * dst.yz;
                temp.zx = cos * dst.zx - sin * dst.zz;
                temp.wx = cos * dst.wx - sin * dst.wz;
                temp.xy = dst.xy;
                temp.yy = dst.yy;
                temp.zy = dst.zy;
                temp.wy = dst.wy;
                temp.xz = sin * dst.xx + cos * dst.xz;
                temp.yz = sin * dst.yx + cos * dst.yz;
                temp.zz = sin * dst.zx + cos * dst.zz;
                temp.wz = sin * dst.wx + cos * dst.wz;
                temp.xw = dst.xw;
                temp.yw = dst.yw;
                temp.zw = dst.zw;
                temp.ww = dst.ww;
            }
            b'z' => {
                temp.xx = cos * dst.xx + sin * dst.xy;
                temp.yx = cos * dst.yx + sin * dst.yy;
                temp.zx = cos * dst.zx + sin * dst.zy;
                temp.wx = cos * dst.wx + sin * dst.wy;
                temp.xy = cos * dst.xy - sin * dst.xx;
                temp.yy = cos * dst.yy - sin * dst.yx;
                temp.zy = cos * dst.zy - sin * dst.zx;
                temp.wy = cos * dst.wy - sin * dst.wx;
                temp.xz = dst.xz;
                temp.yz = dst.yz;
                temp.zz = dst.zz;
                temp.wz = dst.wz;
                temp.xw = dst.xw;
                temp.yw = dst.yw;
                temp.zw = dst.zw;
                temp.ww = dst.ww;
            }
            _ => {}
        }
        // SAFETY: `dst` is valid; `temp` is a local.
        unsafe { uvMat4Copy(dst, &mut temp) };
    }
}

/// Mirrors `uvMat4LocalTranslate` (decomp/src/kernel/matrix.c): post-multiplies
/// the translation (x, y, z) onto dst's bottom row through the rotation part.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn uvMat4LocalTranslate(dst: *mut Mtx4F, x: f32, y: f32, z: f32) {
    // SAFETY: `dst` is a valid Mtx4F (C caller contract, see the crate docs).
    let dst = unsafe { &mut *dst };
    // SAFETY: all-zeros is a valid f32 bit pattern; every field written below
    // is read from dst first.
    let mut temp: Mtx4F = unsafe { std::mem::zeroed() };
    temp.xx = dst.xx;
    temp.yx = dst.yx;
    temp.zx = dst.zx;
    temp.wx = dst.wx;
    temp.xy = dst.xy;
    temp.yy = dst.yy;
    temp.zy = dst.zy;
    temp.wy = dst.wy;
    temp.xz = dst.xz;
    temp.yz = dst.yz;
    temp.zz = dst.zz;
    temp.wz = dst.wz;
    temp.xw = x * dst.xx + y * dst.xy + z * dst.xz + dst.xw;
    temp.yw = x * dst.yx + y * dst.yy + z * dst.yz + dst.yw;
    temp.zw = x * dst.zx + y * dst.zy + z * dst.zz + dst.zw;
    temp.ww = x * dst.wx + y * dst.wy + z * dst.wz + dst.ww;
    // SAFETY: `dst` is valid; `temp` is a local.
    unsafe { uvMat4Copy(dst, &mut temp) };
}

/// Mirrors `uvMat4Scale` (decomp/src/kernel/matrix.c): scales dst's rotation
/// part row-wise.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn uvMat4Scale(dst: *mut Mtx4F, scale_x: f32, scale_y: f32, scale_z: f32) {
    // SAFETY: `dst` is a valid Mtx4F (C caller contract, see the crate docs).
    let dst = unsafe { &mut *dst };
    // SAFETY: all-zeros is a valid f32 bit pattern; every field written below
    // is read from dst first.
    let mut scaled: Mtx4F = unsafe { std::mem::zeroed() };
    scaled.xx = dst.xx * scale_x;
    scaled.yx = dst.yx * scale_x;
    scaled.zx = dst.zx * scale_x;
    scaled.wx = dst.wx * scale_x;
    scaled.xy = dst.xy * scale_y;
    scaled.yy = dst.yy * scale_y;
    scaled.zy = dst.zy * scale_y;
    scaled.wy = dst.wy * scale_y;
    scaled.xz = dst.xz * scale_z;
    scaled.yz = dst.yz * scale_z;
    scaled.zz = dst.zz * scale_z;
    scaled.wz = dst.wz * scale_z;
    scaled.xw = dst.xw;
    scaled.yw = dst.yw;
    scaled.zw = dst.zw;
    scaled.ww = dst.ww;
    // SAFETY: `dst` is valid; `scaled` is a local.
    unsafe { uvMat4Copy(dst, &mut scaled) };
}

/// Mirrors `uvMat4InvertTranslationRotation` (decomp/src/kernel/matrix.c):
/// dst = inverse of mat2's rotation part (its transpose) with the negated
/// translation applied through the rotated axes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn uvMat4InvertTranslationRotation(dst: *mut Mtx4F, mat2: *mut Mtx4F) {
    // SAFETY: `mat2` is a valid Mtx4F (C caller contract, see the crate docs).
    let mat2 = unsafe { &*mat2 };
    // SAFETY: all-zeros is a valid f32 bit pattern; every field written below
    // is read from mat2 first.
    let mut sp50: Mtx4F = unsafe { std::mem::zeroed() };
    // SAFETY: `sp50` is a local; `mat2` valid (read-only).
    unsafe { uvMat4Copy(&mut sp50, &mut { *mat2 }) };
    // Transpose the 3x3 (the C's i/j loop with j < i swaps each off-diagonal
    // pair; it reads mat2, not sp50).
    for i in 0..3 {
        for j in 0..i {
            sp50_set(&mut sp50, i, j, mat2_get(mat2, j, i));
            sp50_set(&mut sp50, j, i, mat2_get(mat2, i, j));
        }
    }
    sp50.xw = 0.0;
    sp50.yw = 0.0;
    sp50.zw = 0.0;
    // SAFETY: operates on the local `sp50`.
    unsafe { uvMat4LocalTranslate(&mut sp50, -mat2.xw, -mat2.yw, -mat2.zw) };
    // SAFETY: `dst` is valid; `sp50` is a local.
    unsafe { uvMat4Copy(dst, &mut sp50) };
}

/// `m[r][c]` of an `Mtx4F` (repr(C) row-major, see the struct docs) — helper
/// for `uvMat4InvertTranslationRotation`'s transposition loop.
fn mat2_get(m: &Mtx4F, r: usize, c: usize) -> f32 {
    // SAFETY: `Mtx4F` is repr(C) over 16 f32s (see the struct docs), so the
    // cast is layout-valid; callers pass r, c < 3, so `r * 4 + c < 16`.
    let f = unsafe { &*(m as *const Mtx4F as *const [f32; 16]) };
    f[r * 4 + c]
}

/// `m[r][c] = v` — write half of [`mat2_get`].
fn sp50_set(m: &mut Mtx4F, r: usize, c: usize, v: f32) {
    // SAFETY: as [`mat2_get`]: repr(C) cast is layout-valid, r * 4 + c < 16.
    let f = unsafe { &mut *(m as *mut Mtx4F as *mut [f32; 16]) };
    f[r * 4 + c] = v;
}

/// Mirrors `uvMat4LocalToWorld` (decomp/src/kernel/matrix.c): dst = src x vec2
/// (rotation + translation). The C reads vec2 before writing dst, so dst may
/// alias vec2.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn uvMat4LocalToWorld(src: *mut Mtx4F, dst: *mut Vec3F, vec2: *mut Vec3F) {
    // SAFETY: valid Mtx4F / Vec3Fs (C caller contract, see the crate docs).
    let src = unsafe { &*src };
    // SAFETY: as `src` above.
    let vec2 = unsafe { &*vec2 };
    let x = vec2.x;
    let y = vec2.y;
    let z = vec2.z;
    // SAFETY: `vec2` was fully read into locals above, so a write through
    // `dst` (which may alias it) is fine.
    let dst = unsafe { &mut *dst };
    dst.x = x * src.xx + y * src.xy + z * src.xz + src.xw;
    dst.y = x * src.yx + y * src.yy + z * src.yz + src.yw;
    dst.z = x * src.zx + y * src.zy + z * src.zz + src.zw;
}

/// Mirrors `uvMat4SetFrustrum` (decomp/src/kernel/matrix.c).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn uvMat4SetFrustrum(
    dst: *mut Mtx4F,
    left: f32,
    right: f32,
    top: f32,
    bottom: f32,
    near: f32,
    far: f32,
) {
    // SAFETY: `dst` is a valid Mtx4F (C caller contract, see the crate docs).
    let dst = unsafe { &mut *dst };
    dst.xx = (2.0 * near) / (right - left);
    dst.yy = (2.0 * near) / (bottom - top);
    dst.xz = (right + left) / (right - left);
    dst.yz = (bottom + top) / (bottom - top);
    dst.zz = -(far + near) / (far - near);
    dst.wz = -1.0;
    dst.zw = -((2.0 * near) * far) / (far - near);
    // The C's chained assignment zeroes m[0][1], m[0][2], m[0][3], m[1][0],
    // m[1][2], m[1][3], m[3][0], m[3][1], m[3][3].
    dst.yx = 0.0;
    dst.zx = 0.0;
    dst.wx = 0.0;
    dst.xy = 0.0;
    dst.zy = 0.0;
    dst.wy = 0.0;
    dst.xw = 0.0;
    dst.yw = 0.0;
    dst.ww = 0.0;
}

/// Mirrors `uvMat4SetOrtho` (decomp/src/kernel/matrix.c).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn uvMat4SetOrtho(
    dst: *mut Mtx4F,
    left: f32,
    right: f32,
    top: f32,
    bottom: f32,
) {
    // SAFETY: `dst` is a valid Mtx4F (C caller contract, see the crate docs).
    let dst = unsafe { &mut *dst };
    dst.xx = 2.0 / (right - left);
    dst.yy = 2.0 / (bottom - top);
    dst.zz = -1.0;
    dst.xw = -(right + left) / (right - left);
    dst.yw = -(bottom + top) / (bottom - top);
    dst.yx = 0.0;
    dst.zx = 0.0;
    dst.wx = 0.0;
    dst.xy = 0.0;
    dst.zy = 0.0;
    dst.wy = 0.0;
    dst.xz = 0.0;
    dst.yz = 0.0;
    dst.wz = 0.0;
    dst.zw = 0.0;
    dst.ww = 1.0;
}

/// Mirrors `uvMat4SetQuaternionRotation` (decomp/src/kernel/matrix.c): the C's
/// `SQ(x)` macro is `x * x` (its multiplication order is mirrored).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn uvMat4SetQuaternionRotation(
    dst: *mut Mtx4F,
    arg1: f32,
    arg2: f32,
    arg3: f32,
    arg4: f32,
) {
    // SAFETY: `dst` is a valid Mtx4F (C caller contract, see the crate docs).
    let dst = unsafe { &mut *dst };
    dst.xx = 1.0 - (2.0 * (arg2 * arg2 + arg3 * arg3));
    dst.yx = 2.0 * (arg1 * arg2 - arg3 * arg4);
    dst.zx = 2.0 * (arg3 * arg1 + arg2 * arg4);
    dst.xy = 2.0 * (arg1 * arg2 + arg3 * arg4);
    dst.yy = 1.0 - (2.0 * (arg3 * arg3 + arg1 * arg1));
    dst.zy = 2.0 * (arg2 * arg3 - arg1 * arg4);
    dst.xz = 2.0 * (arg3 * arg1 - arg2 * arg4);
    dst.yz = 2.0 * (arg2 * arg3 + arg1 * arg4);
    dst.zz = 1.0 - (2.0 * (arg2 * arg2 + arg1 * arg1));
}
