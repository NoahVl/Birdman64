# iOS port (arm64)

Status: compiles for `aarch64-apple-ios`; CI job `ios` builds an unsigned IPA and boots a
simulator build to the first screen. No device runs yet. Platform layer: aarch64.md "Darwin".

- The app is the plain exe (`desktop_main`), bundled by `packaging/ios/build-ipa.sh`
  (no Xcode project). `UILaunchScreen` in Info.plist is required, else iOS letterboxes the app.
- Android-style cfgs (`any(android, ios)`): no gilrs/BLE threads, rfd replaced by the log-only
  `src/ios/rfd.rs` (rfd has no iOS backend), no "Open save folder"/"Quit", no stale-lock toast.
- Data dir = the app's Documents folder (`NSSearchPathForDirectoriesInDomains`, objc2-foundation
  0.3), shown in the Files app via `UIFileSharingEnabled`; it is also the ROM scan root
  (`rom/` and the folder itself). `PW64_DATA_DIR` still overrides.
- `first-run` on iOS is a `compile_error!`: iOS can't run code built on the device. Release
  distribution needs the game module built elsewhere and added to the IPA before signing.
- Not wired yet: touch controls, suspend/resume + audio session, refresh rate
  (falls back to 60 Hz when winit reports none), GameController pads.
- CI's device IPA has the game C linked in: a dev artifact only, never a release asset.
