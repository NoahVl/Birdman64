# iOS port (arm64)

Status: compiles for `aarch64-apple-ios`; CI `ios` boots a simulator build to the first screen (no device runs yet). Platform layer: aarch64.md "Darwin".

- The app is the plain exe (`desktop_main`), bundled by `packaging/ios/build-ipa.sh` (no Xcode
  project). `UILaunchScreen` in Info.plist is required, else iOS letterboxes the app. Windows are
  always the whole screen: no `with_inner_size`/min size/centring — a requested size makes winit
  draw a smaller view; the widescreen default uses the monitor's long side.
- Android-style cfgs (`any(android, ios)`): no gilrs/BLE threads, rfd replaced by the log-only
  `src/ios/rfd.rs` (rfd has no iOS backend), no "Open save folder"/"Quit", no stale-lock toast.
- Data dir = the app's Documents folder (`NSSearchPathForDirectoriesInDomains`, objc2-foundation
  0.3), Files-visible via `UIFileSharingEnabled`; also the ROM scan root (`rom/` + folder);
  `PW64_DATA_DIR` still overrides.
- `first-run` on iOS is a `compile_error!` (iOS can't run code built on the device): release needs the game module added to the IPA before signing.
- Touch controls are wired (`any(android, ios)` in touch.rs/window.rs/settings.rs; both merge
  into `input/mobile_pad.rs`). Not yet: real safe-area insets
  (`touch::Insets::default` for now), suspend/resume + audio session, refresh rate (60 Hz when
  winit reports none).
- GameController pads are wired (`input/ios_gc.rs`, `objc2-game-controller`): the first
  controller with an extended-gamepad profile is polled every frame in the redraw path (main
  thread) and fed through `mobile_pad::set_pad` — the same gilrs `Button` names as a desktop
  pad, stick y flipped to the y-down `Axes` convention, d-pad as buttons (hat = 0). A pad
  press hides the touch controls; disconnect publishes one idle pad.
- stdout/stderr are `dup2`ed into `<Documents>/birdman64.log` (previous run kept as `birdman64-previous.log`): log and panic output reaches the Files app.
- `packaging/ios/run-sim.sh` runs the simulator app on the first iPhone sim (build or `--app`),
  copies a ROM into its `rom/` folder and prints where the log is.
- CI's device IPA has the game C linked in: a dev artifact only, never a release asset.
