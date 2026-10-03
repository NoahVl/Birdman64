//! T7/T8 acceptance (docs/notes/first-run-build.md): builds the game module
//! with zig from the embedded kit (our C, the shim and the ops patches ship
//! inside pw64-cbuild; nothing is read from crates/pw64-game at runtime),
//! from the dev decomp checkout or a real network fetch of the pinned commit,
//! for the dylib test + flight check.
//!
//! ```sh
//! # dev: hash-checks decomp/ and compiles it
//! cargo run -p pw64-cbuild --example build_module -- \
//!     --zig tmp/zig/zig.exe --decomp-dir decomp --out tmp/firstrun
//! # network: downloads the pinned commit, verifies every hash, then builds
//! cargo run -p pw64-cbuild --example build_module -- \
//!     --zig tmp/zig/zig.exe --out tmp/firstrun --fetch
//! # or from a saved codeload zip: PW64_DECOMP_ZIP=<zip> (fetch picks it up)
//! ```
//!
//! Prints the module path (for `PW64_GAME_DLL`, `dylib::load`, the flight
//! check). `--fetch` honours the T6 overrides via `fetch::decomp_tree`. The
//! ABI string defaults to T10's `DLL_ABI` (`kit::abi`): the same value the
//! first-run exe expects and the cache key's ABI part; `--abi` overrides it
//! for tests (a module built for another ABI is refused by the loader).
//!
//! Requires the crate's `fetch` feature (`--decomp-dir` is pure build code,
//! but the example shares one main).

use pw64_cbuild::Progress;
use pw64_cbuild::build::{self, BuildError, Module, Opts};
use std::path::PathBuf;

struct Args {
    zig: Option<PathBuf>,
    decomp_dir: Option<PathBuf>,
    out: Option<PathBuf>,
    abi: Option<String>,
    fetch: bool,
}

fn main() -> Result<(), String> {
    let args = parse_args()?;
    let manifest = pw64_cbuild::manifest::parse(pw64_cbuild::manifest::DECOMP_MANIFEST)
        .map_err(|e| format!("decomp-manifest.txt: {e}"))?;

    // Tree: --decomp-dir (hash-checked), else the T6 fetch (env overrides,
    // cache, network).
    let decomp_dir = if let Some(dir) = &args.decomp_dir {
        pw64_cbuild::fetch::verify_tree(dir, &manifest)
            .map_err(|e| format!("--decomp-dir {}: {e}", dir.display()))?;
        dir.clone()
    } else if args.fetch {
        let out = args.out.as_ref().ok_or("--out is required with --fetch")?;
        pw64_cbuild::fetch::decomp_tree(&manifest, out, &mut report).map_err(|e| e.to_string())?
    } else {
        return Err("--decomp-dir <dir> or --fetch is required".into());
    };

    // T10: the real DLL_ABI (exe version + hash of imports list/shim/kit),
    // computed from the same embedded kit the builder unpacks.
    let abi = args
        .abi
        .clone()
        .unwrap_or_else(|| pw64_cbuild::kit::abi(env!("CARGO_PKG_VERSION"), pw64_cbuild::KIT));

    let out = args
        .out
        .clone()
        .unwrap_or_else(|| PathBuf::from("tmp/firstrun"));
    let opts = Opts {
        zig: args.zig.expect("parse_args requires --zig"),
        decomp_dir,
        out_root: out.clone(),
        abi: abi.clone(),
        key_extra: abi, // the cache key's ABI part == DLL_ABI
        commit: manifest.commit.clone(),
    };
    let module: Module = build::build_module(&opts, &mut report).map_err(|e: BuildError| {
        eprintln!("{e}");
        format!(
            "build failed; diagnostics: {}",
            opts.out_root.join("build").display()
        )
    })?;
    println!("{}", module.library().display());
    println!("map:        {}", module.map.display());
    println!("warnings:   {}", module.warnings_log.display());
    Ok(())
}

/// One progress line per report (T9 replaces this with the progress window).
fn report(p: Progress) {
    let percent = (p.fraction * 100.0).round() as i32;
    println!("{:?}: {percent}%", p.step);
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        zig: None,
        decomp_dir: None,
        out: None,
        abi: None,
        fetch: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let value = |a: &str, it: &mut dyn Iterator<Item = String>| -> Result<String, String> {
            it.next().ok_or_else(|| format!("{a}: missing value"))
        };
        match a.as_str() {
            "--zig" => args.zig = Some(PathBuf::from(value(&a, &mut it)?)),
            "--decomp-dir" => args.decomp_dir = Some(PathBuf::from(value(&a, &mut it)?)),
            "--out" => args.out = Some(PathBuf::from(value(&a, &mut it)?)),
            "--abi" => args.abi = Some(value(&a, &mut it)?),
            "--fetch" => args.fetch = true,
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    if args.zig.is_none() {
        return Err("--zig <path to zig> is required".into());
    }
    if args.out.is_none() {
        return Err("--out <dir> is required".into());
    }
    Ok(args)
}
