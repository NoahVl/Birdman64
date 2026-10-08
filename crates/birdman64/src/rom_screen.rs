//! The ROM step in the game's own window, for systems with no file dialog at
//! all (Linux without xdg-desktop-portal, zenity or kdialog: minimal window
//! managers, kiosk setups, CI desktops). `rom_setup::resolve` returns
//! `Ok(None)` there; instead of exiting with instructions on stderr (which a
//! player starting the AppImage from a file manager never sees), the window
//! opens on this screen. It says where a ROM is found automatically, takes a
//! file dropped onto the window, and rescans on "Check again". Once a ROM is
//! installed the window carries on with the setup screen or the game.

use crate::window::Gpu;
use std::path::PathBuf;
use std::time::Instant;
use winit::keyboard::KeyCode;

/// What the window should do after a [`Screen::render`].
#[derive(Debug, PartialEq, Eq)]
pub enum Next {
    Stay,
    /// A ROM is installed (`pi::set_rom`): continue to setup / the game.
    Loaded,
    Quit,
}

pub struct Screen {
    ctx: egui::Context,
    renderer: Option<egui_wgpu::Renderer>,
    events: Vec<egui::Event>,
    started: Instant,
    /// U1 UI zoom in effect (pointer events divide by `ppp * zoom`).
    zoom: f32,
    /// Where the scan finds a ROM by itself (next to the AppImage file, else
    /// next to the program).
    folder: Option<PathBuf>,
    /// Why the last dropped file / rescan didn't give a ROM.
    error: Option<String>,
    dropped: Option<PathBuf>,
    check: bool,
    quit: bool,
    shot: bool,
}

/// The folder the `pi::load_rom(None)` scan covers that a player can put a
/// file into: the AppImage's own folder (the exe dir is the read-only mount),
/// else the exe's folder.
fn rom_folder() -> Option<PathBuf> {
    let beside = |p: PathBuf| p.parent().map(PathBuf::from);
    std::env::var_os("APPIMAGE")
        .map(PathBuf::from)
        .and_then(beside)
        .or_else(|| std::env::current_exe().ok().and_then(beside))
}

impl Screen {
    pub fn new() -> Self {
        Self {
            ctx: egui::Context::default(),
            renderer: None,
            events: Vec::new(),
            started: Instant::now(),
            zoom: 1.0,
            folder: rom_folder(),
            error: None,
            // Dev switch: a file "dropped" before the first frame (checks the
            // drop path without a real drag and drop).
            dropped: std::env::var_os("PW64_DROP_ROM").map(PathBuf::from),
            check: false,
            quit: false,
            shot: false,
        }
    }

    /// A file dropped onto the window (winit `DroppedFile`).
    pub fn dropped(&mut self, path: PathBuf) {
        self.dropped = Some(path);
    }

    /// Enter = Check again, Escape = Quit.
    pub fn on_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Enter | KeyCode::NumpadEnter => self.check = true,
            KeyCode::Escape => self.quit = true,
            _ => {}
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

    /// Acts on a dropped file / "Check again" from the previous frame.
    fn try_load(&mut self) -> bool {
        if let Some(path) = self.dropped.take() {
            match crate::rom_setup::use_rom_file(&path) {
                Ok(p) => {
                    eprintln!("ROM: {}", p.display());
                    return true;
                }
                Err((_, text)) => self.error = Some(text),
            }
        }
        if std::mem::take(&mut self.check) {
            match pw64_platform::pi::load_rom(None) {
                Ok(p) => {
                    eprintln!("ROM: {}", p.display());
                    return true;
                }
                Err(e) => {
                    eprintln!("[pw64] ROM scan: {e:#}");
                    self.error = Some(
                        "No usable ROM was found there yet. Only the US (USA) version works \
                         (.z64, .n64 or .v64, or a .zip containing one)."
                            .into(),
                    );
                }
            }
        }
        false
    }

