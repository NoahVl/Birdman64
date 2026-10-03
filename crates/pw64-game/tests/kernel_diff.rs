//! C-vs-Rust differential tests: for every function ported in
//! `crates/pw64-kernel/src/lib.rs` (listed in `ported.txt`), run the renamed
//! C original (`pw64_c_<name>`, compiled by this crate's build script) and the
//! Rust port on the same inputs and require **bit-identical** results
//! (`f32::to_bits` on every output f32 — any NaN matches any NaN, see
//! `same_bits`; direct equality on the i32 outputs).
//!
//! Inputs: deterministic xorshift32 random f32 bit patterns (finite, exponent
//! 120..=133, i.e. ~0.008 .. ~32768) plus the edge list: ±0, ±1, ±1e-30,
//! ±3e38 (overflow → inf), ±denormals, ±inf and NaNs. Run in both debug and
//! `--release` (operand order and folding differ with the opt level). dst/src pointers are always distinct buffers except in the
//! dedicated aliasing test (the C handles the aliasing it needs internally).
//! The `pw64_c_*` symbols resolving at link time is itself part of the proof
//! (`c_symbols_resolve` smoke test below).

// Test fns are named after the C functions they compare.
#![allow(non_snake_case)]

use pw64_kernel::{
    Mtx4F, Vec3F, uvMat4Copy, uvMat4CopyXYZ, uvMat4InvertTranslationRotation, uvMat4LocalToWorld,
    uvMat4LocalTranslate, uvMat4Mul, uvMat4MulBA, uvMat4RotateAxis, uvMat4Scale, uvMat4SetFrustrum,
    uvMat4SetIdentity, uvMat4SetOrtho, uvMat4SetQuaternionRotation, uvVec2Dot, uvVec3Add,
    uvVec3Copy, uvVec3Cross, uvVec3Dot, uvVec3Len, uvVec3Mul, uvVec3Normal, uvVec3ScalarProj,
};
use std::mem::offset_of;

// Links the C archive (whole-archive, with pw64-platform and pw64-kernel as
// its dependencies): rustc drops an unreferenced crate from the link line.
use pw64_game as _;

// The renamed C originals (same signatures as the decomp headers), defined in
// the pw64-game archive.
unsafe extern "C" {
    fn pw64_c_uvVec3Len(v: *mut Vec3F) -> f32;
    fn pw64_c_uvVec2Dot(v0: *mut Vec3F, v1: *mut Vec3F) -> f32;
    fn pw64_c_uvVec3Dot(v0: *mut Vec3F, v1: *mut Vec3F) -> f32;
    fn pw64_c_uvVec3Copy(vdst: *mut Vec3F, vsrc: *mut Vec3F);
    fn pw64_c_uvVec3ScalarProj(v0: *mut Vec3F, v1: *mut Vec3F) -> f32;
    fn pw64_c_uvVec3Cross(vd: *mut Vec3F, va: *mut Vec3F, vb: *mut Vec3F);
    fn pw64_c_uvVec3Add(vd: *mut Vec3F, va: *mut Vec3F, vb: *mut Vec3F);
    fn pw64_c_uvVec3Mul(vd: *mut Vec3F, va: *mut Vec3F, sb: f32);
    fn pw64_c_uvVec3Normal(vd: *mut Vec3F, va: *mut Vec3F) -> i32;
    fn pw64_c_uvMat4Copy(dst: *mut Mtx4F, src: *mut Mtx4F);
    fn pw64_c_uvMat4SetIdentity(dst: *mut Mtx4F);
    fn pw64_c_uvMat4Mul(dst: *mut Mtx4F, src1: *mut Mtx4F, src2: *mut Mtx4F);
    fn pw64_c_uvMat4CopyXYZ(dst: *mut Mtx4F, src: *mut Mtx4F);
    fn pw64_c_uvMat4MulBA(dst: *mut Mtx4F, src1: *mut Mtx4F, src2: *mut Mtx4F);
    fn pw64_c_uvMat4RotateAxis(dst: *mut Mtx4F, angle: f32, axis: core::ffi::c_char);
    fn pw64_c_uvMat4LocalTranslate(dst: *mut Mtx4F, x: f32, y: f32, z: f32);
    fn pw64_c_uvMat4Scale(dst: *mut Mtx4F, scale_x: f32, scale_y: f32, scale_z: f32);
    fn pw64_c_uvMat4InvertTranslationRotation(dst: *mut Mtx4F, mat2: *mut Mtx4F);
    fn pw64_c_uvMat4LocalToWorld(src: *mut Mtx4F, dst: *mut Vec3F, vec2: *mut Vec3F);
    fn pw64_c_uvMat4SetFrustrum(
        dst: *mut Mtx4F,
        left: f32,
        right: f32,
        top: f32,
        bottom: f32,
        near: f32,
        far: f32,
    );
    fn pw64_c_uvMat4SetOrtho(dst: *mut Mtx4F, left: f32, right: f32, top: f32, bottom: f32);
    fn pw64_c_uvMat4SetQuaternionRotation(
        dst: *mut Mtx4F,
        arg1: f32,
        arg2: f32,
        arg3: f32,
        arg4: f32,
    );
}

