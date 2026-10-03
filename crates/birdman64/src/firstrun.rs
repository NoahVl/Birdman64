//! First-run launcher (feature `first-run`, docs/notes/first-run-build.md T9):
//! the release exe has no game C in it. After the ROM step it loads the game
//! module from the cache, or downloads the pinned decomp and builds the module
//! with the bundled zig (`pw64_cbuild`), then loads it at 0xB0000000.
//!
//! - Headless / scripted runs and `--build-game`: [`ensure_blocking`] (stderr
//!   progress, a clear message + exit 1 on failure, never any UI).
//! - Windowed: [`Screen`] runs the build on a worker thread and draws the
//!   progress / error screen with egui in the game's own window (window.rs:
//!   one winit event loop per process); the game starts in the same window.
//! - `PW64_GAME_DLL=<module>` (dev override) loads that file, no build.
//! - `PW64_ZIG=<zig>` (dev override) instead of the bundled
//!   `toolchain/zig/zig[.exe]` next to the exe.
//!
//! Every failure is a player-facing sentence plus the path of `build.log` in
//! the cache (steps, timings and the full diagnostics).

use crate::paths;
use crate::window::Gpu;
use pw64_cbuild::build::{self, BuildError, Opts};
use pw64_cbuild::fetch::{self, FetchError};
use pw64_cbuild::{Progress, Step, manifest};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use winit::keyboard::KeyCode;

/// A failed setup, ready to show.
#[derive(Clone, Debug)]
pub struct Failure {
    /// Plain explanation for a non-technical player.
    pub message: String,
    /// The log with the details (`<cache>/build.log`).
    pub log: PathBuf,
    /// True when retrying only retries the module LOAD (the player may fix
    /// the cause, e.g. close an overlay or allow Birdman64 in antivirus):
    /// the setup screen then reloads the same file instead of rebuilding.
    /// Build failures always rebuild, so they set false (the default).
    pub reload_only: bool,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}\n\nDetails: {}", self.message, self.log.display())
    }
}

fn log_path() -> PathBuf {
    paths::cache_dir().join("build.log")
}

fn fail(message: impl Into<String>) -> Failure {
    Failure {
        message: message.into(),
        log: log_path(),
        reload_only: false,
    }
}

/// The module to load without building: `PW64_GAME_DLL`, else a cached
/// module for this exe's ABI and the pinned decomp commit.
pub fn cached() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("PW64_GAME_DLL") {
        return Some(PathBuf::from(p));
    }
    let manifest = manifest::parse(manifest::DECOMP_MANIFEST).ok()?;
    build::find_cached(
        &paths::cache_dir(),
        pw64_game::dylib::DLL_ABI,
        &build::flags_hash_host(),
        &manifest.commit,
    )
    .map(|m| m.library)
}

/// Why a module failed to load, from its error text (unit-tested): how the
/// setup screen should talk about it and whether Retry may just reload.
#[derive(Debug, PartialEq, Eq)]
enum LoadProblem {
    /// Another program is mapped at the module's fixed base.
    RangeTaken,
    /// The file is missing or unreadable (antivirus quarantine is common).
    Blocked,
    /// Anything else (wrong ABI, missing symbol): a rebuild is needed.
    Other,
}

fn load_problem(e: &str) -> LoadProblem {
    let e = e.to_lowercase();
    if e.contains("address range taken") {
        LoadProblem::RangeTaken
    } else if e.contains("could not load")
        && (e.contains("cannot find")
            || e.contains("not be found")
            || e.contains("no such file")
            || e.contains("access is denied")
            || e.contains("permission denied"))
    {
        LoadProblem::Blocked
    } else {
        LoadProblem::Other
    }
}

/// Loads the module and checks the address-space invariants (memmap).
pub fn load(path: &Path) -> Result<(), Failure> {
    pw64_game::dylib::load(path).map_err(|e| load_failure(path, &e))?;
    eprintln!("[setup] game module: {}", path.display());
    eprintln!("{}", pw64_game::memmap::check_low_4gb());
    Ok(())
}

