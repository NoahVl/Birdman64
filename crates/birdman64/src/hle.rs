//! HLE gfx + VI present (native-build.md §6 task 7).
//!
//! - Gfx tasks: the display list is interpreted by `pw64-gfx` directly over
//!   the C address space (RDRAM window at 0x80000000, thread stacks above it,
//!   exe image at 0xC0000000 — all live at their raw host addresses). The
//!   resulting `Frame` is drawn onto its framebuffer's persistent GPU target
//!   (the task's `framebuffer`; `pw64_gfx` `renderer/fb.rs`): like on the
//!   N64, a task only changes what it draws, and `uvCopyFrameBuf` copies
//!   targets ([`Hle::fb_copy`], graphics.c.patch).
//! - The framebuffer operations go, in game order, to the [`FrameSink`] (the
//!   window replays them on its renderer) with the framebuffer each swap
//!   latch (VI retrace, or the present tick with `PW64_FPS`) newly shows;
//!   `PW64_DUMP_FRAMES` reads back the shown target from the dump renderer
//!   at VI retraces. The RDRAM framebuffer itself is only read (320×240
//!   RGBA5551, `OS_VI_NTSC_LAN1`) for the first-present milestone and
//!   `PW64_FB_SHOTS`.
//! - Not honoured (CPU access to framebuffer RDRAM): `gGfxCallback`
//!   (snow.c `snowDraw` plots snow pixels into the finished fb on the CPU),
//!   filesystem.c's GZIP scratch use of the back buffer (garbage the game
//!   overwrites anyway). See renderer.md "Framebuffer persistence".
//!
//! Env: `PW64_DUMP_FRAMES=<n>|<r1,r2,..>` render the displayed frame every n
//! retraces (or at the listed retraces) → `tmp/frame_<retrace>.png`;
//! `PW64_DUMP_HEIGHT=<px>` dump height (default 480, width even at the
//! output aspect: 4:3 or `PW64_WIDESCREEN`).
//! `PW64_GFX_SHOTS` / `PW64_FB_SHOTS` (first-N gfx / RDRAM fb PNGs, default
//! 0); `PW64_STOP_MILESTONE=1` exit as soon as the first gfx task renders or
//! the first non-black framebuffer is presented; `PW64_DUMP_TEX` every
//! decoded texture → `tmp/tex/<hash>.png`; `PW64_TEX_PACKS=<dir>` replace
//! textures from a pack (`packs.rs`); `PW64_NO_GPU` never render.
//! `PW64_DUMP_DL=1` also writes `tmp/frame_<retrace>_dl.txt` per dumped
//! frame (draw list + full command/vertex trace); with
//! `PW64_DUMP_PIXEL=x,y` (dump-PNG coords) it lists which draws change that
//! pixel (renders every draw prefix: slow).

use crate::opts;
use pw64_game::memmap::hle_readable;
use pw64_gfx::{FbOp, Frame, Interpreter, Memory, RenderOptions, Renderer, VecMemory, Widescreen};
use pw64_platform::headless::GfxTaskInfo;
use pw64_platform::os::vi::Retrace;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// `PW64_PROFILE_RETRACES=1`: splits of the per-retrace cost measured by the
/// OS core (temporary instrumentation, renderer.md "144 Hz profiling") —
/// display-list interpretation (`on_gfx_task`) and the dump renderer's GPU
/// work (each task's framebuffer draw + the dump readback in `present`; the
/// PNG encode on top is host, not present cost).
fn profile_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("PW64_PROFILE_RETRACES").is_some())
}

/// Accumulated wall time and call count of one profiled step.
struct Split {
    ns: AtomicU64,
    calls: AtomicU64,
}

impl Split {
    const fn new() -> Self {
        Self {
            ns: AtomicU64::new(0),
            calls: AtomicU64::new(0),
        }
    }

    /// Runs `f`, timed when profiling is on.
    fn time<T>(&self, f: impl FnOnce() -> T) -> T {
        if !profile_on() {
            return f();
        }
        let t0 = std::time::Instant::now();
        let out = f();
        self.calls.fetch_add(1, Relaxed);
        self.ns.fetch_add(t0.elapsed().as_nanos() as u64, Relaxed);
        out
    }

