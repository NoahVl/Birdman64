# In-game settings screen

## Where files live

All host-side files land in one directory, `paths::data_dir()`
(`crates/birdman64/src/paths.rs`, decided once in this order):

- `PW64_DATA_DIR` (created when missing).
- Developer builds (exe path has a `target` component): the cwd.
- Portable: the exe's dir, when writable (probed once).
- Per-user: `%APPDATA%\Birdman64` / `$XDG_DATA_HOME/birdman64` /
  `$HOME/.local/share/birdman64` (created).

Contains: `pw64.toml` (config.rs `path()`, was cwd-relative),
`pw64.eep` (headless.rs default, `PW64_EEP` still wins), `crash.log`
(panic hook). `pw64_platform::headless::set_default_eep_path` sets the
`.eep` fallback. Startup prints `[paths] data dir: <path>`.


Goal: graphics/controller/mapping/fullscreen settings from inside the game,
without extending the decomp C. Rust-side overlay in `pw64` (`settings.rs` +
`window.rs` hook), backed by `pw64.toml` (config.rs).

> The design below is kept as written; the sections after "Implementation
> notes" record how it was built and where it changed.

## Approach

- **Trigger:** a dedicated button the game can never read — the N64 has no
  Select-type button, and `input.rs` maps only the 11 N64 slots
  (`GAMEPAD_SLOTS`): **Select** (DualSense *Create*, Switch *Capture*) on
  every pad + F10 on the keyboard. No overlap with the game's Start/pause,
  no long-press timing, no race (only the overlay consumes the button);
  behaves identically everywhere — flight, menus, over the game's pause
  menu. Unmapped in `[input.gamepad]` on purpose; if a player rebinds a slot
  onto it, that slot simply stops reaching the game (one consumer).
  Configurable once the config lands (`[input] settings_pad_button` /
  `settings_key`). Mode/Guide/Home rejected: OS-intercepted, unreliable.
- **Navigation:** the pad that opened the overlay navigates it; other pads
  stay frozen with the game.
- **Pause model:** while open, pause the OS core (VI clock stops; the
  coroutine threads sit in their waits) and stop delivering pad input.
  Single-player + deterministic clocks make this safe and matches emulator
  pause menus; a live-overlay-over-running-game needs input routing through
  the C side — not worth it. Needs: an os-core pause flag in
  `pw64-platform` (`os::run` loop skips advancing the clock while set) —
  small, self-contained.