/// A failed load as a [`Failure`] (F6): the player-facing wording depends on
/// why it failed, and when the player can fix it, Retry must reload the same
/// file instead of rebuilding it.
fn load_failure(path: &Path, e: &str) -> Failure {
    let mut f = match load_problem(e) {
        LoadProblem::RangeTaken => fail(format!(
            "Birdman64 couldn't start because another program is using memory it \
             needs. Please close any overlay or screen-recording software (such \
             as Discord's overlay, MSI Afterburner or RTSS) and press Retry.\n\n({e})"
        )),
        LoadProblem::Blocked => fail(format!(
            "Birdman64 couldn't read its game file. Your antivirus software may \
             have blocked or quarantined it. Please allow Birdman64 in your \
             antivirus software and press Retry.\n\n({e})"
        )),
        LoadProblem::Other => fail(format!(
            "Birdman64 couldn't start its game module. If another program \
             (an overlay or screen recorder) is running, close it and try again; \
             otherwise delete the folder {} to rebuild it.\n\n({e})",
            path.parent().unwrap_or(path).display()
        )),
    };
    // Retry reloads the same file whenever the player can fix the cause;
    // only the rebuild case (Other) needs the full worker again.
    f.reload_only = load_problem(e) != LoadProblem::Other;
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_error_classification() {
        // Windows: quarantine / missing file.
        assert_eq!(
            load_problem(
                "could not load C:\\cache\\x.dll: The specified module could not be found. (0x7e)"
            ),
            LoadProblem::Blocked
        );
        assert_eq!(
            load_problem("could not load C:\\cache\\x.dll: Access is denied. (0x5)"),
            LoadProblem::Blocked
        );
        // Linux dlopen variants.
        assert_eq!(
            load_problem(
                "could not load /c/x.so: libpw64game.so: cannot open shared object file: No such file or directory"
            ),
            LoadProblem::Blocked
        );
        assert_eq!(
            load_problem("could not load /c/x.so: Permission denied"),
            LoadProblem::Blocked
        );
        // The fixed base was taken.
        assert_eq!(
            load_problem("x.dll loaded at 0xc0000000, not 0xb0000000 (address range taken)"),
            LoadProblem::RangeTaken
        );
        // Everything else is Other (a rebuild is the only fix).
        assert_eq!(
            load_problem("x.dll: missing symbol pw64_dll_abi; delete it to rebuild the game"),
            LoadProblem::Other
        );
        assert_eq!(
            load_problem("could not load x.dll: something completely different"),
            LoadProblem::Other
        );
    }
}

/// Headless, scripted and `--build-game` runs: the cached module or a build
/// with progress on stderr, then load it. Prints the reason and exits 1 on
/// failure (no dialog: nobody may be watching).
pub fn ensure_blocking() -> PathBuf {
    let path = match cached() {
        Some(p) => p,
        None => {
            let mut last = None;
            match build_module(|p| {
                if last != Some(p.step) {
                    last = Some(p.step);
                    // Called from compile workers: never panic on a closed
                    // stderr (eprintln! would).
                    let _ = writeln!(std::io::stderr(), "[setup] {}", step_label(p.step));
                }
            }) {
                Ok(p) => p,
                Err(f) => exit_failure(&f),
            }
        }
    };
    if let Err(f) = load(&path) {
        exit_failure(&f);
    }
    path
}

fn exit_failure(f: &Failure) -> ! {
    eprintln!("error: game setup failed: {f}");
    std::process::exit(1);
}

/// What the progress screen says for each step.
fn step_label(step: Step) -> &'static str {
    match step {
        Step::FindZig => "Checking the bundled compiler",
        Step::Download => "Downloading the game's source code (about 1 MB)",
        Step::VerifyExtract => "Checking the downloaded files",
        Step::ApplyPatches => "Preparing the source code",
        Step::Compile => "Building the game",
        Step::Link => "Putting the game together",
        Step::Load => "Starting",
    }
}