    /// "<total> ms / <calls> <what> (<per call> ms/<unit>)".
    fn report(&self, what: &str, unit: &str) -> String {
        let (ms, calls) = (self.ns.load(Relaxed) as f64 / 1e6, self.calls.load(Relaxed));
        let per = ms / calls.max(1) as f64;
        format!("{ms:.1} ms / {calls} {what} ({per:.3} ms/{unit})")
    }
}

static INTERP: Split = Split::new();
static DUMP_RENDER: Split = Split::new();

/// The pw64-side profile splits (empty when `PW64_PROFILE_RETRACES` is unset).
pub fn profile_stats() -> String {
    if !profile_on() {
        return String::new();
    }
    format!(
        "[profile] DL interpretation {}; GPU dump render {}",
        INTERP.report("gfx tasks", "task"),
        DUMP_RENDER.report("gfx tasks + dumps", "call")
    )
}

/// The C address space as display-list words see it (native-build.md §7):
/// words hold full host pointers (window / stacks / image) or `K0` /
/// `osVirtualToPhysical` values, which must stay distinguishable — the
/// classic 29-bit segment resolution would alias the image onto the window.
struct GfxMemory;

impl GfxMemory {
    /// Maps a raw Gfx word to its host address: host pointers pass through,
    /// everything else (physical / K0) gets bit 31 back. A host-looking word
    /// that points at nothing readable (dead stack slot, guard page, the
    /// module/exe gap) also gets bit 31 added, which lands below the RDRAM
    /// window, so [`Self::mapped`] rejects it either way.
    fn host(raw: u32) -> usize {
        let h = raw as usize;
        if hle_readable(h, 1) {
            return h;
        }
        raw.wrapping_add(0x8000_0000) as usize
    }

    /// Only committed, readable C memory (`memmap::hle_readable`): the RDRAM
    /// window, live thread stacks, the exe / game module image.
    fn mapped(addr: usize, len: usize) -> bool {
        hle_readable(addr, len)
    }
}

impl Memory for GfxMemory {
    /// Words are host pointers or physical/K0 forms — resolve to host.
    fn map(&self, raw: u32) -> Option<u32> {
        Some(Self::host(raw) as u32)
    }

    /// Display-list words are host u32s written by the C macros.
    fn read_u32(&self, addr: u32) -> u32 {
        let mut b = [0u8; 4];
        Self::copy(addr as usize, &mut b);
        u32::from_le_bytes(b)
    }

    /// Structural data (Mtx/Vtx/lights) lives in RAM as host (LE) values;
    /// the interpreter consumes the RSP's big-endian byte stream, so every
    /// u16 is swapped back (`PW64_SWAP` did the same in reverse).
    fn read_bytes(&self, addr: u32, out: &mut [u8]) {
        Self::copy(addr as usize, out);
        for pair in out.as_chunks_mut::<2>().0 {
            pair.swap(0, 1);
        }
    }

    /// Texel data was copied raw (BE) from the ROM; no swap.
    fn read_raw(&self, addr: u32, out: &mut [u8]) {
        Self::copy(addr as usize, out);
    }
}

impl GfxMemory {
    /// Bounds-checked host read of the C address space.
    fn copy(addr: usize, out: &mut [u8]) {
        if out.is_empty() {
            return;
        }
        if Self::mapped(addr, out.len()) {
            // SAFETY: the C address space (window, stacks, image) is mapped
            // host memory; `mapped` bounds-checked the whole read.
            unsafe {
                std::ptr::copy_nonoverlapping(addr as *const u8, out.as_mut_ptr(), out.len())
            };
        } else {
            use std::sync::atomic::{AtomicBool, Ordering};
            static WARNED: AtomicBool = AtomicBool::new(false);
            if !WARNED.swap(true, Ordering::Relaxed) {
                eprintln!(
                    "[hle] display list read unmapped {addr:#x} (+{:#x}); zeros",
                    out.len()
                );
            }
            out.fill(0);
        }
    }
}

/// `SCREEN_WIDTH`/`SCREEN_HEIGHT` (kernel/uv_graphics.h): `OS_VI_NTSC_LAN1`.
const FB_W: usize = 320;
const FB_H: usize = 240;
/// Default PNG dump height (`PW64_DUMP_HEIGHT`): 2x the N64 frame.
const GFX_H: u32 = 480;