- **Rendering:** egui-wgpu on the shared device, drawn after the game frame
  in the window thread, above it (own render pass). Keeps pw64-gfx and the
  game's own display lists untouched — the overlay is presentation-layer
  only, so game fidelity is unaffected. Alternative (rendering the UI with
  pw64-gfx Fixed draws + the game's UVFT fonts) would need C-side font
  access — rejected.
- **Settings → effect:**
  - Fullscreen toggle: live (`winit` `set_fullscreen`). Easy.
  - Volume / mute: live (a setter on the audio output ring; audio.rs owns
    it — small addition).
  - MSAA, scale, widescreen: `RenderOptions` are fixed at `Renderer::new`.
    Recreating the Renderer on change looks feasible (pipelines are cached
    per renderer; the window thread owns it) — decide live-recreate vs
    "applies after restart" during implementation; start with restart +
    a "restart to apply" hint, upgrade if it turns out trivial.
  - Input mappings: read/written via config.rs; the screen edits the
    in-memory mapping and writes `pw64.toml` on OK.
- **Config writes:** round-trip through `toml::Value` so keys the screen
  doesn't know survive; write on apply (atomic rename), not on every
  keystroke.

## Design questions

1. Is stopping the VI clock at an arbitrary host moment safe in the
   cooperative OS (threads mid-`osRecvMesg` with timers queued)? Answered
   under "Pause" below: the clock is frozen exactly, so no timer burst.
2. Start race: dissolved. The overlay uses a dedicated Select/Capture
   button the game never reads, so there is one consumer per press by
   construction. The game's own pause menu and the overlay are independent
   layers.
3. Renderer recreation thread-safety + memory spike (two Renderers alive
   during the swap) vs honest restart-to-apply: restart-to-apply chosen for
   MSAA/widescreen.
4. egui dep weight in the release build: compile-time and binary size only,
   no per-frame cost while closed.

BLE pads use the same mapping UI and report their Capture button through
our own driver.

## Implementation notes

Code: `crates/birdman64/src/settings.rs` (overlay + nav + config write), hooks in
`window.rs`, `input.rs`, `config.rs`, `audio.rs` (live volume) and
`pw64-platform/src/os/{mod,time}.rs` (pause).

- **Trigger:** `settings::key()` / `pad_button()` resolve once from config
  (default F10 / `Button::Select`). Keyboard key handled in `window.rs`'s
  KeyboardInput before the game gets anything. Pad button: the input thread
  edge-detects `is_pressed(settings_btn)` across all pads (Select is not in
  `GAMEPAD_SLOTS`, so `gamepad_pad` never maps it — verified) and posts a
  new `UserEvent::Settings` through a static `EventLoopProxy` (`window::PROXY`,
  `notify_settings()`); no-op headless. `[input]` keys are **flat**
  (`settings_key = "F10"` under `[input]`, not `[input.settings]`).
- **Pause:** `pw64_platform::os::PAUSED` (AtomicBool) + `os::set_paused()`.
  `os::run` checks it at the top of each iteration: freeze the clock, sleep
  2 ms, skip `run_ready`. Answer to design Q1: the clock is `Instant`-based
  (`start.elapsed() + skipped`); `retraces_due` already re-bases
  `next_due = now` after a stall > 4 retraces (so VI never burst), but
  **periodic timers would burst** (each fires its backlog) and throttle
  runs would replay game time. So the pause freezes the clock exactly:
  `Clock::freeze()` pins `now()` and `unfreeze()` adds the wall time spent
  paused to `skipped` — no counts advance, no timers/retraces fire, resume
  is seamless (unit test `freeze_absorbs_wall_time`). Pause takes effect at
  the loop top, so at most a few ms of game time run after the toggle (no
  input reaches the game in that window).
- **Input gating:** while `settings::is_open()`, `input::publish` stores the
  merged pad in `OVERLAY_PAD` and publishes an idle pad to the game;
  `input::overlay_toggled(open)` re-routes on open and close, and on close
  masks still-held buttons from the game until released (the closing A /
  Start must not leak in). `release_all_keys()` on open (no stuck keys);
  whatever the pad holds at open is taken as `prev_pad` (no phantom press). The overlay polls `overlay_pad()` per frame and does its
  own edge detection + hold-repeat (350 ms delay / 120 ms repeat) for
  d-pad/stick up-down, A confirm, B/Start back; keyboard arrows / Enter /
  Space / Escape through `Overlay::on_key` (winit repeats forwarded so
  holding scrolls). The pad that opened it navigates it; other pads are
  frozen with the game (single merged pad — same as before).
- **Rendering:** plain egui + egui-wgpu, **no egui-winit** — window.rs
  feeds the overlay winit keys (while open) and cursor/click events only;
  egui gets `RawInput{time, screen_rect, events}` + the root viewport's
  `native_pixels_per_point` (without it egui paints at 1 px/pt: on HiDPI
  the panel lands in the top-left 1/scale of the window and clicks miss)
  and `ctx.run`. Hover selects a row only while the mouse moves (else a
  resting cursor snaps pad/keyboard selection back). egui-wgpu
  `update_buffers` + own render pass (LoadOp::Load) on the surface view
  after the game render/scale blit, before present. While closed:
  zero egui work, no extra request_redraw — fidelity guard holds. While
  open the render self-schedules (`request_redraw` at the end of
  `Overlay::render`) since no Frame events arrive while paused.
- **Dep pin (important):** egui-wgpu 0.36 needs wgpu 30 — **0.33.0 is the
  last line built against wgpu 27** (ours); egui/egui-wgpu pinned to
  `"0.33"` in `[workspace.dependencies]`, egui-wgpu with
  `default-features = false` (its defaults only add wasm/webgl bits).
  egui defaults (= `default_fonts`) kept. API notes: `ScreenDescriptor
  { size_in_pixels, pixels_per_point }`, `textures_delta.free` is a plain
  `TextureId` vec, `Renderer::render(&self, &mut RenderPass<'static>, …)`
  needs `forget_lifetime()`.
- **Settings → effect:** fullscreen, volume, supersampling scale and scale
  filter are live (`Overlay::apply_live` writes `Gpu::scale/filter` each
  frame; the offscreen target is keyed on its render size, so a scale
  change recreates it, scale 1 frees it). MSAA / widescreen / "Fill screen"
  / "Frame rate" /
  "V-Sync" are staged:
  rows start at the **running** values (env > config > default, captured
  once) and show "(restart)" only when the staged value differs — a live
  Renderer recreation would also need `pw64_game::set_widescreen_aspect`
  on the game thread (racy). Host actions (fullscreen) are applied inside
  `render`, which has the window (the first version queued them in a vec
  that `render` cleared first, so keyboard/pad fullscreen toggles were
  lost). Save writes all of them via `config::save_settings` (round-trip
  through `toml::Table`: unknown keys survive, comments don't;
  `pw64.toml.new` written + `sync_all` + rename; only NotFound counts as
  "no file" — other read errors and malformed files are refused, never
  overwritten; floats rounded to 3 decimals). A failed Save keeps the
  overlay open with the error under the row (later replaced by auto-save on
  close, below). Live changes stay live even if the player closes without
  saving. `PW64_*` env vars still override
  `pw64.toml` at startup — noted in the footer.
- **Volume:** permille atomic in `audio.rs` (`set_volume`/`volume`), scaled
  in the AI sink (copy only when < 100%, so the hot path is untouched);
  initial value from `[graphics] volume` (default 1.0 — `Graphics` got a
  manual `Default` because the derived one gave 0.0).
- **Bindings:** `input::{keyboard,gamepad}_binding_rows()` return the
  resolved bindings as Debug-formatted winit/gilrs names, which
  `parse_key`/`parse_gamepad_button` accept as-is. First shown read-only;
  now editable on the Controls page (below).

## Settings-shot hook and toasts

- **`PW64_SETTINGS_SHOT=<n>`:** in `App::redraw`, once `presented >= n`
  (before the gpu borrow): `toggle_settings()`, feed each
  `PW64_SETTINGS_KEYS` name through `Overlay::on_key(code, false, true)`
  (`input::parse_key`; unknown names noted and skipped), then 48 redraws
  later `hle::shot_png` the surface texture to `tmp/win_settings.png` and
  `quit(0)`. `PW64_SETTINGS_SHOT` requests `COPY_SRC` on the surface itself
  (like `PW64_WIN_SHOT`); a surface without it skips the capture with a note.
- **egui first-frame opacity (important for all overlay captures):** an
  egui `Area`/`Window` fades in over `ctx.style().animation_time`; on its
  very first frame `Painter::opacity_factor == 0.0` and **every shape is
  emitted as `Shape::Noop`** (epaint tessellates them to 0 primitives). A
  capture on the same frame the overlay is opened shows only the game. The
  hook waits 48 redraws (~0.33 s at 144 Hz) before shooting; the overlay keeps
  redrawing while open (its own `request_redraw`), so no extra pump needed.
- **Toast:** `crates/birdman64/src/toast.rs`
  (`Toast { ctx, renderer, msgs: Vec<(String, Instant)>, started }`,
  `show(text, secs)`, `render(gpu, view)`). Bottom-left `egui::Area`
  anchored per message (44 px stack spacing), `Frame::new()` dark
  translucent fill (alpha fades over the last 1 s), 14 pt label; egui pass
  via `settings::raw_input` + `settings::paint(.., LoadOp::Load)`. Empty
  `msgs` → no egui work at all (fidelity guard). Drawn in `redraw` right
  after `fb_present`, before the `PW64_WIN_SHOT` readback (shots show it)
  and before the overlay.
- **Hint:** after the first drain with a new frame (`first_frame_hint`,
  once per process): if `PW64_INPUT_SCRIPT` unset, `PW64_SETTINGS_SHOT`
  unset and `config::get().ui.settings_hint_shown` false →
  `show("F10 / Select: Settings", 8)` + `config::save_ui_flag()` (writes
  `[ui] settings_hint_shown = true`; same `read_table`/`write_table`
  round-trip discipline as `save_rom_path`; write failure is logged only,
  the hint repeats next launch). Scripted runs and settings-shot runs never
  show it (they must not end up in captures). Flag added to `Raw`/`Config`
  (`config::Ui`); unit test `ui_hint_flag_round_trips`.
- **Checks:** `PW64_SETTINGS_SHOT=300 PW64_WIN_SHOT=1
  PW64_SETTINGS_KEYS=Down,Down,Right,Enter PW64_NO_AUDIO=1
  PW64_NO_INPUT=1` → `tmp/win_settings.png` shows the panel with the
  navigated row changed. Hint: fresh `PW64_DATA_DIR`, `PW64_WIN_SHOT=120`
  windowed run (kill the process after the shot; headless
  `PW64_MAX_RETRACES` skips the window entirely) → hint visible bottom-left
  + `pw64.toml` gets `[ui]`; 2nd run → no hint.
- **Gotcha:** a shell that pipes the whole game output through
  `Select-Object` can outlive its timeout with nothing printed: redirect
  stderr to a file (`2> tmp/…txt`) and read that.

## Live frame rate / V-Sync

- `Running` no longer holds fps/vsync; both are live in `Gpu`
  (`fps`, `vsync`, `present_modes`, `present_dirty`). `apply_live`
  (overlay) sets them + `present_dirty`; the fullscreen toggle also sets
  `present_dirty` (different monitor). `redraw` checks the flag after the
  overlay render and calls `Gpu::apply_present()`, which recomputes rate
  (`opts::present_rate`) and mode (`opts::present_mode`) and reconfigures
  the surface. The old `monitor_moved` early return at `fps == Monitor`
  is gone (it was why fps/vsync changes needed a restart).
- Free fn `queue_rate()` shared by start + apply (queueing the OS-core
  present rate is unchanged: on `Moved`, `ScaleFactorChanged` and apply).
- Rows: Frame rate and V-Sync apply live (no restart note); Frame rate
  shows the value "set by V-Sync" and is greyed
  (`add_enabled_ui(false)`, no hover) while V-Sync is on; selecting it is
  a no-op then. Fullscreen stays restart-noted (monitor capture).
- Check: settings-shot hook navigating to V-Sync + Right →
  `tmp/win_settings.png` shows Frame rate greyed + "set by V-Sync", the
  V-Sync row On; log line `present mode Mailbox → Fifo` (live switch).
- Gotcha: windowed game runs stall when the desktop is idle; bound them
  (`timeout 240` in Git Bash, script file not inline quoting).
- **wgpu gotcha:** two ways a live
  `Surface::configure` panics via the fatal error sink:
  1. `SurfaceOutput must be dropped before a new Surface is made` (saw it
     with Mailbox → Fifo): the acquired surface texture's view must be
     DROPPED before reconfiguring. `present()` CONSUMES the `SurfaceTexture`
     in wgpu 27 (takes `self`; the output goes back to the swapchain with
     it), so a `drop(tex)` after `present()` does not compile (E0382).
     Fix in `redraw`: `drop(view);` after `present()` and before the
     `present_dirty` apply. Validated with the Mailbox → Fifo live switch:
     no panic, no wedge.
  2. `Failed to wait for GPU to come idle before reconfiguring the
     Surface` (`GpuWaitTimeout`): configure waits for the device to go
     idle and panics while submissions are in flight, which our own
     just-queued work always is. Fix: `device.poll(Wait)` right before
     `surface.configure` in `refresh_present`.
  Watch for it anywhere else a live reconfigure could race (e.g. the
  `Resized` handler, a plain configure; it runs between redraws so no
  texture is held, but the idle-wait applies the same if it ever hits).
- Live present-mode change: `surface.configure` must run after `tex.present()`; reconfiguring while
  the frame's surface texture is held wedged the swapchain (no redraw ever again after Mailbox → Fifo).
- Git Bash `timeout N ./target/release/birdman64.exe` does NOT kill the windowed exe on Windows: a stale
  instance keeps running and locks the exe for the next build ("Access is denied"). `taskkill //F //IM birdman64.exe`.

## Restart labelling and render resolution

- **Restart labelling:** `Overlay::save()` returns `Result<Option<String>, String>`; a Save with restart-only
  items staged queues `saved_toast` and closes the overlay (`confirm`). The window takes it via
  `take_toast()` and `toast.show(.., 8)`. Placement matters: take the toast BEFORE the toast
  pass in `redraw` (not after the overlay render) so it shows on the very frame Save closed
  the overlay; after the overlay pass it only appeared once frames resumed, and the
  settings-shot capture (same frame) missed it entirely. `RichText` cannot wrap a
  `LayoutJob`: use `egui::WidgetText::from(job)` (`rich_row` helper) for the two-part
  restart-row label (value + weak "(restart)"). Banner: "Restart Birdman64 to apply: <items>"
  while any staged != running; Save row hint "& close".
- **Render resolution:** row "Render resolution"; `SCALE_STEPS = [0.5, 0.75, 1.0, 1.5, 2.0, 3.0, 4.0]`;
  value "100% (1200x900)" from `Gpu::fb_size_for_scale` -> pure `Gpu::fb_size_for(scale, out,
  max_tex)` (unit-tested); min scale 0.5 in `opts::scale` + config validation (key stays
  `scale`); weak hint line under the row.
- **Check:** staging MSAA + Save via the settings-shot hook shows the banner "Restart
  Birdman64 to apply: MSAA" and "MSAA: 8x (restart)"; after Enter, "[config] saved" in the
  log and the toast "Saved. Restart Birdman64 to apply: MSAA" bottom-left. `PW64_SCALE=0.5`
  is blurry but correct, `PW64_SCALE_FILTER=nearest` blocky.
- **pw64.toml caveat:** saving during automated runs writes the repo-root `pw64.toml`
  (gitignored); delete it before runs that must start from defaults.

## Display mode

- **opts.rs:** `DisplayMode {Windowed, Borderless, Exclusive}` (`as_str`,
  case-insensitive parse), `Res {w, h, hz: Option<u32>}` +
  `parse_resolution` ("WxH" or "WxH@Hz", rejects 0/junk, hz capped at 1000);
  `display_mode()` (env > config > Windowed), `fullscreen_resolution()`
  (env > config > None = desktop mode). Note: its `precedence` call needs
  `map(Some)` on every source because `T = Option<Res>` there.
- **window.rs:** `Gpu` holds `display_mode`, `fullscreen_res`,
  `fullscreen_kind` (last fullscreen kind; F11 = Windowed <-> kind),
  `exclusive_ok` (false on Wayland, checked via
  `ActiveEventLoopExtWayland::is_wayland`), `exclusive_mhz`.
  `set_display_mode(mode)`: Exclusive with no candidate mode falls back to
  borderless (logged); `fullscreen_kind` always records what actually
  happened. Free fn `exclusive_mode(monitor, Option<Res>)`: size-matched
  candidates, prefer res.hz (else the monitor's current mHz), then nearest
  rate, then highest bit depth. winit API: `MonitorHandle::
  refresh_rate_millihertz()` returns `Option<u32>`, but `VideoModeHandle`'s
  (and `size()`, `bit_depth()`) return plain values. Startup applies the
  configured mode right after Gpu construction, then `apply_present`.
- **config.rs:** `[graphics] display_mode` + `fullscreen_resolution`
  (string), validated in `from_raw` like fps; `SavedSettings` writes them
  (res `Some(None)` removes the key; the `put` closure must be scoped so a
  later `graphics.remove` passes borrows).
- **settings.rs:** rows "Display mode: …" (live, skips Exclusive when
  `!exclusive_ok`) and "Resolution: …" (enabled only while the staged mode
  is Exclusive, value "exclusive only" otherwise). Choices: desktop first,
  deduped by (w,h,hz), highest first. Left/right only stage; A/Enter sets
  `apply_display_mode` / `apply_res` consumed in `apply_live` (which then
  sets `gpu.present_dirty`). Restart rows keep the normal row color
  (`v.widgets.inactive.fg_stroke.color`); only the "(restart)" suffix uses
  `weak_text_color` (egui weak = base gamma-multiplied by 0.6).
- **Keep-resolution revert:** applying an exclusive resolution records the previous
  (display_mode, fullscreen_res) in `Revert` with a 10 s deadline. A/Enter
  on any row calls `revert_confirm()` (keep); B/Escape while the countdown
  runs reverts immediately instead of closing (the deadline would never
  fire once the overlay stops redrawing); `close()` clears it. Banner:
  "Keep this resolution? A = keep, reverting in N s" (whole seconds via
  `saturating_duration_since(..).div_ceil(1000)`), drawn at the top of
  `rows()`; deadline checked after the UI pass.
- **Checks:** `PW64_DISPLAY_MODE=borderless PW64_WIN_SHOT=200` → shot at
  monitor size. Applying Exclusive + a resolution via the settings-shot hook
  shows the yellow revert banner. Unit tests: `display_mode_parsing`,
  `resolution_parsing`, `display_mode_round_trips`,
  `fullscreen_resolution_writes_or_removes`,
  `display_choices_skip_exclusive_when_unavailable`,
  `resolution_choices_desktop_first_highest_first`,
  `exclusive_revert_state_machine`.

## Config input maps

- **`SavedSettings` has `keyboard`/`gamepad: Option<BTreeMap<String, String>>`** (filled by
  the Controls page). Apply semantics: `Some(map)` replaces the sub-table wholesale,
  `Some(empty)` removes it (defaults again), `None` leaves it untouched. The apply block
  errors if `[input]` exists but is not a table.
- Unit test: `input_maps_write_or_remove` (replace / remove / neighbors
  `[input] settings_key` + `[input.ble]` survive / non-table `[input]` errors).

## Pages and rows

- `Overlay::rows(&self) -> Vec<Row>` per page
  (`Row { Opt(Opt), Bind(Kind, usize), Reset(Kind), Controls, Back, .. }`,
  `Kind { Keyboard, Gamepad }`); `Overlay.page` resets to Main in
  `new`/`open`. `move_sel`/`confirm`/`change` index `rows()[self.sel]`;
  `sel` is per-page.
- B/Escape (and the Back row) on a sub-page go back to Main with the
  selection resting on the row that opened it; on Main they close the
  overlay. The egui pass keeps the section headers/banners outside
  `rows()` (drawn unconditionally); the draw fn is `ui_rows` (`rows(ui)` was
  taken), with a shared `selectable(..)` tail for hover/click.

## Quit, Esc opens settings, row help

- **Quit:** `Row::Quit` on Main (after Volume). `quit_armed: Option<Instant>` +
  `quit_requested: bool` (`QUIT_ARM` = 3 s): the first confirm arms ("Press again
  to quit", LIGHT_RED), any other confirm or `move_sel` disarms, `quit_check()`
  (per overlay frame) clears an expired arm, a confirm inside the window sets
  `quit_requested` + closes. The window takes `take_quit()` in `redraw` right
  after `close_settings()` (the auto-save has run) and calls `quit(0)`.
- **Esc:** opens settings in `window.rs KeyboardInput` (after the
  first-run block, before the settings-key check), only when
  `!settings::is_open()` and Escape is not bound to a game slot
  (`input::key_mapping(KeyCode::Escape).is_none()`).
- **Row help:** free fn `help(Row) -> &'static str`, drawn weak 14 pt above
  the footer for `rows[self.sel]`; every row has one (unit test). Capture
  rejects: settings key ("{name} opens settings and can't be used. …"), F11
  mirrors it
  ("{name} switches fullscreen and …"); pad timeout "No button pressed.
  Select the row to try again.".
- Player-facing labels (the banner/toast must match the rows):
  `restart_row` + `pending_restart` use "Anti-aliasing"; `msaa_value` helper
  (1 -> "Off", else "4\u{d7}"); Widescreen value "Off (4:3)"; `mode_label`
  (Window / Fullscreen / Exclusive fullscreen); Fps Monitor value
  "Match display ({hz} Hz)" from `monitor_hz` cached in `open()` (0 mHz or no
  monitor -> "Match display"); Filter "Scaling filter": Smooth / Sharp pixels;
  TexFilter Smooth / N64 original.
- **Gotcha:** `bash -lc` from the PowerShell shell opens WSL (wrong
  toolchain): pipe through `Select-Object -Last` instead of `tail`.

## Controls page (live rebinding)

- Controls page rows: 18 `Bind(Keyboard, i)` + `Reset(Keyboard)` +
  11 `Bind(Gamepad, i)` + `Reset(Gamepad)` + Back (32 selectable rows).
  Row values re-read from `input::*_binding_rows()` every frame, so a
  rebind's effect on other slots shows immediately ((unbound), moved
  keys); that is the only warning about cross-slot side effects there is.
- Capture: `Overlay::capture { kind, slot, name, started }`. Keyboard: the
  next `on_key` press binds (`input::bind_key`); Escape cancels; the
  settings key and F11 are rejected with a red line (window.rs consumes
  them in real runs, so this fires on the settings-shot path only).
  Gamepad: `input::start_pad_capture`, `take_captured_button` polled each
  overlay frame, 5 s timeout (`poll_capture` in `render`, before
  `pad_nav`).
- Pitfalls handled: `pad_nav` skipped while capturing and
  `prev_pad = input::overlay_pad()` on capture end (the arming A/Start
  would re-confirm); capture cancelled in `close()` (pad Select closes the
  overlay); `confirm()` ends a running capture before acting (mouse clicks
  bypass `on_key`); `start_capture` ends any previous capture first.
- Save now writes `keyboard`/`gamepad` = the current override maps
  (`input::*_overrides()`); empty map = defaults = the `[input.*]`
  sub-table is dropped (see "Config input maps").
- Tests: `rows_pages_and_controls_row`, `controls_capture_state_machine`
  (arm, reserved-key reject, Escape cancel). Settings-shot check: open
  Controls, Enter arms capture, `KeyP` binds the selected slot.

## Readable panel, pages, auto-save

- The whole overlay zooms with the window height via
  `ui_zoom(height_px, ppp) = (height_px / ppp / 720).clamp(1.0, 2.5)`
  (720 = the design height); applied with `ctx.set_zoom_factor(zoom)`.
  egui's `pixels_per_point` = `zoom_factor * native_pixels_per_point`, so
  the raw-input screen rect is divided by the native ppp only
  (`raw_input_zoom(gpu, native_ppp, time, events)`; the zoom lives in the
  context, not in raw input). Pointer events divide by `self.ppp =
  native_ppp * zoom` (settings + firstrun + their `window.rs` call sites).
  Gotcha: while a zoom change is pending, egui REPLACES the fresh raw
  input's screen rect with the previous pass's rect scaled by the ratio
  (its anti-jitter hack, context.rs ~434). Calling `set_zoom_factor` every
  frame therefore keeps the layout one frame stale forever; call it only
  when the value actually changed (the first frame after a change is off,
  acceptable). `ctx.screen_rect()` is deprecated: use `content_rect()`.
- Panel: `egui::Window` anchored CENTER_CENTER, collapsible/resizable off, min width 460, ScrollArea
  capped at 0.8 of the screen height, `scroll_to_me(Center)` on the
  selection when it moves, black_alpha(140) backdrop on
  `LayerId::background` (dims the paused game), titles Paused /
  Display and graphics / Controls, row text 14 to 16 pt, weak 13 pt
  footer "Up/Down Select, Left/Right Change, Enter / A Confirm, Esc / B
  Back".
- `Page { Main, Graphics, Controls }`; `rows()` per page; Main =
  Resume / Controls / Display and graphics / Volume. `Opt::Save` and
  `Opt::Close` are gone: every close path funnels through `close()`, which
  saves the staged values when they differ from a `SavedSettings` snapshot
  taken in `open()` (`staged_settings()` replaces the old `save()`, and
  `SavedSettings` now derives `PartialEq`). Save error gives a
  "Couldn't save settings: ..." toast. `ResetGraphics` stages the built-in
  defaults (honours `low_end()`). B/Escape/Back on a sub-page returns to
  Main with the selection on the row that opened it (`enter_page`/`back`
  keep `main_row`).
- Toast/firstrun share `raw_input_zoom` and apply the same zoom (their own
  `set_zoom_factor` on-change, pointer divide by ppp * zoom).
- Tests: `ui_zoom_scales_with_window_height` (720/1440/clamp/HiDPI),
  `rows_pages_and_controls_row` (Main 4, Graphics 12, Controls 32; back
  lands on the opening row), `close_with_changes_saves` +
  `close_without_changes_writes_nothing` (temp dir via
  `paths::set_data_dir` BEFORE the first `data_dir()` call, serialized by
  a mutex: `set_data_dir` is once-only per process, so the two tests
  cannot run concurrently in one binary).
- Automated fullscreen shots don't work: unattended borderless/exclusive
  runs never present (`present ticks 0`, overlay never opens); the windowed
  shot verifies centering and zoom (the zoom formula is resolution-driven
  and unit tested).
- egui's default fonts have no arrow glyphs (U+2190-2193 render as boxes): write "Up/Down" etc. in UI text. '›' is fine.
- Controls page layout: `rows()` returns Bind rows in `slot_rank` order
  (A,B,Z,R,START,C_*,stick=9; L/D-pad=10 last, stable within a rank);
  `ordered_slots` maps slot-table indices to that order. Each Bind row =
  "{label}  {value}" (16 pt, two spaces, no colon) + weak 13 pt action
  line from `slot_action` (wording = input.md "What each N64 input does").
  "Controller" section: `input::active_pad()` name as "{name} connected",
  else weak "No controller connected. Plug one in: it works right away."
  Weak rows "Stick: left stick" / "C buttons: right stick (C Up also
  {North name})" use `input::button_name(Button::North, family)`.
  `button_label` routes through `parse_gamepad_button` + `button_name`
  (family from `active_pad()`, Generic fallback), so values show Xbox
  names: "A / B", "LT", "D-pad up". `pad_art::draw(ui, selected_slot())`
  at the top highlights the selected Bind row's slot.
- Controller art (`pad_art.rs`): original hand-authored SVG
  `crates/birdman64/assets/n64_pad.svg` (viewBox 440x360, no logos/trademarks;
  letters are stroked paths because resvg is built with
  `default-features = false`, i.e. no text/fonts). Ids `btn-<slot>` (+
  `ind-STICK_*` arrows, hidden by `.ind`); highlight = append a `<style>`
  (`#btn-X .face` accent fill, `.mark`/`.glyph` white, `#hl-glow` filter)
  and re-rasterize. Textures cached in egui temp memory, MRU of 6, keyed
  (slot, pixel width, accent). Preview: `PW64_PAD_ART_PREVIEW=1 cargo test
  -p birdman64 pad_art` → `tmp/pad_art_*.png`. resvg adds ~1.0 MB to the exe.
- With a ROM loaded (always in the game) `pad_art` draws the game's own
  attract-demo controller instead (`pad_sprites.rs`, mirrors
  `hudDemoController`): UVBT blits read from `pi::rom_bytes()` once
  (body 0x0B 80×77, knob 0x0C, overlays C 0x16 / A,B 0x17 / D-pad 0x18 /
  R 0x19 / L 0x1A / Z burst 0x1B at the hud.c offsets; START reuses the
  A/B overlay at (35,21); stick slots move the knob 4 px like the demo).
  Highlight = game overlay over a blurred accent glow. Composed at native
  res, nearest ×k (k = ceil(px/86), ≤ 8), egui linear to the exact size;
  fitted (near-square) inside the SVG's 440:360 box so layout is
  unchanged. SVG stays the no-ROM fallback (unit tests); resvg could only
  go if that fallback is dropped. Preview: `PW64_PAD_ART_PREVIEW=1 cargo
  test -p birdman64 pad_sprites` → `tmp/pad_rom_*.png` (needs `rom/`).
- Bind-row gotcha: `bind_row` takes the capture *prompt* (`Option<&str>`)
  instead of kind+slot to stay under clippy's 7-arg limit; `is_capturing`
  helper decides. egui scrolls animate (default ScrollAnimation 1000 pts/s,
  0.1-0.3 s): automated shots taken before the animation finishes show an
  intermediate position, not a bug. Run slow (PW64_FPS=30) for shots of
  scrolled views.
- rich_row font bug: it hardcoded `FontId::proportional(14.0)` while every
  other row is 16.0, so "(restart)" suffixes rendered smaller. Fixed to
  16.0. When adding a second text source to a row, share the size
  constant or the suffix drifts.
- Widescreen default: `opts::MONITOR_ASPECT` static; window.rs
  `App::start` seeds it via `set_widescreen_monitor_default(w, h)` from
  `el.primary_monitor()` BEFORE the first `opts::widescreen()` read (which
  sizes the window); pure rule `monitor_widescreen_default`: aspect > 4:3
  → clamp to 21:9, else None (keep 4:3). Headless never sets it (dumps
  stay 640x480). Help text ends "Needs a restart."; the staged default
  shows on the Widescreen row as "16:9 (restart)" on a wide monitor.

## Welcome card, toasts, OLED

- Welcome card: `first_frame_hint` (window.rs) queues `toast.show_card` after the
  first *presented* frame; gated by `PW64_INPUT_SCRIPT` / `PW64_SETTINGS_SHOT`
  and the `[ui] settings_hint_shown` flag in pw64.toml (`config::save_ui_flag`
  writes it once, round-trips in config tests). Verify visually with a
  **windowed** run (PW64_WIN_SHOT only works windowed; PW64_MAX_RETRACES
  forces headless and kills it): hold the flag file out, run with
  `PW64_NO_AUDIO=1 PW64_FPS=30 PW64_WIN_SHOT=10,30,60`, taskkill after the
  shots land. Any `PW64_INPUT_SCRIPT` in the env suppresses the card:
  unset it in the same command (fresh env per shell).
- toast.rs: shared `card_area` helper (Area anchored LEFT_BOTTOM,
  id + index) + `fade()`; cards stack bottom-up, plain toasts offset above
  the card stack (est. heights 42 + 18/line). Card text comes from
  `settings::welcome_card()` (live bindings, 17 pt title / 14 pt lines).
- Controller toasts: input.rs Connected/Disconnected handlers fire when
  `started.elapsed() > 2 s` (skips startup storms); window shows 6 s.
- OLED: config `[oled]` table (`OLED_BRIGHTNESS_STEPS`, manual Default
  brightness 1.0: derived would give 0 = invisible HUD), `PW64_OLED=1`
  preset (drift on, brightness min(0.8)). Applied per-drain in window.rs
  `drain()` via `g.renderer.options.oled`; drift time = fps_timer. HUD dim
  check: white text max 255 (off) vs 204 (on, 0.8).
- Controls page wide mode: panel >=1000 pt → list left (440 pt), drawing
  centred in the rest via `left_to_right(Align::Center)` **wrapped in a
  nested `allocate_ui_with_layout`**: a bare left_to_right layout leaks
  into ScrollArea rows; nested region fixes it.

## Review fixes

- Auto-save writes only fields that differ from the open snapshot (`changed_since`); env/low-end values are never pinned. Fill screen is written whenever widescreen changes.
- A keep-resolution revert re-stages the Display mode / Resolution rows (`restage_display`), else close saved the rejected mode.
- OLED rows were in `rows()` but never drawn; pad B on any sub-page goes back (was: closed on Graphics).
- `[oled] drift = true` no longer caps brightness at 0.8 (only `PW64_OLED=1` does).
- `config::parse` reads `pw64.toml` into a `toml::Table` key by key (`from_table`); a wrong type or invalid value warns, skips only that key and records it in `Config::ignored` (`config::ignored_keys()` feeds a startup toast). Only text that is not valid TOML at all still falls back to defaults. Lenient forms accepted: integer 0/1 for booleans, numeric strings/integers for numbers (`vsync = 1`, `msaa = "4"`). The serde `Raw` struct is gone; new keys read in `from_table` need a test there.

## Window/settings robustness

- Overlay nav reads `input::overlay_pad()` = fixed physical pad state (`gamepad_nav_pad`: South=A confirm, East=B back, Start, d-pad/hat, left stick) merged with BLE, never the rebindable game bindings (rebinding South away used to lock the menu).
- `open` stages MSAA/widescreen/fill from `saved_restart` (what this process last saved; restart pending) else `running`; banner still compares to `running`.
- Reset to defaults stages `opts::widescreen_default()` (monitor aspect) with `widescreen_follow`; staged as `widescreen = ""`, which `config::apply` turns into a key removal.
- CloseRequested runs `close_settings` first (auto-save + unpause); `os::set_pause_hook(park_if_quit)` parks the game thread from the pause loop too.
- `restart_with` releases the instance lock and sets `PW64_RESTARTED=1`; the new copy waits up to 5 s for `instance.lock` (`sys::single_instance`, `File::try_lock` in the data dir, interactive runs only).
- Surface Timeout/Lost and any overlay key/pointer input re-request a redraw (the overlay's own redraw chain dies on a dropped frame, and a paused game sends no Frame events).
- Toasts wired at game start: `paths::startup_notice()`, `config::ignored_keys()`; per frame: `headless::take_toasts()` (save errors), `Gpu::notice`.

## Switch 2 (BLE) row on the Controls page

- `Opt::Ble` row between "Reset gamepad" and Back: "Switch 2 controllers (Bluetooth): On/Off (experimental)". Live like Volume: `change` calls `ble::set_enabled`; staged as `SavedSettings::ble` → `[input.ble] enabled` (default false removed, empty `[input.ble]` dropped).
- Env precedent: locked (disabled row, no toggle, never saved) when `ble::locked_by()` is `PW64_BLE` (env wins) or `PW64_NO_INPUT` (BLE unavailable); the status lines say why. Status lines under the row: `ble_status_lines`, redrawn every overlay frame.
- Disabled rows now also scroll into view when nav selects them (`opt_row`), else the last-row BLE lock was invisible.
- Check: `PW64_SETTINGS_SHOT=200 PW64_WIN_SHOT=1 PW64_SETTINGS_KEYS=Down,Enter,Up,Up[,Enter] PW64_DATA_DIR=<scratch>` (Up from the first Controls row wraps to Back, then BLE). Without `PW64_NO_INPUT`, Enter really starts a BLE scan.
