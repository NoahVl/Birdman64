//! Packs the first-run build kit into `$OUT_DIR/kit.bin` (T8,
//! docs/notes/first-run-build.md): `pw64-game`'s `native/**` and `dylib/*`
//! plus the context-free ops generated from its `patches/`, as one
//! deterministic byte string that `lib.rs` embeds with `include_bytes!`
//! (`pw64_cbuild::KIT`) and the release exe ships inside itself.
//!
//! The packer/reader code is shared with the library by including the same
//! source files (`#[path]`): one implementation, so the generator and the
//! builder's reader cannot drift apart (and the ops conversion is exactly
//! `ops::from_unified_diff`/`to_text`, pinned by the kit tests).

// The shared modules are the library's; the build script only calls the
// packer half, so their other items are "unused" in this compilation.
#![allow(dead_code)]

#[path = "src/kit.rs"]
mod kit;
#[path = "src/ops.rs"]
mod ops;

fn main() {
    let manifest = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let game = manifest.join("..").join("pw64-game");
    println!("cargo:rerun-if-changed={}", game.join("native").display());
    println!("cargo:rerun-if-changed={}", game.join("dylib").display());
    println!("cargo:rerun-if-changed={}", game.join("patches").display());

    let mut entries = Vec::new();
    kit::collect_files(&game.join("native"), "native", &mut entries)
        .unwrap_or_else(|e| panic!("kit: {e}"));
    kit::collect_files(&game.join("dylib"), "dylib", &mut entries)
        .unwrap_or_else(|e| panic!("kit: {e}"));
    entries.push((
        kit::OPS_PATH.to_string(),
        kit::ops_text(&game.join("patches")).unwrap_or_else(|e| panic!("kit: {e}")),
    ));

    std::fs::write(
        std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("kit.bin"),
        kit::write(&entries),
    )
    .unwrap_or_else(|e| panic!("writing kit.bin: {e}"));
}