/// Receives, at a swap latch (retrace or present tick), the framebuffer
/// operations since the last call (in game order: replay them all, none may
/// be dropped) and the
/// framebuffer this latch newly shows (`None`: keep showing the last).
pub type FrameSink = Box<dyn FnMut(Vec<FbOp>, Option<u32>)>;

/// `PW64_DUMP_FRAMES`: every n retraces, or an explicit retrace list.
enum DumpFrames {
    Never,
    Every(u64),
    At(Vec<u64>),
}

impl DumpFrames {
    fn from_env() -> Self {
        let Ok(v) = std::env::var("PW64_DUMP_FRAMES") else {
            return Self::Never;
        };
        let nums: Vec<u64> = v.split(',').filter_map(|s| s.trim().parse().ok()).collect();
        match (v.contains(','), nums.as_slice()) {
            (_, []) | (false, [0]) => Self::Never,
            (false, [n]) => Self::Every(*n),
            _ => Self::At(nums),
        }
    }

    fn due(&self, retrace: u64) -> bool {
        match self {
            Self::Never => false,
            Self::Every(n) => retrace.is_multiple_of(*n),
            Self::At(list) => list.contains(&retrace),
        }
    }
}

pub struct Hle {
    /// Offscreen renderer for PNG dumps only (the window has its own).
    gpu: Option<Renderer>,
    /// PNG dump size: width even at the output aspect, `PW64_DUMP_HEIGHT` high.
    dump_size: (u32, u32),
    interp: Interpreter,
    /// Snow pixel draws ([`Self::fb_pixels`], `pw64_fb_pixels`) run on
    /// their own interpreter so the game's display-list state is untouched.
    snow_interp: Interpreter,
    mem: GfxMemory,
    sink: Option<FrameSink>,
    /// Framebuffer operations not yet handed to the sink.
    ops: Vec<FbOp>,
    /// Framebuffers drawn or copied into since they were last latched.
    dirty: Vec<u32>,
    /// The last task per framebuffer (+ its display-list trace when
    /// `PW64_DUMP_DL` is set), for the `_dl.txt` dumps.
    last: Vec<(u32, Arc<Frame>, String)>,
    /// The framebuffer the VI currently shows.
    shown: Option<u32>,
    dump_frames: DumpFrames,
    gfx_shots: u32,
    fb_shots: u32,
    gfx_shot: u32,
    fb_shot: u32,
    gfx_tasks: u64,
    first_gfx: bool,
    first_present: bool,
    first_real_present: bool,
    last_fb: Vec<u8>,
    stop_at_milestone: bool,
}

/// Physical form of a framebuffer address (gfx task `cimg` vs VI origin).
fn fb_key(addr: usize) -> u32 {
    (addr & 0x1FFF_FFFF) as u32
}

