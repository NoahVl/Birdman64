//! pw64-viewer: renders a terra (UVTR grid + UVCT tiles + placed UVMD
//! models) by building real Fast3D display lists from the ROM assets and
//! running them through `pw64-gfx`.
//!
//! Usage:
//!   pw64-viewer [rom] [--terra N] [--screenshot out.png] [--size WxH]
//!               [--msaa 1|4] [--widescreen] [--filter bilinear|n64] [--fog F] [--near N] [--far F]
//!               [--cam x,y,z,yaw,pitch]
//!
//! `--fog` is `uvGfxSetFogFactor` (the game uses 0.996 with near 1, far 2000).
//!
//! Interactive: WASD move, Q/E down/up, Shift faster, hold the left mouse
//! button to look around, Esc quits, P prints the camera (for `--cam`).

mod dl;
mod scene;

use anyhow::{Context, Result, bail};
use pw64_formats::uvlv;
use pw64_gfx::{Interpreter, RenderOptions, Renderer, TexFilter, Widescreen};
use scene::{Assets, Camera, Environment, Scene, Setup};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use winit::application::ApplicationHandler;
use winit::event::{DeviceEvent, ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowId};

struct Args {
    rom: PathBuf,
    terra: usize,
    screenshot: Option<PathBuf>,
    size: (u32, u32),
    options: RenderOptions,
    env: Environment,
    cam: Option<Camera>,
    /// UVEN environment (`_uvEnvDraw` before the terrain) and UVTP palette.
    env_id: Option<usize>,
    pal: Option<usize>,
    /// `--setup N`: flyable environment N (2..=21), like the game's `envGetCurrentId`
    /// table: picks terra, env and palette together.
    setup: Option<u16>,
    /// Seconds of texture scrolling for `--screenshot` (0 = none).
    time: f32,
    /// Whether `--fog` was passed explicitly.
    fog_set: bool,
}

fn default_rom() -> Result<PathBuf> {
    for e in std::fs::read_dir("rom").context("no rom/ directory; pass the ROM path")? {
        let p = e?.path();
        if matches!(
            p.extension().and_then(|e| e.to_str()),
            Some("z64" | "n64" | "v64")
        ) {
            return Ok(p);
        }
    }
    bail!("no ROM found in rom/")
}

fn parse_args() -> Result<Args> {
    let mut a = Args {
        rom: PathBuf::new(),
        terra: 0,
        screenshot: None,
        size: (1280, 960),
        options: RenderOptions::default(),
        env: Environment {
            fog: 0.0,
            fog_color: [170, 200, 230],
            sky: [120, 170, 230],
            near: 2.0,
            far: 16000.0,
        },
        cam: None,
        env_id: None,
        pal: None,
        setup: None,
        time: 0.0,
        fog_set: false,
    };
    let mut rom = None;
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut val = || it.next().with_context(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--terra" => a.terra = val()?.parse()?,
            "--screenshot" => a.screenshot = Some(val()?.into()),
            "--size" => {
                let v = val()?;
                let (w, h) = v.split_once('x').context("--size WxH")?;
                a.size = (w.parse()?, h.parse()?);
            }
            "--msaa" => a.options.msaa = val()?.parse()?,
            "--widescreen" => a.options.widescreen = Widescreen::Fill,
            "--filter" => {
                a.options.filter = match val()?.as_str() {
                    "bilinear" => TexFilter::Bilinear,
                    "n64" => TexFilter::N64,
                    v => bail!("--filter {v:?}: use bilinear or n64"),
                }
            }
            "--fog" => {
                a.env.fog = val()?.parse()?;
                a.fog_set = true;
            }
            "--near" => a.env.near = val()?.parse()?,
            "--far" => a.env.far = val()?.parse()?,
            "--env" => a.env_id = Some(val()?.parse()?),
            "--pal" => a.pal = Some(val()?.parse()?),
            "--setup" => a.setup = Some(val()?.parse()?),
            "--time" => a.time = val()?.parse()?,
            "--cam" => {
                let v: Vec<f32> = val()?
                    .split(',')
                    .map(str::parse)
                    .collect::<Result<_, _>>()?;
                anyhow::ensure!(v.len() == 5, "--cam x,y,z,yaw,pitch");
                a.cam = Some(Camera {
                    pos: [v[0], v[1], v[2]],
                    yaw: v[3],
                    pitch: v[4],
                    fov_y: 45.0,
                });
            }
            "-h" | "--help" => {
                println!(
                    "usage: pw64-viewer [rom] [--terra N] [--env N] [--pal N] [--setup N] [--screenshot out.png] [--size WxH] [--msaa 1|4] [--widescreen] [--filter bilinear|n64] [--fog F] [--near N] [--far F] [--time T] [--cam x,y,z,yaw,pitch]"
                );
                std::process::exit(0);
            }
            _ if rom.is_none() && !arg.starts_with('-') => rom = Some(PathBuf::from(arg)),
            _ => bail!("unexpected argument {arg:?}"),
        }
    }
    a.rom = match rom {
        Some(r) => r,
        None => default_rom()?,
    };
    Ok(a)
}