/// The bundled compiler: `PW64_ZIG`, else `toolchain/zig/zig[.exe]` next to
/// the exe (release zip), else (Linux AppImage) `../lib/birdman64/toolchain/
/// zig/zig`. `Err` = the path the player should look for.
fn find_zig() -> Result<PathBuf, PathBuf> {
    if let Some(p) = std::env::var_os("PW64_ZIG") {
        let p = PathBuf::from(p);
        return if p.is_file() { Ok(p) } else { Err(p) };
    }
    let exe = if cfg!(windows) { "zig.exe" } else { "zig" };
    let dir = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(Path::to_path_buf))
        .unwrap_or_default();
    let candidates = [
        dir.join("toolchain").join("zig").join(exe),
        dir.join("../lib/birdman64/toolchain/zig").join(exe),
    ];
    let n = if cfg!(windows) { 1 } else { 2 };
    candidates[..n]
        .iter()
        .find(|p| p.is_file())
        .cloned()
        .ok_or_else(|| candidates[0].clone())
}

/// Download (or reuse) the decomp, build the module, delete older modules.
/// Writes `<cache>/build.log` (steps, timings, the full error). Blocking: the
/// window runs it on a worker thread.
pub fn build_module(mut progress: impl FnMut(Progress) + Send) -> Result<PathBuf, Failure> {
    let cache = paths::cache_dir();
    let no_space = |e: &dyn std::fmt::Display| {
        fail(format!(
            "Birdman64 couldn't write its files to {}. Please check that this drive \
             has about 200 MB free and that the folder isn't read-only, then try again.\n\n({e})",
            cache.display()
        ))
    };
    std::fs::create_dir_all(&cache).map_err(|e| no_space(&e))?;
    let mut log = std::fs::File::create(log_path()).map_err(|e| no_space(&e))?;
    let t0 = Instant::now();
    let _ = writeln!(
        log,
        "{}\nabi {}",
        crate::version_line(),
        pw64_game::dylib::DLL_ABI
    );
    let result = run_build(&cache, &mut log, t0, &mut progress);
    match &result {
        Ok(p) => {
            let _ = writeln!(
                log,
                "{:6.1}s done: {}",
                t0.elapsed().as_secs_f32(),
                p.display()
            );
        }
        Err((f, detail)) => {
            let _ = writeln!(
                log,
                "{:6.1}s FAILED: {}\n\n{detail}",
                t0.elapsed().as_secs_f32(),
                f.message
            );
        }
    }
    eprintln!(
        "[setup] {:.1}s, log: {}",
        t0.elapsed().as_secs_f32(),
        log_path().display()
    );
    result.map_err(|(f, detail)| {
        eprintln!("[setup] {detail}");
        f
    })
}

/// [`build_module`]'s steps; errors carry the full technical detail for the
/// log next to the player message.
fn run_build(
    cache: &Path,
    log: &mut std::fs::File,
    t0: Instant,
    progress: &mut (impl FnMut(Progress) + Send),
) -> Result<PathBuf, (Failure, String)> {
    let mut last = None;
    let mut max = 0.0f32;
    let mut report = |p: Progress| {
        // Monotonic: the builder restarts at FindZig (0%) after the fetch,
        // and download retries report 5% again; the bar never goes back.
        if p.fraction < max {
            return;
        }
        max = p.fraction;
        if last != Some(p.step) {
            last = Some(p.step);
            let _ = writeln!(log, "{:6.1}s {:?}", t0.elapsed().as_secs_f32(), p.step);
        }
        progress(p);
    };
    report(Progress {
        step: Step::FindZig,
        fraction: 0.0,
    });
    let zig = find_zig().map_err(|p| {
        (
            fail(format!(
                "The bundled compiler ({}) is missing or was blocked, often by antivirus \
                 software. Extract the Birdman64 zip again, or allow Birdman64 in your \
                 antivirus, then try again.",
                p.display()
            )),
            format!("zig not found at {}", p.display()),
        )
    })?;
    let manifest = manifest::parse(manifest::DECOMP_MANIFEST)
        .map_err(|e| (fail("This copy of Birdman64 is damaged."), e))?;
    let tree = fetch::decomp_tree(&manifest, cache, &mut report)
        .map_err(|e| (fetch_failure(&e, cache), e.to_string()))?;
    let opts = Opts {
        zig,
        decomp_dir: tree,
        out_root: cache.to_path_buf(),
        abi: pw64_game::dylib::DLL_ABI.to_string(),
        key_extra: pw64_game::dylib::DLL_ABI.to_string(),
        commit: manifest.commit.clone(),
    };
    let module = build::build_module(&opts, &mut report)
        .map_err(|e| (build_failure(&e, cache), e.to_string()))?;
    build::remove_other_keys(cache, &module.dir);
    report(Progress {
        step: Step::Load,
        fraction: 1.0,
    });
    Ok(module.library)
}