impl Hle {
    /// `gpu`: a device to render PNG dumps with (the window passes its own);
    /// `None` = create one only if a dump is requested. `sink`: where
    /// displayed frames go.
    pub fn new(gpu: Option<(wgpu::Device, wgpu::Queue)>, sink: Option<FrameSink>) -> Self {
        let dump_frames = DumpFrames::from_env();
        let gfx_shots = env_num("PW64_GFX_SHOTS", 0);
        let fb_shots = env_num("PW64_FB_SHOTS", 0);
        let want_gpu = !matches!(dump_frames, DumpFrames::Never) || gfx_shots > 0;
        // Frame dumps honour `PW64_MSAA` and `PW64_WIDESCREEN` (at a fixed
        // size per run, so `PW64_DUMP_PIXEL` coordinates stay put).
        let widescreen = opts::widescreen();
        let fill = opts::fill_screen();
        let msaa = opts::msaa();
        // `PW64_DUMP_HEIGHT=<px>`: frame PNG dump height (default 480). The
        // width follows the output aspect (4:3, else `PW64_WIDESCREEN`),
        // rounded to even; a fixed size per run keeps `PW64_DUMP_PIXEL`
        // coordinates put.
        let dump_h = env_num("PW64_DUMP_HEIGHT", GFX_H);
        let dump_size = match widescreen {
            Some(a) => (((dump_h as f32 * a / 2.0).round() as u32) * 2, dump_h),
            None => (
                ((dump_h as f32 * 4.0 / 3.0 / 2.0).round() as u32) * 2,
                dump_h,
            ),
        };
        let mut gpu = if !want_gpu || std::env::var_os("PW64_NO_GPU").is_some() {
            None
        } else {
            gpu.map(|(device, queue)| (device, queue, msaa))
                .or_else(|| Self::gpu(msaa))
                .map(|(device, queue, msaa)| {
                    Renderer::new(
                        &device,
                        &queue,
                        wgpu::TextureFormat::Rgba8Unorm,
                        RenderOptions {
                            msaa,
                            widescreen: widescreen.map_or(Widescreen::Off, Widescreen::Aspect),
                            filter: opts::tex_filter(),
                            fill_view: fill,
                            oled: Default::default(),
                        },
                    )
                })
        };
        if want_gpu && gpu.is_none() {
            eprintln!("[hle] no GPU adapter: frame dumps disabled");
        }
        // Framebuffer targets at the dump size (exactly the output aspect,
        // so a target is the whole dump image).
        if let Some(r) = gpu.as_mut() {
            r.set_fb_size(dump_size);
        }
        let mut interp = Interpreter::new();
        // The C side brackets the world view with widescreen tags
        // (pw64_widescreen.c): only that is extended past 4:3.
        interp.wide_tags = true;
        // `PW64_DUMP_DL`: trace every list, written next to each dumped frame.
        if std::env::var_os("PW64_DUMP_DL").is_some() {
            interp.trace = Some(String::new());
        }
        // `PW64_TEX_PACKS=<dir>`: replace textures by content key
        // (`packs.rs`). `PW64_DUMP_TEX`: dump every texture the interpreter
        // decodes (bind_tile -> replacer, once per key) as it will be drawn,
        // i.e. the pack's replacement where it has one.
        let packs = crate::packs::TexPacks::from_env();
        let dumping_tex = std::env::var_os("PW64_DUMP_TEX").is_some();
        if dumping_tex || packs.any() {
            interp.textures.replacer = Some(Box::new(move |key, img| {
                let replaced = packs.replace(key);
                if dumping_tex {
                    let shown = replaced.as_ref().unwrap_or(img);
                    write_png(
                        &format!("tmp/tex/{key:016x}.png"),
                        &shown.rgba,
                        shown.width,
                        shown.height,
                    );
                }
                replaced
            }));
        }
        Self {
            gpu,
            dump_size,
            interp,
            snow_interp: Interpreter::new(),
            mem: GfxMemory,
            sink,
            ops: Vec::new(),
            dirty: Vec::new(),
            last: Vec::new(),
            shown: None,
            dump_frames,
            gfx_shots,
            fb_shots,
            gfx_shot: 0,
            fb_shot: 0,
            gfx_tasks: 0,
            first_gfx: false,
            first_present: false,
            first_real_present: false,
            last_fb: Vec::new(),
            stop_at_milestone: std::env::var_os("PW64_STOP_MILESTONE").is_some(),
        }
    }