/// Deterministic xorshift32 (no deps, no time): fixed nonzero seed.
struct Rng(u32);

impl Rng {
    fn new() -> Self {
        Self(0x9E37_79B9)
    }

    fn next_u32(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }

    /// Random finite f32 from bit patterns: random sign, exponent 120..=133
    /// (about 1e-9 .. 4e13 — normal, comfortably inside f32), random mantissa.
    /// No NaN/Inf by construction.
    fn next_f32(&mut self) -> f32 {
        let sign = self.next_u32() & 0x8000_0000;
        let exp = 120 + self.next_u32() % 14;
        let man = self.next_u32() & 0x007f_ffff;
        f32::from_bits(sign | (exp << 23) | man)
    }

    fn next_vec3(&mut self) -> Vec3F {
        Vec3F {
            x: self.next_f32(),
            y: self.next_f32(),
            z: self.next_f32(),
        }
    }

    fn next_mat4(&mut self) -> Mtx4F {
        // repr(C): the 16 fields are contiguous f32, so bit patterns can be
        // filled through the raw pointer (fields generated in memory order,
        // i.e. xx, yx, zx, wx, …).
        let mut m: Mtx4F = unsafe { std::mem::zeroed() };
        let f = unsafe { std::slice::from_raw_parts_mut(&mut m as *mut Mtx4F as *mut f32, 16) };
        for v in f {
            *v = self.next_f32();
        }
        m
    }
}

/// Edge inputs: ±0, ±1, ±tiny (~1e-30), ±huge (~3e38, products overflow to
/// inf), ±denormals, ±inf, and two NaNs with different payloads/signs (NaN
/// *ness* must propagate identically; payloads are exempt, see `same_bits`).
const EDGES: [f32; 16] = [
    0.0,
    -0.0,
    1.0,
    -1.0,
    1e-30,
    -1e-30,
    3e38,
    -3e38,
    f32::from_bits(0x0000_0001),
    f32::from_bits(0x8040_0000),
    f32::INFINITY,
    f32::NEG_INFINITY,
    f32::from_bits(0x7fc1_2345),
    f32::from_bits(0xffc5_4321),
    0.5,
    -2.0,
];

const N_RANDOM: usize = 10_000;

/// Edge vectors: every edge value on all three components, plus mixed tuples.
fn edge_vecs() -> Vec<Vec3F> {
    let mut v: Vec<Vec3F> = EDGES.iter().map(|&e| Vec3F { x: e, y: e, z: e }).collect();
    for i in 0..EDGES.len() {
        v.push(Vec3F {
            x: EDGES[i],
            y: EDGES[(i + 3) % EDGES.len()],
            z: EDGES[(i + 5) % EDGES.len()],
        });
    }
    v
}

/// Edge matrices: every edge value in all 16 entries, plus mixed ones.
fn edge_mats() -> Vec<Mtx4F> {
    let mut v: Vec<Mtx4F> = EDGES
        .iter()
        .map(|&e| {
            let mut m: Mtx4F = unsafe { std::mem::zeroed() };
            let f = unsafe { std::slice::from_raw_parts_mut(&mut m as *mut Mtx4F as *mut f32, 16) };
            f.fill(e);
            m
        })
        .collect();
    for i in 0..EDGES.len() {
        let mut m: Mtx4F = unsafe { std::mem::zeroed() };
        let f = unsafe { std::slice::from_raw_parts_mut(&mut m as *mut Mtx4F as *mut f32, 16) };
        for (j, e) in f.iter_mut().enumerate() {
            *e = EDGES[(i + j) % EDGES.len()];
        }
        v.push(m);
    }
    v
}

/// The 16 matrix entries as a f32 slice (repr(C), contiguous).
unsafe fn mat_as_f32(m: &Mtx4F) -> &[f32; 16] {
    unsafe { &*(m as *const Mtx4F as *const [f32; 16]) }
}

/// Bit-identical, except that any NaN equals any NaN: NaN sign/payload is
/// not reproducible — LLVM treats fadd/fmul as commutative and may swap
/// operands (x86 SSE returns the first operand's NaN), and on both sides
/// the result depends on the opt level (a release build diverged on
/// NaN payloads, even C-vs-C would). The game never inspects NaN bits.
fn same_bits(c: f32, r: f32) -> bool {
    c.to_bits() == r.to_bits() || (c.is_nan() && r.is_nan())
}

fn cmp_f32(ctx: &str, i: usize, c: f32, r: f32) {
    if !same_bits(c, r) {
        panic!(
            "{ctx}: input {i}: C bits {:#010x} ({c}) != Rust bits {:#010x} ({r})",
            c.to_bits(),
            r.to_bits()
        );
    }
}

fn cmp_vec3(ctx: &str, i: usize, c: &Vec3F, r: &Vec3F) {
    cmp_f32(&format!("{ctx} x"), i, c.x, r.x);
    cmp_f32(&format!("{ctx} y"), i, c.y, r.y);
    cmp_f32(&format!("{ctx} z"), i, c.z, r.z);
}