fn fetch_failure(e: &FetchError, cache: &Path) -> Failure {
    match e {
        FetchError::Network(_) => fail(
            "Birdman64 couldn't download the game's source code from GitHub. \
             Please check your internet connection and try again. If you use a \
             proxy, set the HTTPS_PROXY environment variable.",
        ),
        FetchError::Http { code } => fail(format!(
            "GitHub refused the download (HTTP {code}). Please report this at \
             https://github.com/NoahVl/Birdman64/issues and attach the log file."
        )),
        FetchError::HashMismatch { path, .. } => fail(format!(
            "The downloaded source code doesn't match the expected version ({path}). \
             Please try again; if it keeps happening, report it with the log file."
        )),
        FetchError::Archive(_) => fail(
            "The downloaded source code was incomplete or damaged. Please try again; \
             if it keeps happening, report it with the log file.",
        ),
        FetchError::Io(d) => io_failure(d, cache),
    }
}

fn build_failure(e: &BuildError, cache: &Path) -> Failure {
    match e {
        BuildError::Toolchain(_) => fail(
            "The bundled compiler (toolchain/zig) couldn't run: it is missing or was \
             blocked, often by antivirus software. Extract the Birdman64 zip again, or \
             allow Birdman64 in your antivirus, then try again.",
        ),
        BuildError::Io(d) => io_failure(d, cache),
        BuildError::Compile { .. }
        | BuildError::Link(_)
        | BuildError::Zig(_)
        | BuildError::Patches(_)
        | BuildError::Kit(_) => fail(
            "Building the game failed. Please report this and attach the log file. \
             (If antivirus software removed files while the game was being built, \
             allowing Birdman64 and trying again may help.)",
        ),
    }
}

fn io_failure(detail: &str, cache: &Path) -> Failure {
    fail(format!(
        "Birdman64 couldn't write its files to {}. Please check that this drive has \
         about 200 MB free and that the folder isn't read-only (antivirus software can \
         also block it), then try again.\n\n({detail})",
        cache.display()
    ))
}

/// Shared between the build worker and the window thread.
struct Status {
    progress: Progress,
    done: Option<Result<PathBuf, Failure>>,
}

/// The windowed setup screen: progress while the worker builds, then either
/// the module path for the window to load ([`Screen::poll`]) or an error with
/// Retry / Quit.
pub struct Screen {
    status: Arc<Mutex<Status>>,
    /// Wakes the event loop (progress / done).
    wake: Arc<dyn Fn() + Send + Sync>,
    failed: Option<Failure>,
    /// The module whose load failed reload-only (`Failure::reload_only`):
    /// Retry reloads this file instead of rebuilding.
    reload: Option<PathBuf>,
    /// The module path last handed to the window by [`Self::poll`] (so a
    /// reload-only retry can ask for it again).
    last_module: Option<PathBuf>,
    ctx: egui::Context,
    renderer: Option<egui_wgpu::Renderer>,
    events: Vec<egui::Event>,
    started: Instant,
    /// U1 UI zoom in effect (updated each `render`; pointer events divide
    /// the cursor by `ppp * zoom`).
    zoom: f32,
    /// Tag of the last `PW64_WIN_SHOT` capture (one per step, dev only).
    shot: Option<String>,
    /// Pad on the previous `render` (error screen pad nav, edge-triggered
    /// like the settings overlay): seeded from the current pad when the
    /// failure is shown, so a held A from the ROM dialog doesn't retry.
    prev_pad: crate::input::Pad,
    quit: bool,
    retry: bool,
}

