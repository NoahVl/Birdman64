# Contributing to Birdman64

Thanks for your interest! Contributions and forks are welcome.

## Build and test

Prerequisites are the same as in the
[README "Building from source"](README.md#building-from-source) section:
Rust stable, the project's C compiler (zig 0.16.0, fetched by the command
below; no LLVM install needed), on Linux also `libasound2-dev`,
`libudev-dev`, `libdbus-1-dev` and `pkg-config`, and a recursive clone (the
decompilation is a submodule).

```sh
git clone --recursive https://github.com/USER/Birdman64
cd Birdman64
cargo run -p pw64-cbuild --example get_zig --features fetch   # once
cargo run --release -p birdman64        # run the game
cargo test --workspace             # run the tests
cargo clippy --workspace --all-targets -- -D warnings   # must be clean
```

The decompiled C compiles on first build (with zig, the same compiler the
release uses on players' machines); you need your own US ROM to actually
play, never committed.

## Legal rules (non-negotiable)

- Never commit, upload, or attach **ROM data or extracted game assets** to a
  pull request or an issue: that includes ROM images, textures, audio,
  models, and in-game text. For rendering bugs, a small cropped screenshot of
  the glitch is fine; no full-screen captures, videos or asset dumps.
- Code may be derived from the MIT-licensed
  [Pilotwings64Decomp](https://github.com/gcsmith/Pilotwings64Decomp).
- Never copy code from [Pilotwings64Recomp](https://github.com/gcsmith/Pilotwings64Recomp):
  it is GPL-3.0 and incompatible with this project's MIT code. Reading it for
  reference is fine; copying is not.

## Pull requests

- One topic per PR.
- `cargo test --workspace` passes and `cargo clippy --workspace --all-targets`
  is clean.
- Keep changes consistent with the surrounding code style, and comment the
  *why* (name the decomp function a port mirrors).

## Bug reports

Open an issue with:

- `crash.log` if the game crashed (it lives next to your saves),
- your GPU model and driver version, your OS,
- the SHA-1 of your ROM (on Windows: `certutil -hashfile rom.z64 SHA1`).

**Never attach the ROM itself.** We do not need it, and we cannot distribute
it.

## Conduct and security

Contributions follow the
[Contributor Covenant code of conduct](CODE_OF_CONDUCT.md); security issues
are reported privately per [SECURITY.md](SECURITY.md), never in a public
issue.