fn cmp_mat4(ctx: &str, i: usize, c: &Mtx4F, r: &Mtx4F) {
    let (c, r) = unsafe { (mat_as_f32(c), mat_as_f32(r)) };
    for (j, (c, r)) in c.iter().zip(r).enumerate() {
        if !same_bits(*c, *r) {
            panic!(
                "{ctx}: input {i}: entry {j} (m[{}][{}]): C bits {:#010x} != Rust bits {:#010x}",
                j / 4,
                j % 4,
                c.to_bits(),
                r.to_bits()
            );
        }
    }
}

/// Case generator for the (Vec3F) -> f32 and (Vec3F, Vec3F) -> f32 shapes.
fn random_vec_cases(n: usize) -> (Vec<Vec3F>, Vec<Vec3F>) {
    let mut rng = Rng::new();
    let (mut a, mut b) = (Vec::with_capacity(n), Vec::with_capacity(n));
    for _ in 0..n {
        a.push(rng.next_vec3());
        b.push(rng.next_vec3());
    }
    (a, b)
}

#[test]
fn diff_1in_f32() {
    type F = unsafe extern "C" fn(*mut Vec3F) -> f32;
    for (name, c_fn, r_fn) in [("uvVec3Len", pw64_c_uvVec3Len as F, uvVec3Len as F)] {
        let (as_, _) = random_vec_cases(N_RANDOM);
        for (i, a) in as_.iter().chain(edge_vecs().iter()).enumerate() {
            let (mut ca, mut ra) = (*a, *a);
            let (c, r) = unsafe { (c_fn(&mut ca), r_fn(&mut ra)) };
            cmp_f32(name, i, c, r);
        }
    }
}

#[test]
fn diff_2in_f32() {
    type F = unsafe extern "C" fn(*mut Vec3F, *mut Vec3F) -> f32;
    for (name, c_fn, r_fn) in [
        ("uvVec2Dot", pw64_c_uvVec2Dot as F, uvVec2Dot as F),
        ("uvVec3Dot", pw64_c_uvVec3Dot as F, uvVec3Dot as F),
        (
            "uvVec3ScalarProj",
            pw64_c_uvVec3ScalarProj as F,
            uvVec3ScalarProj as F,
        ),
    ] {
        let (as_, bs) = random_vec_cases(N_RANDOM);
        for (i, (a, b)) in as_.iter().zip(&bs).enumerate() {
            let (mut ca, mut cb, mut ra, mut rb) = (*a, *b, *a, *b);
            let (c, r) = unsafe { (c_fn(&mut ca, &mut cb), r_fn(&mut ra, &mut rb)) };
            cmp_f32(name, i, c, r);
        }
    }
    // Edge pairs (both operands over the edge list).
    let edges = edge_vecs();
    for (i, a) in edges.iter().enumerate() {
        for b in &edges {
            let (mut ca, mut cb, mut ra, mut rb) = (*a, *b, *a, *b);
            for (name, c_fn, r_fn) in [
                ("uvVec2Dot", pw64_c_uvVec2Dot as F, uvVec2Dot as F),
                ("uvVec3Dot", pw64_c_uvVec3Dot as F, uvVec3Dot as F),
                (
                    "uvVec3ScalarProj",
                    pw64_c_uvVec3ScalarProj as F,
                    uvVec3ScalarProj as F,
                ),
            ] {
                let (c, r) = unsafe { (c_fn(&mut ca, &mut cb), r_fn(&mut ra, &mut rb)) };
                cmp_f32(name, i, c, r);
            }
        }
    }
}

#[test]
fn diff_uvVec3Copy() {
    let (as_, _) = random_vec_cases(N_RANDOM);
    for (i, a) in as_.iter().chain(edge_vecs().iter()).enumerate() {
        let (mut cd, mut rd) = (Vec3F::default(), Vec3F::default());
        unsafe {
            pw64_c_uvVec3Copy(&mut cd, &mut { *a });
            uvVec3Copy(&mut rd, &mut { *a });
        }
        cmp_vec3("uvVec3Copy", i, &cd, &rd);
    }
}

#[test]
fn diff_uvVec3Add() {
    type F = unsafe extern "C" fn(*mut Vec3F, *mut Vec3F, *mut Vec3F);
    let (c_fn, r_fn) = (pw64_c_uvVec3Add as F, uvVec3Add as F);
    let (as_, bs) = random_vec_cases(N_RANDOM);
    for (i, (a, b)) in as_.iter().zip(&bs).enumerate() {
        let (mut cd, mut rd) = (Vec3F::default(), Vec3F::default());
        unsafe {
            c_fn(&mut cd, &mut { *a }, &mut { *b });
            r_fn(&mut rd, &mut { *a }, &mut { *b });
        }
        cmp_vec3("uvVec3Add", i, &cd, &rd);
    }
    let edges = edge_vecs();
    for (i, a) in edges.iter().enumerate() {
        for b in &edges {
            let (mut cd, mut rd) = (Vec3F::default(), Vec3F::default());
            unsafe {
                c_fn(&mut cd, &mut { *a }, &mut { *b });
                r_fn(&mut rd, &mut { *a }, &mut { *b });
            }
            cmp_vec3("uvVec3Add (edge)", i, &cd, &rd);
        }
    }
}