impl Screen {
    /// Starts the build worker; `wake` is called from it on every progress
    /// report and when it finishes.
    pub fn start(wake: impl Fn() + Send + Sync + 'static) -> Self {
        let mut s = Self {
            status: Arc::new(Mutex::new(Status {
                progress: Progress {
                    step: Step::FindZig,
                    fraction: 0.0,
                },
                done: None,
            })),
            wake: Arc::new(wake),
            failed: None,
            reload: None,
            last_module: None,
            ctx: egui::Context::default(),
            renderer: None,
            events: Vec::new(),
            started: Instant::now(),
            zoom: 1.0,
            shot: None,
            prev_pad: crate::input::Pad::default(),
            quit: false,
            retry: false,
        };
        s.spawn_worker();
        s
    }

    fn spawn_worker(&mut self) {
        self.failed = None;
        self.reload = None;
        self.last_module = None;
        *self.status.lock().unwrap() = Status {
            progress: Progress {
                step: Step::FindZig,
                fraction: 0.0,
            },
            done: None,
        };
        let (status, wake) = (self.status.clone(), self.wake.clone());
        std::thread::spawn(move || {
            let st = status.clone();
            let w = wake.clone();
            let r = build_module(move |p| {
                st.lock().unwrap().progress = p;
                w();
            });
            status.lock().unwrap().done = Some(r);
            wake();
        });
    }

    /// The finished module (once), for the window to load. A build failure
    /// switches to the error screen; a reload-only retry (from [`Self::render`])
    /// returns the failed module again so the window re-runs `load`.
    pub fn poll(&mut self) -> Option<PathBuf> {
        if let Some(p) = self.reload.take() {
            return Some(p);
        }
        let done = self.status.lock().unwrap().done.take()?;
        match done {
            Ok(p) => {
                self.last_module = Some(p.clone());
                Some(p)
            }
            Err(f) => {
                self.fail(f);
                None
            }
        }
    }

    /// Show `f` (build or load failure).
    pub fn fail(&mut self, f: Failure) {
        eprintln!("error: game setup failed: {f}");
        self.failed = Some(f);
        // Seed the pad edge from what is pressed now, so a button held from
        // the ROM dialog doesn't act immediately (same as settings.rs).
        self.prev_pad = crate::input::overlay_pad();
    }

    /// Enter = Retry, Escape = Quit (error screen only).
    pub fn on_key(&mut self, code: KeyCode) {
        if self.failed.is_some() {
            match code {
                KeyCode::Enter | KeyCode::NumpadEnter => self.retry = true,
                KeyCode::Escape => self.quit = true,
                _ => {}
            }
        }
    }

    pub fn pointer_moved(&mut self, x: f32, y: f32, ppp: f32) {
        let p = ppp * self.zoom;
        self.events
            .push(egui::Event::PointerMoved(egui::pos2(x / p, y / p)));
    }

    pub fn pointer_button(&mut self, x: f32, y: f32, ppp: f32, pressed: bool) {
        let p = ppp * self.zoom;
        self.events.push(egui::Event::PointerButton {
            pos: egui::pos2(x / p, y / p),
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        });
    }

