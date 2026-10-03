//! Display-list interpreter benchmark.
//!
//! `cargo run --release -p pw64-gfx --example bench_interp [-- <iterations>]`
//!
//! Runs a synthetic in-flight-sized frame (`tests/synth`) through one
//! long-lived [`Interpreter`] (steady state: texture cache warm, like the
//! game) and prints ms/frame and ns/command. With
//! `PW64_BENCH_CAPTURE=<file>` it replays a real frame captured from the game
//! instead (see `pw64_gfx::capture`; renderer.md "DL interpreter benchmark").

#[path = "../tests/synth/mod.rs"]
mod synth;

use pw64_gfx::{Frame, Interpreter, Memory};
use std::time::Instant;

fn bench(name: &str, mem: &dyn Memory, dl: u32, wide_tags: bool, iters: usize) {
    let mut it = Interpreter::new();
    it.wide_tags = wide_tags;
    let mut f = Frame::default();
    for _ in 0..10 {
        f = it.run(mem, dl);
    }
    let cmds = it.command_count();
    println!(
        "{name}: {cmds} commands, {} tris, {} draws, {} textures, hash {:016x}",
        f.triangle_count(),
        f.draws.len(),
        f.textures.len(),
        synth::frame_hash(&f)
    );
    // `run` (a new Frame per call, as the game does today) and `run_into`
    // (the caller's Frame reused).
    for reuse in [false, true] {
        let mut times: Vec<f64> = (0..iters)
            .map(|_| {
                let t = Instant::now();
                if reuse {
                    it.run_into(mem, dl, &mut f);
                } else {
                    f = it.run(mem, dl);
                }
                std::hint::black_box(&f);
                t.elapsed().as_secs_f64()
            })
            .collect();
        times.sort_by(f64::total_cmp);
        let med = times[times.len() / 2];
        let mean = times.iter().sum::<f64>() / times.len() as f64;
        println!(
            "  {:8} ms/frame: median {:.3}  min {:.3}  mean {:.3}   ns/command (median): {:.1}",
            if reuse { "run_into" } else { "run" },
            med * 1e3,
            times[0] * 1e3,
            mean * 1e3,
            med * 1e9 / cmds as f64
        );
    }
}

fn main() {
    let iters = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(300);
    if let Ok(path) = std::env::var("PW64_BENCH_CAPTURE") {
        let cap = pw64_gfx::capture::load(std::path::Path::new(&path))
            .unwrap_or_else(|e| panic!("{path}: {e}"));
        bench(&path, &cap.mem, cap.dl, cap.wide_tags, iters);
        return;
    }
    let w = synth::build(1);
    bench("synthetic", &w.mem, w.dl, true, iters);
}