#[test]
fn diff_uvVec3Mul() {
    type F = unsafe extern "C" fn(*mut Vec3F, *mut Vec3F, f32);
    let (c_fn, r_fn) = (pw64_c_uvVec3Mul as F, uvVec3Mul as F);
    let (as_, bs) = random_vec_cases(N_RANDOM);
    for (i, (a, _b)) in as_.iter().zip(&bs).enumerate() {
        let sb = bs[i % bs.len()].x; // deterministic scalar from the pair
        let (mut cd, mut rd) = (Vec3F::default(), Vec3F::default());
        unsafe {
            c_fn(&mut cd, &mut { *a }, sb);
            r_fn(&mut rd, &mut { *a }, sb);
        }
        cmp_vec3("uvVec3Mul", i, &cd, &rd);
    }
    let edges = edge_vecs();
    for (i, a) in edges.iter().enumerate() {
        for &sb in &EDGES {
            let (mut cd, mut rd) = (Vec3F::default(), Vec3F::default());
            unsafe {
                c_fn(&mut cd, &mut { *a }, sb);
                r_fn(&mut rd, &mut { *a }, sb);
            }
            cmp_vec3("uvVec3Mul (edge)", i, &cd, &rd);
        }
    }
}

#[test]
fn diff_uvVec3Normal() {
    type F = unsafe extern "C" fn(*mut Vec3F, *mut Vec3F) -> i32;
    let (c_fn, r_fn) = (pw64_c_uvVec3Normal as F, uvVec3Normal as F);
    let (as_, _) = random_vec_cases(N_RANDOM);
    for (i, a) in as_.iter().chain(edge_vecs().iter()).enumerate() {
        let (mut cd, mut rd) = (Vec3F::default(), Vec3F::default());
        let (c, r) = unsafe { (c_fn(&mut cd, &mut { *a }), r_fn(&mut rd, &mut { *a })) };
        if c != r {
            panic!("uvVec3Normal: input {i}: C return {c} != Rust return {r}");
        }
        cmp_vec3("uvVec3Normal", i, &cd, &rd);
    }
}

#[test]
fn diff_uvVec3Cross() {
    type F = unsafe extern "C" fn(*mut Vec3F, *mut Vec3F, *mut Vec3F);
    let (c_fn, r_fn) = (pw64_c_uvVec3Cross as F, uvVec3Cross as F);
    let (as_, bs) = random_vec_cases(N_RANDOM);
    for (i, (a, b)) in as_.iter().zip(&bs).enumerate() {
        let (mut cd, mut rd) = (Vec3F::default(), Vec3F::default());
        unsafe {
            c_fn(&mut cd, &mut { *a }, &mut { *b });
            r_fn(&mut rd, &mut { *a }, &mut { *b });
        }
        cmp_vec3("uvVec3Cross", i, &cd, &rd);
    }
    let edges = edge_vecs();
    for (i, a) in edges.iter().enumerate() {
        for b in &edges {
            let (mut cd, mut rd) = (Vec3F::default(), Vec3F::default());
            unsafe {
                c_fn(&mut cd, &mut { *a }, &mut { *b });
                r_fn(&mut rd, &mut { *a }, &mut { *b });
            }
            cmp_vec3("uvVec3Cross (edge)", i, &cd, &rd);
        }
    }
}

#[test]
fn diff_uvMat4Copy() {
    type F = unsafe extern "C" fn(*mut Mtx4F, *mut Mtx4F);
    let (c_fn, r_fn) = (pw64_c_uvMat4Copy as F, uvMat4Copy as F);
    let mut rng = Rng::new();
    for i in 0..N_RANDOM {
        let a = rng.next_mat4();
        let (mut cd, mut rd) = (unsafe { std::mem::zeroed() }, unsafe { std::mem::zeroed() });
        unsafe {
            c_fn(&mut cd, &mut { a });
            r_fn(&mut rd, &mut { a });
        }
        cmp_mat4("uvMat4Copy", i, &cd, &rd);
    }
    for (i, a) in edge_mats().iter().enumerate() {
        let (mut cd, mut rd) = (unsafe { std::mem::zeroed() }, unsafe { std::mem::zeroed() });
        unsafe {
            c_fn(&mut cd, &mut { *a });
            r_fn(&mut rd, &mut { *a });
        }
        cmp_mat4("uvMat4Copy (edge)", i, &cd, &rd);
    }
}

#[test]
fn diff_uvMat4SetIdentity() {
    type F = unsafe extern "C" fn(*mut Mtx4F);
    let (c_fn, r_fn) = (pw64_c_uvMat4SetIdentity as F, uvMat4SetIdentity as F);
    let mut rng = Rng::new();
    for i in 0..N_RANDOM {
        // dst starts as garbage (random bits) to prove every entry is written.
        let (mut cd, mut rd) = (rng.next_mat4(), rng.next_mat4());
        unsafe {
            c_fn(&mut cd);
            r_fn(&mut rd);
        }
        cmp_mat4("uvMat4SetIdentity", i, &cd, &rd);
    }
}