    /// Draws the screen into `view` (cleared). `shot`: true once, for the
    /// `PW64_WIN_SHOT` dev capture (`tmp/win_setup_rom.png`).
    pub fn render(&mut self, gpu: &Gpu, view: &wgpu::TextureView) -> (Next, bool) {
        if self.try_load() {
            return (Next::Loaded, false);
        }
        let ppp = gpu.window.scale_factor() as f32;
        let zoom = crate::settings::ui_zoom(gpu.config.height, ppp);
        // Only on change, as in the settings overlay (a pending zoom makes
        // egui keep the previous pass's screen rect).
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
        let ctx = self.ctx.clone();
        let out = ctx.run(raw, |ctx| self.ui(ctx));
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
        // A click acts on the next frame; ask for it.
        if self.check || self.quit || self.dropped.is_some() {
            gpu.window.request_redraw();
        }
        let shot = !std::mem::replace(&mut self.shot, true);
        let next = if self.quit { Next::Quit } else { Next::Stay };
        (next, shot)
    }

    fn ui(&mut self, ctx: &egui::Context) {
        let frame = egui::Frame::new()
            .fill(egui::Color32::from_rgb(9, 13, 20))
            .inner_margin(24.0);
        egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.set_max_width(560.0);
                ui.add_space((ui.available_height() * 0.18).max(8.0));
                ui.label(
                    egui::RichText::new("Welcome to Birdman64")
                        .size(26.0)
                        .strong(),
                );
                ui.label(
                    egui::RichText::new("Your game file (ROM) is needed")
                        .size(16.0)
                        .weak(),
                );
                ui.add_space(20.0);
                ui.label(
                    "Birdman64 plays your own copy of Pilotwings 64: the game file (ROM) you \
                     made from your cartridge, a .z64, .n64 or .v64 file, or a .zip \
                     containing one. Only the US version works.",
                );
                ui.add_space(12.0);
                ui.label(
                    egui::RichText::new("Drag the file onto this window.")
                        .size(17.0)
                        .strong(),
                );
                ui.add_space(8.0);
                match &self.folder {
                    Some(dir) => {
                        ui.label("Or copy it into this folder and choose Check again:");
                        ui.label(egui::RichText::new(dir.display().to_string()).monospace());
                    }
                    None => {
                        ui.label(
                            "Or copy it next to the Birdman64 program and choose Check again.",
                        );
                    }
                }
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new(
                        "(This system has no file chooser Birdman64 can open; installing \
                         xdg-desktop-portal, zenity or kdialog brings it back.)",
                    )
                    .weak()
                    .size(12.0),
                );
                if let Some(e) = &self.error {
                    ui.add_space(12.0);
                    ui.label(egui::RichText::new(e).color(egui::Color32::from_rgb(255, 140, 120)));
                }
                ui.add_space(20.0);
                ui.horizontal(|ui| {
                    let (small, wide) = (egui::vec2(90.0, 32.0), egui::vec2(120.0, 32.0));
                    let open = self.folder.is_some();
                    let total = wide.x + small.x + 12.0 + if open { wide.x + 12.0 } else { 0.0 };
                    ui.add_space((ui.available_width() - total).max(0.0) / 2.0);
                    if ui
                        .add_sized(wide, egui::Button::new("Check again"))
                        .clicked()
                    {
                        self.check = true;
                    }
                    if let Some(dir) = &self.folder {
                        ui.add_space(12.0);
                        if ui
                            .add_sized(wide, egui::Button::new("Open folder"))
                            .clicked()
                        {
                            crate::paths::open_in_file_manager(dir);
                        }
                    }
                    ui.add_space(12.0);
                    if ui.add_sized(small, egui::Button::new("Quit")).clicked() {
                        self.quit = true;
                    }
                });
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new("Enter = Check again, Esc = Quit")
                        .weak()
                        .size(12.0),
                );
            });
        });
    }
}