    fn gpu(msaa: u32) -> Option<(wgpu::Device, wgpu::Queue, u32)> {
        if std::env::var_os("PW64_NO_GPU").is_some() {
            return None;
        }
        let instance = wgpu::Instance::default();
        let adapter = pw64_gfx::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
        }))
        .ok()?;
        let msaa = opts::resolve_msaa(&adapter, wgpu::TextureFormat::Rgba8Unorm, msaa);
        let name = adapter.get_info().name;
        let (device, queue) =
            pw64_gfx::block_on(adapter.request_device(&pw64_gfx::device_descriptor(&adapter)))
                .ok()?;
        eprintln!("[hle] GPU: {name}, msaa {msaa}×");
        Some((device, queue, msaa))
    }

    /// The gfx-task handler: interpret (and optionally render) one task.
    pub fn on_gfx_task(&mut self, info: &GfxTaskInfo) {
        self.gfx_tasks += 1;
        let dl = info.data_ptr as u32;
        // `capture::run` = `interp.run` unless PW64_CAPTURE_DL lists this task.
        let frame = INTERP.time(|| pw64_gfx::capture::run(&mut self.interp, &self.mem, dl));
        if self.gfx_tasks.is_power_of_two() {
            eprintln!(
                "[hle] gfx task #{}, {} commands, {} draws, {} tris",
                self.gfx_tasks,
                info.data_size / 8,
                frame.draws.len(),
                frame.triangle_count()
            );
        }
        if !self.first_gfx {
            self.first_gfx = true;
            let unknown = self.interp.unknown_opcodes();
            eprintln!(
                "[milestone] first gfx task interpreted: dlist {dl:#x} \
                 ({} commands), {} draws, {} tris, {} textures, target fb {:#x}{}",
                info.data_size / 8,
                frame.draws.len(),
                frame.triangle_count(),
                frame.textures.len(),
                info.framebuffer,
                if unknown.is_empty() {
                    String::new()
                } else {
                    format!(", unimplemented opcodes: {unknown:02X?}")
                }
            );
            self.milestone();
        }
        if self.gfx_shots > 0
            && let Some(renderer) = self.gpu.as_mut()
        {
            self.gfx_shots -= 1;
            self.gfx_shot += 1;
            let rgba = renderer.render_to_rgba(&frame, self.dump_size);
            let out = format!("tmp/gfx_{:03}.png", self.gfx_shot);
            write_png(&out, &rgba, self.dump_size.0, self.dump_size.1);
        }
        // Drawn over the framebuffer's current contents (several tasks per
        // swap accumulate, as on the N64).
        let fb = fb_key(info.framebuffer);
        let frame = Arc::new(frame);
        self.submit(fb, frame.clone());
        let trace = self.interp.trace.clone().unwrap_or_default();
        self.last.retain(|(f, ..)| *f != fb);
        self.last.push((fb, frame, trace));
    }

    /// A finished `frame` drawn into framebuffer `fb` (N64 address): onto
    /// the GPU target when rendering, and queued for the sink. Shared by
    /// [`Self::on_gfx_task`] and [`Self::fb_pixels`].
    fn submit(&mut self, fb: u32, frame: Arc<Frame>) {
        if let Some(renderer) = self.gpu.as_mut() {
            DUMP_RENDER.time(|| renderer.fb_draw(fb, &frame));
        }
        if self.sink.is_some() {
            self.ops.push(FbOp::Draw {
                fb,
                frame: frame.clone(),
            });
        }
        self.touch(fb);
    }

    /// `uvCopyFrameBuf` (graphics.c.patch → `pw64_fb_copy`): framebuffer
    /// `src` copied into `dst` (N64 addresses), after the RDRAM memcpy.
    pub fn fb_copy(&mut self, dst: u32, src: u32) {
        let (dst, src) = (fb_key(dst as usize), fb_key(src as usize));
        if let Some(renderer) = self.gpu.as_mut() {
            renderer.fb_copy(dst, src);
        }
        if self.sink.is_some() {
            self.ops.push(FbOp::Copy { dst, src });
        }
        self.touch(dst);
    }

    /// Snow (snow.c.patch → `pw64_fb_pixels`): the C wrote `idx` (y*320+x)
    /// pixel indexes with `color` into framebuffer `fb_raw`'s RDRAM (the
    /// previous, finished frame). Framebuffers are GPU targets, so draw
    /// the pixels there as 1-pixel FILL rects (`pw64_gfx::pixels`), on a
    /// separate interpreter so the game's display-list state stays put.
    pub fn fb_pixels(&mut self, fb_raw: u32, idx: &[u32], color: u16) {
        let mut mem = VecMemory::default();
        let dl = pw64_gfx::pixels::fill_pixels_dl(&mut mem, fb_raw, idx, color);
        let frame = self.snow_interp.run(&mem, dl);
        self.submit(fb_key(fb_raw as usize), Arc::new(frame));
    }

    /// Marks `fb` as changed since it was last latched.
    fn touch(&mut self, fb: u32) {
        if !self.dirty.contains(&fb) {
            self.dirty.push(fb);
        }
    }

    /// The retrace hook on the 60 Hz VI path: the retrace latches swaps, so
    /// [`Self::latch`] then [`Self::retrace`].
    pub fn present(&mut self, r: &Retrace) {
        self.latch(r.framebuffer, r.black);
        self.retrace(r);
    }

    /// A swap latch (VI retrace, or the present tick with `PW64_FPS` ≠ 60):
    /// hand the framebuffer operations + the latched framebuffer to the
    /// window sink.
    pub fn latch(&mut self, framebuffer: usize, black: bool) {
        if framebuffer == 0 || black {
            return;
        }
        // A latched fb that was drawn (or copied into) since it was last
        // shown becomes the shown one; a re-latched fb without changes, or
        // one never drawn, keeps showing the old picture.
        let fb = fb_key(framebuffer);
        let newly = self.dirty.iter().position(|&f| f == fb).map(|i| {
            self.dirty.swap_remove(i);
            fb
        });
        if newly.is_some() {
            self.shown = newly;
        }
        if let Some(sink) = self.sink.as_mut()
            && (newly.is_some() || !self.ops.is_empty())
        {
            sink(std::mem::take(&mut self.ops), newly);
        }
    }

    /// Per VI retrace (time-keyed, so runs at any `PW64_FPS` line up): dump
    /// the shown target (`PW64_DUMP_FRAMES`) and the RDRAM fb dumps.
    pub fn retrace(&mut self, r: &Retrace) {
        if r.framebuffer == 0 {
            return;
        }
        if !self.first_present {
            self.first_present = true;
            eprintln!(
                "[hle] first VI present: retrace {} latches fb {:#x} (black={})",
                r.number, r.framebuffer, r.black
            );
        }
        if r.black {
            return;
        }
        if self.dump_frames.due(r.number)
            && let (Some(renderer), Some(shown)) = (self.gpu.as_mut(), self.shown)
            && let Some(rgba) = DUMP_RENDER.time(|| renderer.fb_read_rgba(shown))
        {
            write_png(
                &format!("tmp/frame_{:05}.png", r.number),
                &rgba,
                self.dump_size.0,
                self.dump_size.1,
            );
            // The draw list of the last task into the shown fb (earlier
            // tasks' pixels may still show: the target persists).
            if let Some((_, frame, trace)) = self.last.iter().find(|(f, ..)| *f == shown)
                && !trace.is_empty()
            {
                use std::fmt::Write;
                let mut s = String::new();
                for (i, d) in frame.draws.iter().enumerate() {
                    let _ = writeln!(
                        s,
                        "draw {i}: verts {}+{} 3d={} wide {:?} {:?} hud {} scissor {:?} {:?} tex {:?} prim {:?} env {:?} blend {:?}",
                        d.first_vertex,
                        d.vertex_count,
                        d.is_3d,
                        d.wide,
                        d.anchor,
                        d.hud,
                        d.scissor,
                        d.pipeline,
                        d.textures.each_ref().map(|t| t.as_ref().map(|t| t.key)),
                        d.uniforms.prim,
                        d.uniforms.env,
                        d.uniforms.blend,
                    );
                }
                // `PW64_DUMP_PIXEL=x,y` (dump-PNG coords): which draws change
                // that pixel (renders every draw prefix; slow, debug only).
                // The prefixes start from black, not from the persistent
                // target's earlier contents.
                if let Ok(p) = std::env::var("PW64_DUMP_PIXEL") {
                    let xy: Vec<usize> = p.split(',').filter_map(|v| v.parse().ok()).collect();
                    if let [x, y] = xy[..]
                        && x < self.dump_size.0 as usize
                        && y < self.dump_size.1 as usize
                    {
                        let mut prev = [0u8; 4];
                        let mut part = Frame::clone(frame);
                        for n in 0..=frame.draws.len() {
                            part.draws = frame.draws[..n].to_vec();
                            let img = renderer.render_to_rgba(&part, self.dump_size);
                            let o = (y * self.dump_size.0 as usize + x) * 4;
                            let px = [img[o], img[o + 1], img[o + 2], img[o + 3]];
                            if px != prev {
                                let _ = writeln!(
                                    s,
                                    "pixel ({x},{y}) after draw {}: {px:?}",
                                    n as i64 - 1
                                );
                                prev = px;
                            }
                        }
                    }
                }
                s += "\n";
                s += trace;
                let _ = std::fs::write(format!("tmp/frame_{:05}_dl.txt", r.number), s);
            }
        }
        if self.first_real_present && self.fb_shots == 0 {
            return;
        }
        // Read the fb as the game writes it: RGBA5551, 320×240.
        let mut bytes = vec![0u8; FB_W * FB_H * 2];
        self.mem.read_bytes(r.framebuffer as u32, &mut bytes);
        let mut rgba = vec![0u8; FB_W * FB_H * 4];
        for (i, px) in bytes.as_chunks::<2>().0.iter().enumerate() {
            let px = u16::from_be_bytes(*px);
            let [r5, g5, b5] =
                [(px >> 11) & 0x1F, (px >> 6) & 0x1F, (px >> 1) & 0x1F].map(|c| c as u8);
            let o = &mut rgba[i * 4..i * 4 + 4];
            // 5→8 bits: replicate the top bits (like the VI's DAC would).
            o[0] = (r5 << 3) | (r5 >> 2);
            o[1] = (g5 << 3) | (g5 >> 2);
            o[2] = (b5 << 3) | (b5 >> 2);
            o[3] = 0xFF;
        }
        if !self.first_real_present {
            self.first_real_present = true;
            eprintln!(
                "[milestone] first framebuffer present at retrace {}: fb {:#x} is not black",
                r.number, r.framebuffer
            );
            self.last_fb = rgba.clone();
            if self.fb_shots > 0 {
                self.fb_shots -= 1;
                self.fb_shot += 1;
                let out = format!("tmp/fb_{:03}.png", self.fb_shot);
                write_png(&out, &rgba, FB_W as u32, FB_H as u32);
            }
            self.milestone();
            return;
        }
        // Later dumps only when the picture changed.
        if self.fb_shots > 0 && self.last_fb != rgba {
            self.fb_shots -= 1;
            self.fb_shot += 1;
            let out = format!("tmp/fb_{:03}.png", self.fb_shot);
            write_png(&out, &rgba, FB_W as u32, FB_H as u32);
            self.last_fb = rgba;
        }
    }

    /// `PW64_STOP_MILESTONE=1`: exit at the first milestone (the crash-free
    /// proof). The default keeps running to `PW64_MAX_RETRACES`.
    fn milestone(&mut self) {
        if self.stop_at_milestone {
            eprintln!("[hle] PW64_STOP_MILESTONE: stopping at the milestone");
            std::process::exit(0);
        }
    }
}