#[test]
fn diff_uvMat4Mul() {
    type F = unsafe extern "C" fn(*mut Mtx4F, *mut Mtx4F, *mut Mtx4F);
    let (c_fn, r_fn) = (pw64_c_uvMat4Mul as F, uvMat4Mul as F);
    let mut rng = Rng::new();
    let mut mats = Vec::with_capacity(N_RANDOM);
    for _ in 0..N_RANDOM {
        mats.push((rng.next_mat4(), rng.next_mat4()));
    }
    for (i, (a, b)) in mats.iter().enumerate() {
        let (mut cd, mut rd) = (unsafe { std::mem::zeroed() }, unsafe { std::mem::zeroed() });
        unsafe {
            c_fn(&mut cd, &mut { *a }, &mut { *b });
            r_fn(&mut rd, &mut { *a }, &mut { *b });
        }
        cmp_mat4("uvMat4Mul", i, &cd, &rd);
    }
    let edges = edge_mats();
    for (i, (a, b)) in edges.iter().cloned().zip(edges.iter().cloned()).enumerate() {
        let (mut cd, mut rd) = (unsafe { std::mem::zeroed() }, unsafe { std::mem::zeroed() });
        unsafe {
            c_fn(&mut cd, &mut { a }, &mut { b });
            r_fn(&mut rd, &mut { a }, &mut { b });
        }
        cmp_mat4("uvMat4Mul (edge)", i, &cd, &rd);
    }
}

/// dst-only matrix writes: (Mtx4F*, …) functions that don't read dst.
#[test]
fn diff_uvMat4SetFrustrum() {
    type F = unsafe extern "C" fn(*mut Mtx4F, f32, f32, f32, f32, f32, f32);
    let (c_fn, r_fn) = (pw64_c_uvMat4SetFrustrum as F, uvMat4SetFrustrum as F);
    let mut rng = Rng::new();
    let mut cases = Vec::with_capacity(N_RANDOM);
    for _ in 0..N_RANDOM {
        // left/right and top/bottom differ (the C divides by them), as do
        // near/far; keep them non-zero-ish via the random finite f32s.
        cases.push((
            rng.next_f32(),
            rng.next_f32(),
            rng.next_f32(),
            rng.next_f32(),
            rng.next_f32(),
            rng.next_f32(),
        ));
    }
    for (i, &(l, r, t, b, n, f)) in cases.iter().enumerate() {
        let (mut cd, mut rd) = (unsafe { std::mem::zeroed() }, unsafe { std::mem::zeroed() });
        unsafe {
            c_fn(&mut cd, l, r, t, b, n, f);
            r_fn(&mut rd, l, r, t, b, n, f);
        }
        cmp_mat4("uvMat4SetFrustrum", i, &cd, &rd);
    }
    // Edge floats: the divisions can produce 0/±inf/NaN-free fns (the C's
    // arithmetic is IEEE too); bit patterns must still match.
    for (i, &e) in EDGES.iter().enumerate() {
        let (mut cd, mut rd) = (unsafe { std::mem::zeroed() }, unsafe { std::mem::zeroed() });
        unsafe {
            c_fn(
                &mut cd,
                e,
                -e,
                e,
                e + 1.0,
                e.abs().max(1e-30),
                e.abs() + 2.0,
            );
            r_fn(
                &mut rd,
                e,
                -e,
                e,
                e + 1.0,
                e.abs().max(1e-30),
                e.abs() + 2.0,
            );
        }
        cmp_mat4("uvMat4SetFrustrum (edge)", i, &cd, &rd);
    }
}

#[test]
fn diff_uvMat4SetOrtho() {
    type F = unsafe extern "C" fn(*mut Mtx4F, f32, f32, f32, f32);
    let (c_fn, r_fn) = (pw64_c_uvMat4SetOrtho as F, uvMat4SetOrtho as F);
    let mut rng = Rng::new();
    let mut cases = Vec::with_capacity(N_RANDOM);
    for _ in 0..N_RANDOM {
        cases.push((
            rng.next_f32(),
            rng.next_f32(),
            rng.next_f32(),
            rng.next_f32(),
        ));
    }
    for (i, &(l, r, t, b)) in cases.iter().enumerate() {
        let (mut cd, mut rd) = (unsafe { std::mem::zeroed() }, unsafe { std::mem::zeroed() });
        unsafe {
            c_fn(&mut cd, l, r, t, b);
            r_fn(&mut rd, l, r, t, b);
        }
        cmp_mat4("uvMat4SetOrtho", i, &cd, &rd);
    }
    for (i, &e) in EDGES.iter().enumerate() {
        let (mut cd, mut rd) = (unsafe { std::mem::zeroed() }, unsafe { std::mem::zeroed() });
        unsafe {
            c_fn(&mut cd, e, -e, e, e + 1.0);
            r_fn(&mut rd, e, -e, e, e + 1.0);
        }
        cmp_mat4("uvMat4SetOrtho (edge)", i, &cd, &rd);
    }
}

