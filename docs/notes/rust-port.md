# Rust port of the decomp (crates/pw64-kernel)

Goal: port decomp functions to Rust one by one, proving each is
**bit-identical** to the C before the Rust version takes over the game's
calls.

- Formatting the port crates: bare `rustfmt <file>` fails ("let chains are
  only allowed in Rust 2024") because it reads no edition; use
  `cargo fmt -p <crate>` instead.

## Mechanism (one iteration)

1. `crates/pw64-game/ported.txt`: one C function name per line (`#`
   comments). Append the next function here.
2. `crates/pw64-game/build.rs` finds the decomp `.c` file that *defines*
   each name (non-indented line with `name(` not ending in `;`; patch `+`
   lines count too) and compiles that file with `-D<name>=pw64_c_<name>`:
   the C original stays linked under the `pw64_c_` name (for the tests),
   while every other C file still calls `<name>` — resolved by the Rust
   crate. Panics when a name is defined nowhere or in two files.
   `ported.txt` is `rerun-if-changed`; the `-D`s are part of the object's
   cache key, so adding/removing a name rebuilds just that TU.
3. Port the function in `crates/pw64-kernel/src/lib.rs`:
   `#[unsafe(no_mangle)] pub unsafe extern "C" fn <Name>(<C signature>)`
   with doc comment "mirrors `<name>` (decomp/src/kernel/<file>.c)".
   `#[repr(C)]` `Vec3F`/`Mtx4F` mirror the decomp unions' layouts
   (`Mtx4F` field `[r*4+c]` = `m[r][c]`: `yx` = `m[0][1]`, `xy` = `m[1][0]`).
4. Extend `crates/pw64-game/tests/kernel_diff.rs`: declare `pw64_c_<name>`,
   run C and Rust on ≥ 10 000 deterministic xorshift inputs plus `EDGES`
   (±0, ±1, ±tiny, ±huge, ±denormal, ±inf, NaNs) and the aliasing shapes
   the game uses; outputs must be bit-identical (`same_bits`).
5. Run `cargo test -p pw64-game --test kernel_diff` **and** the same with
   `--release`, then the full gates, then the scripted-flight check.

## -D rename: what it does and doesn't touch

- Token-level, whole defining TU: the header prototype is renamed too
  (consistent), prefixes (`uvMat4Copy` vs `uvMat4CopyXYZ`) are separate
  tokens. Other TUs are untouched, so their prototypes stay `<name>`.
- **Callers inside the defining TU call the C original** (`pw64_c_…`),
  e.g. matrix.c's `_uvDbMstackPush` → C `uvMat4Copy`/`uvMat4MulBA`. Harmless
  (bit-identical) and it keeps the `pw64_c_` references pure-C for the tests;
  the Rust takes over those calls once the caller itself is ported.
- A same-named `static` in another file, or a `#define` of the name, would
  make the scan panic / misbehave — none exist for the current names.
- Taking a ported function's address gives different pointers in the
  defining TU vs elsewhere; no current caller compares them.

## Floating-point and semantics rules (why the ports are identical)

- The C is compiled with `-ffp-contract=off` (both flag sets in build.rs):
  no FMA contraction of `a*b+c`. Rust never fuses automatically either.
- Never reimplement libm or game math wrappers: call the same function
  from Rust via `extern "C"` (`uvSqrtF`, `uvSinF`, `uvCosF` are declared in
  lib.rs and linked from the C archive).
- Mirror the C statement by statement: same operation order and
  association, same intermediates, no algebraic simplification. All
  decomp literals so far are `f`-suffixed (no f64 promotion); check each
  new function for unsuffixed literals / `sqrt` vs `sqrtf`.
- **NaN payload/sign is not reproducible** and is exempt (`same_bits`: any
  NaN == any NaN): LLVM treats fadd/fmul as commutative and x86 SSE returns
  the first operand's NaN, so a `--release` build diverged from the C on
  payloads (C-vs-C across opt levels would too). NaN-*ness*, ±0, inf and
  denormals must match exactly. The game never inspects NaN bits.
- **Aliasing: use raw-pointer accesses** (`(*vd).x = (*va).x + (*vb).x`)
  whenever dst may alias a source (the game does `uvVec3Add(&a, &a, &b)`,
  `uvMat4Copy(x, x)`). A live `&mut` + `&` to one buffer is UB and lets
  rustc reorder loads past stores. References are fine only once every
  source value is read into locals (Cross, Normal, Mul-into-temp).
- Uninitialised C locals are zero (`-ftrivial-auto-var-init=zero`): e.g.
  `uvMat4RotateAxis` with an axis other than x/y/z copies a zero `temp`
  into dst; the port zero-inits its temp to match.
- C `char` is unsigned in this build (`-J` / `-funsigned-char`); Rust
  `c_char` is `i8`. Moot for 'x'/'y'/'z', but use `u8` for any char
  argument/field that can be ≥ 0x80.
- Next batch hazard: C `(s32)f` of an out-of-range/NaN float is
  `cvttss2si` = `0x8000_0000` in this build, Rust `as i32` saturates (NaN →
  0) — use `f.to_int_unchecked::<i32>()` (same instruction) or test the
  edge explicitly (`uvMat4CopyF2L`).

## Linking (the non-obvious parts)

- rustc drops an **unreferenced** dependency from the link line; the C's
  references to ported symbols don't count. So `pw64-game` depends on
  `pw64-kernel` normally and `lib.rs` has `use pw64_kernel as _;` (like
  `use pw64_platform as _;`): every binary that links pw64-game (the exe,
  its tests, any future tool) gets the ports automatically, and the archive
  (whole-archive) precedes the kernel rlib on the line, so GNU ld resolves
  both directions (kernel → `uvSqrtF` in the archive, archive → ports).
- The diff tests live in `crates/pw64-game/tests/kernel_diff.rs`
  (`use pw64_game as _;` links everything; no `#[link]` hacks, no
  dev-dependency cycle).
- pw64-kernel `[lib] test = false, doctest = false`: its lib-test harness
  would reference `uvSqrtF` etc. without the C archive and can't link.
  (Batch 1/2 had the tests in pw64-kernel with a pw64-game ⇄ pw64-kernel
  dev-dep cycle, `use pw64_kernel as _;` in pw64's main.rs, `#[link(name =
  "pw64game")]` + `use pw64_platform as _;` in the test; replaced by the
  above, 2026-09-30 review.)

## Verification per batch

- `cargo test -p pw64-game --test kernel_diff` (debug and `--release`) and
  the full workspace gates. Mutation-checked: swapping one addition's
  association in `uvVec3Len` fails the random inputs immediately.
- Scripted flight (`fly_hang_glider.txt`, 2700 retraces,
  `PW64_DUMP_FRAMES=600,2700` → `tmp/frame_00600.png`/`_02700.png`, fresh
  `PW64_EEP`), then look at the PNGs. Headless runs are not
  pixel-deterministic (sweep.md), so an A/B pixel diff is only a hint.
- CI's Linux job (clang, ELF) runs the same tests — watch it after pushing.

## Ported so far

- Batch 1: all 9 `uvVec3*`/`uvVec2Dot` (vector.c) + `uvMat4Copy`/`Mul`/
  `SetIdentity` (matrix.c).
- Batch 2 (matrix.c): `CopyXYZ`, `MulBA` (dst = src2 × src1), `RotateAxis`,
  `LocalTranslate`, `Scale`, `InvertTranslationRotation`, `LocalToWorld`,
  `SetFrustrum`, `SetOrtho`, `SetQuaternionRotation`.
- Next: the rest of matrix.c (`uvMat4UnkOp6`, `uvMat4SetIdentityL`,
  `uvMat4CopyL`, `uvMat4CopyL2F`, `uvMat4CopyF2L` — fixed-point, see the
  cast hazard above; `_uvDbMstack*` touch C globals), then the pure helpers
  in `kernel/math.c` other than the libm wrappers.