fn gpu(
    surface_for: Option<&wgpu::Surface>,
    instance: &wgpu::Instance,
) -> Result<(wgpu::Adapter, wgpu::Device, wgpu::Queue)> {
    let adapter = pw64_gfx::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: surface_for,
        force_fallback_adapter: false,
    }))
    .context("no GPU adapter")?;
    let (device, queue) =
        pw64_gfx::block_on(adapter.request_device(&pw64_gfx::device_descriptor(&adapter)))?;
    Ok((adapter, device, queue))
}

fn screenshot(args: &Args, scene: &mut Scene, out: &Path) -> Result<()> {
    let instance = wgpu::Instance::default();
    let (adapter, device, queue) = gpu(None, &instance)?;
    println!("GPU: {}", adapter.get_info().name);
    let mut renderer = Renderer::new(
        &device,
        &queue,
        wgpu::TextureFormat::Rgba8Unorm,
        args.options,
    );
    let mut interp = Interpreter::new();
    let cam = args.cam.unwrap_or_else(|| scene.default_camera());
    let t = Instant::now();
    let dl = scene.build_frame(&cam, &args.env, args.time);
    let frame = interp.run(&scene.mem, dl | dl::K0);
    let cpu = t.elapsed();
    let rgba = renderer.render_to_rgba(&frame, args.size);
    println!(
        "{} tris, {} draws, {} textures; CPU {:.1} ms; pipelines/shaders/textures {:?}",
        frame.triangle_count(),
        frame.draws.len(),
        frame.textures.len(),
        cpu.as_secs_f64() * 1e3,
        renderer.cache_stats()
    );
    if !interp.unknown_opcodes().is_empty() {
        println!("unimplemented opcodes: {:02X?}", interp.unknown_opcodes());
    }
    let file = std::io::BufWriter::new(std::fs::File::create(out)?);
    let mut enc = png::Encoder::new(file, args.size.0, args.size.1);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()?.write_image_data(&rgba)?;
    println!("wrote {}", out.display());
    Ok(())
}

struct Gpu {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    config: wgpu::SurfaceConfiguration,
    renderer: Renderer,
}

struct App {
    args: Args,
    scene: Scene,
    interp: Interpreter,
    cam: Camera,
    gpu: Option<Gpu>,
    keys: HashSet<KeyCode>,
    looking: bool,
    last: Instant,
    time: f32,
    fps_timer: Instant,
    frames: u32,
}