#[test]
fn diff_uvMat4SetQuaternionRotation() {
    type F = unsafe extern "C" fn(*mut Mtx4F, f32, f32, f32, f32);
    let (c_fn, r_fn) = (
        pw64_c_uvMat4SetQuaternionRotation as F,
        uvMat4SetQuaternionRotation as F,
    );
    let mut rng = Rng::new();
    let mut cases = Vec::with_capacity(N_RANDOM);
    for _ in 0..N_RANDOM {
        cases.push((
            rng.next_f32(),
            rng.next_f32(),
            rng.next_f32(),
            rng.next_f32(),
        ));
    }
    for (i, &(a, b, c, d)) in cases.iter().enumerate() {
        let (mut cd, mut rd) = (unsafe { std::mem::zeroed() }, unsafe { std::mem::zeroed() });
        unsafe {
            c_fn(&mut cd, a, b, c, d);
            r_fn(&mut rd, a, b, c, d);
        }
        cmp_mat4("uvMat4SetQuaternionRotation", i, &cd, &rd);
    }
    for (i, &e) in EDGES.iter().enumerate() {
        let (mut cd, mut rd) = (unsafe { std::mem::zeroed() }, unsafe { std::mem::zeroed() });
        unsafe {
            c_fn(&mut cd, e, e, e, e);
            r_fn(&mut rd, e, e, e, e);
        }
        cmp_mat4("uvMat4SetQuaternionRotation (edge)", i, &cd, &rd);
    }
}

/// Matrices that read dst (in-place transforms): CopyXYZ, RotateAxis,
/// LocalTranslate, Scale, InvertTranslationRotation.
#[test]
fn diff_uvMat4_inplace() {
    let mut rng = Rng::new();
    let mut mats = Vec::with_capacity(N_RANDOM);
    for _ in 0..N_RANDOM {
        mats.push(rng.next_mat4());
    }
    for (i, m) in mats.iter().enumerate() {
        // CopyXYZ: only the 3x3 changes.
        let (mut cd, mut rd) = (*m, *m);
        unsafe {
            pw64_c_uvMat4CopyXYZ(&mut cd, &mut cd);
            uvMat4CopyXYZ(&mut rd, &mut rd);
        }
        cmp_mat4("uvMat4CopyXYZ", i, &cd, &rd);
        // RotateAxis: one of x/y/z, a spread of angles (incl. 0 = no-op).
        for (j, &(angle, axis)) in [
            (0.0, b'x'),
            (1.0, b'x'),
            (-2.5, b'y'),
            (0.5, b'z'),
            (rng.next_f32(), b'x'),
            (rng.next_f32(), b'y'),
            (rng.next_f32(), b'z'),
            // Not x/y/z: the C copies its (zero-initialised, via
            // -ftrivial-auto-var-init=zero) temp into dst.
            (1.0, b'w'),
            (f32::NAN, b'y'),
        ]
        .iter()
        .enumerate()
        {
            let (mut cd, mut rd) = (*m, *m);
            unsafe {
                pw64_c_uvMat4RotateAxis(&mut cd, angle, axis as core::ffi::c_char);
                uvMat4RotateAxis(&mut rd, angle, axis as core::ffi::c_char);
            }
            cmp_mat4(
                &format!("uvMat4RotateAxis {angle} {axis} case {j}"),
                i,
                &cd,
                &rd,
            );
        }
        // LocalTranslate / Scale.
        let (tx, ty, tz) = (rng.next_f32(), rng.next_f32(), rng.next_f32());
        let (mut cd, mut rd) = (*m, *m);
        unsafe {
            pw64_c_uvMat4LocalTranslate(&mut cd, tx, ty, tz);
            uvMat4LocalTranslate(&mut rd, tx, ty, tz);
        }
        cmp_mat4("uvMat4LocalTranslate", i, &cd, &rd);
        let (mut cd, mut rd) = (*m, *m);
        unsafe {
            pw64_c_uvMat4Scale(&mut cd, tx, ty, tz);
            uvMat4Scale(&mut rd, tx, ty, tz);
        }
        cmp_mat4("uvMat4Scale", i, &cd, &rd);
        // InvertTranslationRotation (mat2 = the same random matrix).
        let (mut cd, mut rd) = (*m, *m);
        unsafe {
            pw64_c_uvMat4InvertTranslationRotation(&mut cd, &mut cd);
            uvMat4InvertTranslationRotation(&mut rd, &mut rd);
        }
        cmp_mat4("uvMat4InvertTranslationRotation", i, &cd, &rd);
    }
    // Edge matrices through every function.
    for (i, m) in edge_mats().iter().enumerate() {
        let (mut cd, mut rd) = (*m, *m);
        unsafe {
            pw64_c_uvMat4CopyXYZ(&mut cd, &mut cd);
            uvMat4CopyXYZ(&mut rd, &mut rd);
        }
        cmp_mat4("uvMat4CopyXYZ (edge)", i, &cd, &rd);
        for axis in *b"xyz" {
            let (mut cd, mut rd) = (*m, *m);
            unsafe {
                pw64_c_uvMat4RotateAxis(&mut cd, 1.0, axis as core::ffi::c_char);
                uvMat4RotateAxis(&mut rd, 1.0, axis as core::ffi::c_char);
            }
            cmp_mat4(&format!("uvMat4RotateAxis (edge {axis})"), i, &cd, &rd);
        }
        let (mut cd, mut rd) = (*m, *m);
        unsafe {
            pw64_c_uvMat4LocalTranslate(&mut cd, 1.0, -1.0, 0.5);
            uvMat4LocalTranslate(&mut rd, 1.0, -1.0, 0.5);
            pw64_c_uvMat4Scale(&mut cd, 2.0, -1.0, 0.5);
            uvMat4Scale(&mut rd, 2.0, -1.0, 0.5);
            pw64_c_uvMat4InvertTranslationRotation(&mut cd, &mut cd);
            uvMat4InvertTranslationRotation(&mut rd, &mut rd);
        }
        cmp_mat4("uvMat4LocalTranslate/Scale/Invert (edge)", i, &cd, &rd);
    }
}

