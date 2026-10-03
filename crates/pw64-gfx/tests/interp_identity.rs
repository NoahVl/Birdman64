//! Output identity of the display-list interpreter: the synthetic frame's
//! hash (vertices, draws, texture keys + pixels) must match the value
//! recorded before the interpreter's performance work (texture-key caching,
//! template dirty flags, buffer reuse). If an intended output change breaks
//! these, re-record the constants and say why in the commit.

mod synth;

use pw64_gfx::{Frame, Interpreter, Memory};

/// Recorded with the pre-optimisation interpreter (2026-09-29); checked
/// through `frame_hash_pre_lod` (draws without the `lodp`/`filt` uniforms).
const PRE_LOD_SEED1_WIDE: u64 = 0x0933_a0ae_3d2c_0465;
const PRE_LOD_SEED2: u64 = 0x26d6_8dda_6a83_8b81;
/// Full hashes, re-recorded when draws gained `lodp`/`filt` (per-pixel LOD
/// fraction, 2026-09-29) — the only difference, see the pre-LOD check.
const SEED1_WIDE: u64 = 0xda31_22f8_6dda_a33f;
const SEED2: u64 = 0xf0a3_821b_0e2d_aadb;

fn hash(mem: &dyn Memory, dl: u32, wide_tags: bool, f: fn(&Frame) -> u64) -> (u64, u64) {
    let mut it = Interpreter::new();
    it.wide_tags = wide_tags;
    let cold = f(&it.run(mem, dl));
    // Second run: texture cache warm, per-run state reset.
    let warm = f(&it.run(mem, dl));
    (cold, warm)
}

#[test]
fn synthetic_frame_is_unchanged() {
    let w = synth::build(1);
    let pre = synth::frame_hash_pre_lod;
    assert_eq!(
        hash(&w.mem, w.dl, true, pre),
        (PRE_LOD_SEED1_WIDE, PRE_LOD_SEED1_WIDE)
    );
    assert_eq!(
        hash(&w.mem, w.dl, true, synth::frame_hash),
        (SEED1_WIDE, SEED1_WIDE)
    );
    let w = synth::build(2);
    assert_eq!(
        hash(&w.mem, w.dl, false, pre),
        (PRE_LOD_SEED2, PRE_LOD_SEED2)
    );
    assert_eq!(hash(&w.mem, w.dl, false, synth::frame_hash), (SEED2, SEED2));
}

/// `run_into` (reused frame) produces what `run` does.
#[test]
fn run_into_matches_run() {
    let w = synth::build(1);
    let mut it = Interpreter::new();
    it.wide_tags = true;
    let mut f = it.run(&w.mem, w.dl);
    for _ in 0..2 {
        it.run_into(&w.mem, w.dl, &mut f);
        assert_eq!(synth::frame_hash(&f), SEED1_WIDE);
    }
}

/// Texture memory changing between runs (RDRAM is live in the game) must
/// show up in the next frame exactly as for a fresh interpreter.
#[test]
fn rdram_changes_between_runs_are_seen() {
    let mut w = synth::build(3);
    let mut it = Interpreter::new();
    let before = synth::frame_hash(&it.run(&w.mem, w.dl));
    let mut r = synth::Rng::new(99);
    for t in w.textures.iter().step_by(3) {
        let a = t.addr as usize;
        for b in &mut w.mem.bytes[a..a + 64] {
            *b = r.next() as u8;
        }
        if let Some(tl) = t.tlut {
            w.mem.bytes[tl as usize] ^= 0x5A;
        }
    }
    let after = synth::frame_hash(&it.run(&w.mem, w.dl));
    let fresh = synth::frame_hash(&Interpreter::new().run(&w.mem, w.dl));
    assert_ne!(before, after);
    assert_eq!(after, fresh);
}

/// A recorded capture replays to the same frame after a save/load round trip.
#[test]
fn capture_round_trip() {
    let w = synth::build(4);
    let mut it = Interpreter::new();
    it.wide_tags = true;
    let (frame, cap) = pw64_gfx::capture::record(&mut it, &w.mem, w.dl);
    let dir = std::env::temp_dir().join(format!("pw64-cap-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("cap.bin");
    cap.save(&path).unwrap();
    let loaded = pw64_gfx::capture::load(&path).unwrap();
    std::fs::remove_dir_all(&dir).ok();
    let replay = loaded.run(&mut Interpreter::new());
    assert!(loaded.wide_tags);
    assert_eq!(synth::frame_hash(&replay), synth::frame_hash(&frame));
}