impl App {
    fn frame(&mut self) {
        let dt = self.last.elapsed().as_secs_f32().min(0.1);
        self.last = Instant::now();
        self.time += dt;
        let speed = if self.keys.contains(&KeyCode::ShiftLeft) {
            1500.0
        } else {
            300.0
        } * dt;
        let f = self.cam.forward();
        let r = [f[1], -f[0], 0.0];
        let rl = (r[0] * r[0] + r[1] * r[1]).sqrt().max(1e-6);
        let mut mv = |d: [f32; 3], s: f32| (0..3).for_each(|i| self.cam.pos[i] += d[i] * s);
        let k = |c| self.keys.contains(&c);
        let (fw, bw, lf, rt, up, dn) = (
            k(KeyCode::KeyW),
            k(KeyCode::KeyS),
            k(KeyCode::KeyA),
            k(KeyCode::KeyD),
            k(KeyCode::KeyE),
            k(KeyCode::KeyQ),
        );
        if fw {
            mv(f, speed);
        }
        if bw {
            mv(f, -speed);
        }
        if rt {
            mv([r[0] / rl, r[1] / rl, 0.0], speed);
        }
        if lf {
            mv([r[0] / rl, r[1] / rl, 0.0], -speed);
        }
        if up {
            mv([0.0, 0.0, 1.0], speed);
        }
        if dn {
            mv([0.0, 0.0, 1.0], -speed);
        }

        let Some(g) = &mut self.gpu else { return };
        let dl = self.scene.build_frame(&self.cam, &self.args.env, self.time);
        let frame = self.interp.run(&self.scene.mem, dl | dl::K0);
        let tex = match g.surface.get_current_texture() {
            Ok(t) => t,
            Err(_) => {
                g.surface.configure(&g.device, &g.config);
                return;
            }
        };
        let view = tex.texture.create_view(&Default::default());
        g.renderer
            .render(&frame, &view, (g.config.width, g.config.height));
        tex.present();
        self.frames += 1;
        if self.fps_timer.elapsed().as_secs_f32() >= 1.0 {
            g.window.set_title(&format!(
                "pw64-viewer — terra {} — {} fps, {} tris, {} draws",
                self.args.terra,
                self.frames,
                frame.triangle_count(),
                frame.draws.len()
            ));
            self.frames = 0;
            self.fps_timer = Instant::now();
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.gpu.is_some() {
            return;
        }
        let window = Arc::new(
            el.create_window(
                Window::default_attributes()
                    .with_title("pw64-viewer")
                    .with_inner_size(winit::dpi::PhysicalSize::new(
                        self.args.size.0,
                        self.args.size.1,
                    )),
            )
            .expect("create window"),
        );
        let instance = wgpu::Instance::default();
        let surface = instance.create_surface(window.clone()).expect("surface");
        let (adapter, device, queue) = gpu(Some(&surface), &instance).expect("GPU");
        let caps = surface.get_capabilities(&adapter);
        // The N64 outputs gamma-space colors: prefer a non-sRGB format.
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let size = window.inner_size();
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            desired_maximum_frame_latency: 2,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
        };
        surface.configure(&device, &config);
        let renderer = Renderer::new(&device, &queue, format, self.args.options);
        println!("GPU: {}, surface {format:?}", adapter.get_info().name);
        self.gpu = Some(Gpu {
            window,
            surface,
            device,
            config,
            renderer,
        });
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::Resized(s) => {
                if let Some(g) = &mut self.gpu {
                    g.config.width = s.width.max(1);
                    g.config.height = s.height.max(1);
                    g.surface.configure(&g.device, &g.config);
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if let PhysicalKey::Code(c) = event.physical_key {
                    if event.state == ElementState::Pressed {
                        match c {
                            KeyCode::Escape => el.exit(),
                            KeyCode::KeyP => {
                                let c = &self.cam;
                                println!(
                                    "--cam {:.1},{:.1},{:.1},{:.3},{:.3}",
                                    c.pos[0], c.pos[1], c.pos[2], c.yaw, c.pitch
                                );
                            }
                            _ => {}
                        }
                        self.keys.insert(c);
                    } else {
                        self.keys.remove(&c);
                    }
                }
            }
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } => self.looking = state == ElementState::Pressed,
            WindowEvent::RedrawRequested => {
                self.frame();
                if let Some(g) = &self.gpu {
                    g.window.request_redraw();
                }
            }
            _ => {}
        }
    }

    fn device_event(&mut self, _: &ActiveEventLoop, _: winit::event::DeviceId, event: DeviceEvent) {
        if let DeviceEvent::MouseMotion { delta } = event
            && self.looking
        {
            self.cam.yaw -= delta.0 as f32 * 0.004;
            self.cam.pitch = (self.cam.pitch - delta.1 as f32 * 0.004).clamp(-1.55, 1.55);
        }
    }
}

fn main() -> Result<()> {
    let mut args = parse_args()?;
    let assets = Assets::load(&args.rom)?;
    // Resolve --setup: the game's env table picks terra + env + palette together.
    if let Some(s) = args.setup {
        let es = uvlv::EnvSetup::for_env(s).with_context(|| format!("env {s} is not flyable"))?;
        args.terra = es.map.terra() as usize;
        args.env_id.get_or_insert(es.env as usize);
        if args.pal.is_none() {
            args.pal = es.palette.map(|p| p as usize);
        }
    }
    // With an environment, take its clear/fog colors and fog factor
    // (`_uvEnvDraw` + `uvGfxSetFogFactor`), unless overridden on the CLI.
    if let Some(e) = args.env_id {
        let uven = &assets.envs[e];
        args.env.fog_color = uven.fog_color[..3].try_into().unwrap();
        args.env.sky = uven.screen[..3].try_into().unwrap();
        if !args.fog_set {
            args.env.fog = uven.fog_factor();
        }
    }
    let mut scene = Scene::new(
        &assets,
        args.terra,
        Setup {
            env: args.env_id,
            palette: args.pal,
        },
    )?;
    println!("{}", scene.stats);
    if let Some(out) = args.screenshot.clone() {
        return screenshot(&args, &mut scene, &out);
    }
    let cam = args.cam.unwrap_or_else(|| scene.default_camera());
    let el = EventLoop::new()?;
    let mut app = App {
        args,
        scene,
        interp: Interpreter::new(),
        cam,
        gpu: None,
        keys: HashSet::new(),
        looking: false,
        last: Instant::now(),
        time: 0.0,
        fps_timer: Instant::now(),
        frames: 0,
    };
    el.run_app(&mut app)?;
    Ok(())
}