/// uvMat4MulBA (same shape as Mul) and uvMat4LocalToWorld.
#[test]
fn diff_uvMat4MulBA_LocalToWorld() {
    type F3 = unsafe extern "C" fn(*mut Mtx4F, *mut Mtx4F, *mut Mtx4F);
    let (c_ba, r_ba) = (pw64_c_uvMat4MulBA as F3, uvMat4MulBA as F3);
    let mut rng = Rng::new();
    let mut mats = Vec::with_capacity(N_RANDOM);
    for _ in 0..N_RANDOM {
        mats.push((rng.next_mat4(), rng.next_mat4()));
    }
    for (i, (a, b)) in mats.iter().enumerate() {
        let (mut cd, mut rd) = (unsafe { std::mem::zeroed() }, unsafe { std::mem::zeroed() });
        unsafe {
            c_ba(&mut cd, &mut { *a }, &mut { *b });
            r_ba(&mut rd, &mut { *a }, &mut { *b });
        }
        cmp_mat4("uvMat4MulBA", i, &cd, &rd);
        // LocalToWorld: dst is a separate buffer from vec2.
        let v = rng.next_vec3();
        let (mut cd, mut rd) = (unsafe { std::mem::zeroed() }, unsafe { std::mem::zeroed() });
        let (mut cv, mut rv) = (v, v);
        unsafe {
            pw64_c_uvMat4LocalToWorld(&mut { *a }, &mut cd, &mut cv);
            uvMat4LocalToWorld(&mut { *a }, &mut rd, &mut rv);
        }
        cmp_vec3("uvMat4LocalToWorld", i, &cd, &rd);
    }
    let edges = edge_mats();
    for (i, (a, b)) in edges.iter().cloned().zip(edges.iter().cloned()).enumerate() {
        let (mut cd, mut rd) = (unsafe { std::mem::zeroed() }, unsafe { std::mem::zeroed() });
        unsafe {
            c_ba(&mut cd, &mut { a }, &mut { b });
            r_ba(&mut rd, &mut { a }, &mut { b });
        }
        cmp_mat4("uvMat4MulBA (edge)", i, &cd, &rd);
    }
}

/// The C computes into a temporary when dst aliases src1/src2, then copies;
/// the ports must do the same (the game does alias these).
#[test]
fn aliasing() {
    let mut rng = Rng::new();
    let (mats, vecs) = (
        (0..500)
            .map(|_| (rng.next_mat4(), rng.next_mat4()))
            .collect::<Vec<_>>(),
        (0..500).map(|_| rng.next_vec3()).collect::<Vec<_>>(),
    );
    for (i, (a, b)) in mats.iter().enumerate() {
        // dst == src1 and dst == src2.
        for alias_src2 in [false, true] {
            let (mut cd, mut rd) = (*a, *a);
            unsafe {
                if alias_src2 {
                    pw64_c_uvMat4Mul(&mut cd, &mut cd, &mut cd);
                    uvMat4Mul(&mut rd, &mut rd, &mut rd);
                } else {
                    pw64_c_uvMat4Mul(&mut cd, &mut cd, &mut { *b });
                    uvMat4Mul(&mut rd, &mut rd, &mut { *b });
                }
            }
            cmp_mat4("uvMat4Mul (alias)", i, &cd, &rd);
            // MulBA handles the same aliasing (same temp + copy shape).
            unsafe {
                if alias_src2 {
                    pw64_c_uvMat4MulBA(&mut cd, &mut cd, &mut cd);
                    uvMat4MulBA(&mut rd, &mut rd, &mut rd);
                } else {
                    pw64_c_uvMat4MulBA(&mut cd, &mut cd, &mut { *b });
                    uvMat4MulBA(&mut rd, &mut rd, &mut { *b });
                }
            }
            cmp_mat4("uvMat4MulBA (alias)", i, &cd, &rd);
        }
    }
    for (i, v) in vecs.iter().enumerate() {
        let (mut cd, mut rd) = (*v, *v);
        unsafe {
            pw64_c_uvVec3Copy(&mut cd, &mut cd);
            uvVec3Copy(&mut rd, &mut rd);
            pw64_c_uvVec3Add(&mut cd, &mut cd, &mut cd);
            uvVec3Add(&mut rd, &mut rd, &mut rd);
            pw64_c_uvVec3Cross(&mut cd, &mut cd, &mut cd);
            uvVec3Cross(&mut rd, &mut rd, &mut rd);
        }
        cmp_vec3("vec (alias)", i, &cd, &rd);
        // The game's common shape: vd == va, vb distinct (a += b, a *= s).
        let w = vecs[(i + 1) % vecs.len()];
        let (mut cd, mut rd) = (*v, *v);
        unsafe {
            pw64_c_uvVec3Add(&mut cd, &mut cd, &mut { w });
            uvVec3Add(&mut rd, &mut rd, &mut { w });
            pw64_c_uvVec3Mul(&mut cd, &mut cd, w.y);
            uvVec3Mul(&mut rd, &mut rd, w.y);
            pw64_c_uvVec3Cross(&mut cd, &mut { w }, &mut cd);
            uvVec3Cross(&mut rd, &mut { w }, &mut rd);
        }
        cmp_vec3("vec (alias vd == va)", i, &cd, &rd);
        // LocalToWorld with dst == vec2: the C reads vec2 before writing dst.
        let m = rng.next_mat4();
        let (mut cm, mut rm) = (m, m);
        let (mut cd, mut rd) = (*v, *v);
        unsafe {
            pw64_c_uvMat4LocalToWorld(&mut cm, &mut cd, &mut cd);
            uvMat4LocalToWorld(&mut rm, &mut rd, &mut rd);
        }
        cmp_vec3("uvMat4LocalToWorld (alias)", i, &cd, &rd);
        let (mut cd, mut rd) = (*v, *v);
        let (c, r) = unsafe {
            (
                pw64_c_uvVec3Normal(&mut cd, &mut cd),
                uvVec3Normal(&mut rd, &mut rd),
            )
        };
        if c != r {
            panic!("uvVec3Normal (alias): input {i}: C return {c} != Rust return {r}");
        }
        cmp_vec3("uvVec3Normal (alias)", i, &cd, &rd);
    }
}