    /// Draws the screen into `view` (cleared). Returns false when the player
    /// chose Quit. `shot`: a new capture tag (`PW64_WIN_SHOT` dev captures:
    /// one per step, mid-compile, and the error screen).
    pub fn render(&mut self, gpu: &Gpu, view: &wgpu::TextureView) -> (bool, Option<String>) {
        // Pad nav on the error screen (A = Retry, B = Quit), edge-triggered
        // exactly like the settings overlay's `prev_pad`.
        if self.failed.is_some() {
            let pad = crate::input::overlay_pad();
            let prev = std::mem::replace(&mut self.prev_pad, pad);
            let edge = |bit: u16| pad.button & bit != 0 && prev.button & bit == 0;
            if edge(crate::input::CONT_A) {
                self.retry = true;
            }
            if edge(crate::input::CONT_B) {
                self.quit = true;
            }
        }
        if std::mem::take(&mut self.retry) {
            if self.failed.as_ref().is_some_and(|f| f.reload_only)
                && let Some(p) = self.last_module.clone()
            {
                // F6: the player may have fixed the cause (antivirus, overlay):
                // re-run `load` on the same file, not the whole build.
                self.reload = Some(p);
                (self.wake)();
            } else {
                self.spawn_worker();
            }
        }
        let ppp = gpu.window.scale_factor() as f32;
        let zoom = crate::settings::ui_zoom(gpu.config.height, ppp);
        // Same pattern as the settings overlay: only on change, because a
        // pending zoom makes egui keep the previous pass's screen rect.
        if (self.ctx.zoom_factor() - zoom).abs() > f32::EPSILON {
            self.ctx.set_zoom_factor(zoom);
        }
        self.zoom = zoom;
        let raw = crate::settings::raw_input_zoom(
            gpu,
            ppp,
            self.started.elapsed().as_secs_f64(),
            std::mem::take(&mut self.events),
        );
        let progress = self.status.lock().unwrap().progress;
        let ctx = self.ctx.clone();
        let out = ctx.run(raw, |ctx| self.ui(ctx, progress));
        let bg = wgpu::Color {
            r: 0.035,
            g: 0.05,
            b: 0.08,
            a: 1.0,
        };
        crate::settings::paint(
            gpu,
            &self.ctx,
            &mut self.renderer,
            out,
            view,
            wgpu::LoadOp::Clear(bg),
        );
        let tag = match &self.failed {
            Some(_) => Some("error".to_string()),
            // Compile: capture once it is well under way (a visible bar).
            None if progress.step == Step::Compile && progress.fraction < 0.6 => None,
            None => Some(format!("{:?}", progress.step)),
        };
        let new_tag = tag.filter(|t| self.shot.as_ref() != Some(t));
        if new_tag.is_some() {
            self.shot.clone_from(&new_tag);
        }
        (!self.quit, new_tag)
    }

    fn ui(&mut self, ctx: &egui::Context, p: Progress) {
        let frame = egui::Frame::new()
            .fill(egui::Color32::from_rgb(9, 13, 20))
            .inner_margin(24.0);
        egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.set_max_width(560.0);
                ui.add_space((ui.available_height() * 0.22).max(8.0));
                ui.label(
                    egui::RichText::new("Preparing Birdman64 for your PC")
                        .size(26.0)
                        .strong(),
                );
                ui.label(egui::RichText::new("One-time setup").size(16.0).weak());
                ui.add_space(20.0);
                if let Some(f) = self.failed.clone() {
                    self.error_ui(ui, &f);
                    return;
                }
                ui.label(
                    "Birdman64 downloads the game's open-source code from GitHub (about \
                     1 MB) and builds it on your PC. Your ROM never leaves your computer. \
                     This usually takes under a minute; next time the game starts right \
                     away.",
                );
                ui.add_space(24.0);
                ui.label(egui::RichText::new(step_label(p.step)).size(17.0));
                ui.add_space(8.0);
                ui.add(
                    egui::ProgressBar::new(p.fraction)
                        .show_percentage()
                        .desired_height(22.0)
                        .animate(true),
                );
            });
        });
    }

    fn error_ui(&mut self, ui: &mut egui::Ui, f: &Failure) {
        ui.label(
            egui::RichText::new("Setup couldn't finish")
                .size(19.0)
                .color(egui::Color32::from_rgb(255, 140, 120)),
        );
        ui.add_space(10.0);
        ui.label(&f.message);
        ui.add_space(10.0);
        ui.label(egui::RichText::new(format!("Details: {}", f.log.display())).weak());
        ui.add_space(20.0);
        ui.horizontal(|ui| {
            // Center the three buttons ("Open log folder" needs more width).
            // Quit last, as in most dialogs.
            let (small, wide) = (egui::vec2(90.0, 32.0), egui::vec2(120.0, 32.0));
            let total = small.x + wide.x + small.x + 24.0;
            ui.add_space((ui.available_width() - total).max(0.0) / 2.0);
            if ui.add_sized(small, egui::Button::new("Retry")).clicked() {
                self.retry = true;
            }
            ui.add_space(12.0);
            if ui
                .add_sized(wide, egui::Button::new("Open log folder"))
                .clicked()
                && let Some(dir) = f.log.parent()
            {
                crate::paths::open_in_file_manager(dir);
            }
            ui.add_space(12.0);
            if ui.add_sized(small, egui::Button::new("Quit")).clicked() {
                self.quit = true;
            }
        });
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new("Enter or A = Retry, Esc or B = Quit")
                .weak()
                .size(12.0),
        );
    }
}
