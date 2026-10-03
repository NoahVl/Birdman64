# Packaging notes (Linux)

Files here are templates the `release.yml` Linux job assembles into
`Birdman64-x86_64.AppImage` and `birdman64-linux-x64.tar.gz`:

- `birdman64.desktop` - desktop entry inside the AppImage (also drives
  `appimagetool` validation). Icon name `birdman64`.
- `birdman64.png` - the 256x256 app icon for the desktop entry and AppImage.
- `AppRun` - wrapper the job marks executable (`chmod +x` after checkout,
  so git does not need to track the exec bit). It exports
  `PW64_DATA_DIR=${XDG_DATA_HOME:-$HOME/.local/share}/birdman64` when
  unset: the AppImage's own directory is read-only (squashfs), while the
  exe's cache (the compiled game module) must be on an exec-mountable,
  writable filesystem (`crates/birdman64/src/paths.rs` rule 1).

Not packaged: ROM, decomp sources, extracted assets (CONTRIBUTING.md). The
game module is compiled on the player's machine from the downloaded,
hash-checked decomp (docs/notes/first-run-build.md).