/// Layout: the repr(C) structs must match the C unions byte for byte, so the
/// C's `f[i]`/`m[r][c]` accesses agree with the Rust fields.
#[test]
fn layouts() {
    assert_eq!(std::mem::size_of::<Vec3F>(), 12);
    assert_eq!(std::mem::size_of::<Mtx4F>(), 64);
    // Vec3F: f[0] = x, f[1] = y, f[2] = z.
    assert_eq!(offset_of!(Vec3F, x), 0);
    assert_eq!(offset_of!(Vec3F, y), 4);
    assert_eq!(offset_of!(Vec3F, z), 8);
    // Mtx4F: the anonymous struct's member order (xx, yx, zx, wx, xy, …) IS
    // the m[4][4] row-major order.
    let fields: [(&str, usize); 16] = [
        ("xx", 0),
        ("yx", 4),
        ("zx", 8),
        ("wx", 12),
        ("xy", 16),
        ("yy", 20),
        ("zy", 24),
        ("wy", 28),
        ("xz", 32),
        ("yz", 36),
        ("zz", 40),
        ("wz", 44),
        ("xw", 48),
        ("yw", 52),
        ("zw", 56),
        ("ww", 60),
    ];
    for (j, &(name, off)) in fields.iter().enumerate() {
        let actual = match name {
            "xx" => offset_of!(Mtx4F, xx),
            "yx" => offset_of!(Mtx4F, yx),
            "zx" => offset_of!(Mtx4F, zx),
            "wx" => offset_of!(Mtx4F, wx),
            "xy" => offset_of!(Mtx4F, xy),
            "yy" => offset_of!(Mtx4F, yy),
            "zy" => offset_of!(Mtx4F, zy),
            "wy" => offset_of!(Mtx4F, wy),
            "xz" => offset_of!(Mtx4F, xz),
            "yz" => offset_of!(Mtx4F, yz),
            "zz" => offset_of!(Mtx4F, zz),
            "wz" => offset_of!(Mtx4F, wz),
            "xw" => offset_of!(Mtx4F, xw),
            "yw" => offset_of!(Mtx4F, yw),
            "zw" => offset_of!(Mtx4F, zw),
            "ww" => offset_of!(Mtx4F, ww),
            _ => unreachable!(),
        };
        assert_eq!(actual, off, "Mtx4F field {j} ({name}) at wrong offset");
    }
    // Cross-check against the raw f32 view used by the tests: field j is
    // m[j/4][j%4].
    let m = Mtx4F {
        xx: 1.0,
        yx: 2.0,
        zx: 3.0,
        wx: 4.0,
        xy: 5.0,
        yy: 6.0,
        zy: 7.0,
        wy: 8.0,
        xz: 9.0,
        yz: 10.0,
        zz: 11.0,
        wz: 12.0,
        xw: 13.0,
        yw: 14.0,
        zw: 15.0,
        ww: 16.0,
    };
    for (j, &v) in unsafe { mat_as_f32(&m) }.iter().enumerate() {
        assert_eq!(v, (j + 1) as f32, "m[{}][{}] not field {j}", j / 4, j % 4);
    }
}

/// Smoke test: the renamed C symbols resolve and run (this also pulls `uvSqrtF`
/// via `pw64_c_uvVec3Len`, proving the C-side call chain links).
#[test]
fn c_symbols_resolve() {
    unsafe {
        let mut v = Vec3F {
            x: 0.0,
            y: 0.0,
            z: 0.0,
        };
        assert_eq!(pw64_c_uvVec3Len(&mut v).to_bits(), 0.0f32.to_bits());
        let mut m: Mtx4F = std::mem::zeroed();
        pw64_c_uvMat4SetIdentity(&mut m);
        let r = mat_as_f32(&m);
        assert_eq!(r[0], 1.0);
        assert_eq!(r[5], 1.0);
        assert_eq!(r[10], 1.0);
        assert_eq!(r[15], 1.0);
    }
}