fn env_num(name: &str, default: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Comma-separated list env (`PW64_WIN_SHOT`).
pub fn env_list(name: &str) -> Vec<u64> {
    std::env::var(name)
        .ok()
        .map(|v| v.split(',').filter_map(|s| s.trim().parse().ok()).collect())
        .unwrap_or_default()
}

/// Reads a surface/backing texture back and writes it as a PNG (what the
/// window shows, including the scale blit). Runs on the window thread; the
/// device/queue are the window's own. Handles the 32-bit unorm formats a
/// swapchain picks (BGRA needs an R/B swap for the RGBA PNG).
pub(crate) fn shot_png(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    tex: &wgpu::Texture,
    path: &str,
) {
    let format = tex.format();
    let bgra = format == wgpu::TextureFormat::Bgra8Unorm;
    if !bgra && format != wgpu::TextureFormat::Rgba8Unorm {
        eprintln!("[pw64] window shot: unsupported surface format {format:?}; skipped");
        return;
    }
    let (w, h) = (tex.width(), tex.height());
    let row = (w * 4).next_multiple_of(256);
    let buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("pw64 window shot"),
        size: (row * h) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = device.create_command_encoder(&Default::default());
    enc.copy_texture_to_buffer(
        tex.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buf,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(row),
                rows_per_image: Some(h),
            },
        },
        tex.size(),
    );
    queue.submit([enc.finish()]);
    buf.slice(..)
        .map_async(wgpu::MapMode::Read, |r| r.expect("map shot"));
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("device poll");
    let data = buf.slice(..).get_mapped_range();
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        let o = (y * row) as usize;
        let line = &data[o..o + (w * 4) as usize];
        if !bgra {
            rgba.extend_from_slice(line);
        } else {
            for px in line.as_chunks::<4>().0 {
                rgba.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
            }
        }
    }
    drop(data);
    write_png(path, &rgba, w, h);
}

fn write_png(path: &str, rgba: &[u8], w: u32, h: u32) {
    let _ = std::fs::create_dir_all(Path::new(path).parent().unwrap_or(Path::new(".")));
    let file = match std::fs::File::create(path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("[hle] could not write {path}: {e}");
            return;
        }
    };
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let res = enc
        .write_header()
        .and_then(|mut w| w.write_image_data(rgba));
    match res {
        Ok(()) => eprintln!("[hle] wrote {path}"),
        Err(e) => eprintln!("[hle] PNG {path}: {e}"),
    }
}
