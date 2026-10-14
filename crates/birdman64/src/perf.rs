//! Optional performance overlay: a small translucent card in a window
//! corner with the frame rate and, each opt-in, the frame time, a frame-time
//! graph, 1% lows, CPU load and GPU load.
//!
//! Drawn like the toasts (egui + egui-wgpu over the frame already in the
//! surface, own pass with `LoadOp::Load`). With everything off it does no
//! egui or GPU work at all, so game rendering is untouched.
//!
//! - **Frame rate / frame time:** from the intervals between game frames
//!   (`UserEvent::Frame`), not between redraws, so it is the game's rate even
//!   while the window redraws for other reasons.
//! - **CPU:** the busiest thread's CPU time over wall time, as a percentage
//!   of one core (the game, audio and window each run on their own thread;
//!   a game is limited by its busiest thread, and a whole-process figure
//!   divided by all cores would hide that on a many-core PC).
//! - **GPU:** a wgpu queue has no portable busy counter, so this is the
//!   time from the frame's last submit until the queue reports it done,
//!   over the frame interval. It is an estimate: it includes anything that
//!   was still queued ahead of the frame.

use crate::settings;
use crate::window::Gpu;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Frame intervals kept (1% lows and the graph read from these).
const HISTORY: usize = 300;
/// Samples the graph draws (the newest ones).
const GRAPH_SAMPLES: usize = 120;
/// The fps number averages over this long.
const FPS_WINDOW: Duration = Duration::from_millis(500);
/// CPU load is sampled this often.
const CPU_PERIOD: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Corner {
    TopLeft,
    #[default]
    TopRight,
    BottomLeft,
    BottomRight,
}

impl Corner {
    pub const ALL: [Corner; 4] = [
        Corner::TopLeft,
        Corner::TopRight,
        Corner::BottomLeft,
        Corner::BottomRight,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Corner::TopLeft => "top_left",
            Corner::TopRight => "top_right",
            Corner::BottomLeft => "bottom_left",
            Corner::BottomRight => "bottom_right",
        }
    }

    pub fn parse(s: &str) -> Option<Corner> {
        Self::ALL
            .into_iter()
            .find(|c| c.as_str() == s.trim().to_ascii_lowercase())
    }

    pub fn label(self) -> &'static str {
        match self {
            Corner::TopLeft => "Top left",
            Corner::TopRight => "Top right",
            Corner::BottomLeft => "Bottom left",
            Corner::BottomRight => "Bottom right",
        }
    }

    /// The next corner in `ALL`, wrapping (`d` = +1 / -1).
    pub fn step(self, d: i32) -> Corner {
        let i = Self::ALL.iter().position(|&c| c == self).unwrap_or(0) as i32;
        Self::ALL[(i + d).rem_euclid(Self::ALL.len() as i32) as usize]
    }

    fn anchor(self) -> egui::Align2 {
        match self {
            Corner::TopLeft => egui::Align2::LEFT_TOP,
            Corner::TopRight => egui::Align2::RIGHT_TOP,
            Corner::BottomLeft => egui::Align2::LEFT_BOTTOM,
            Corner::BottomRight => egui::Align2::RIGHT_BOTTOM,
        }
    }
}

/// What the overlay shows. `fps` (the counter itself) is `[graphics]
/// show_fps`; the rest is `[perf]`. Everything defaults to off.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Cfg {
    pub fps: bool,
    pub frametime: bool,
    pub graph: bool,
    pub lows: bool,
    pub cpu: bool,
    pub gpu: bool,
    pub corner: Corner,
}

impl Cfg {
    /// Anything to draw at all.
    pub fn any(&self) -> bool {
        self.fps || self.frametime || self.graph || self.lows || self.cpu || self.gpu
    }
}

static CFG: Mutex<Option<Cfg>> = Mutex::new(None);

/// The live options (the settings screen edits them; the window reads
/// them each frame). Seeded from `pw64.toml` on first use.
pub fn cfg() -> Cfg {
    let mut g = CFG.lock().unwrap_or_else(|e| e.into_inner());
    *g.get_or_insert_with(|| {
        let c = crate::config::get();
        Cfg {
            fps: c.graphics.show_fps,
            ..c.perf
        }
    })
}

pub fn set_cfg(c: Cfg) {
    *CFG.lock().unwrap_or_else(|e| e.into_inner()) = Some(c);
}

/// CPU time (user + kernel) in seconds of every thread of this process,
/// keyed by thread id.
fn thread_cpu_secs() -> Option<HashMap<u32, f64>> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
        };
        use windows_sys::Win32::System::Threading::{
            GetCurrentProcessId, GetThreadTimes, OpenThread, THREAD_QUERY_LIMITED_INFORMATION,
        };
        let mut out = HashMap::new();
        // SAFETY: Toolhelp snapshot walk with a correctly sized entry; every
        // handle opened here is closed here.
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
            if snap == INVALID_HANDLE_VALUE {
                return None;
            }
            let pid = GetCurrentProcessId();
            let mut e: THREADENTRY32 = std::mem::zeroed();
            e.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
            let mut ok = Thread32First(snap, &mut e);
            while ok != 0 {
                if e.th32OwnerProcessID == pid {
                    let h = OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, e.th32ThreadID);
                    if !h.is_null() {
                        let z = FILETIME {
                            dwLowDateTime: 0,
                            dwHighDateTime: 0,
                        };
                        let (mut c, mut x, mut k, mut u) = (z, z, z, z);
                        if GetThreadTimes(h, &mut c, &mut x, &mut k, &mut u) != 0 {
                            let t = |f: FILETIME| {
                                (u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime)
                            };
                            // 100 ns units.
                            out.insert(e.th32ThreadID, (t(k) + t(u)) as f64 / 1e7);
                        }
                        CloseHandle(h);
                    }
                }
                ok = Thread32Next(snap, &mut e);
            }
            CloseHandle(snap);
        }
        Some(out)
    }
    #[cfg(unix)]
    {
        // SAFETY: plain query.
        let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as f64;
        let mut out = HashMap::new();
        for ent in std::fs::read_dir("/proc/self/task").ok()?.flatten() {
            let Some(tid) = ent.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
                continue;
            };
            let Ok(stat) = std::fs::read_to_string(ent.path().join("stat")) else {
                continue;
            };
            // Fields after the ")" that closes the (possibly spaced) name:
            // state is field 3, utime/stime are 14/15.
            let Some(rest) = stat.rsplit_once(')').map(|(_, r)| r) else {
                continue;
            };
            let f: Vec<&str> = rest.split_whitespace().collect();
            if let (Some(u), Some(s)) = (
                f.get(11).and_then(|v| v.parse::<f64>().ok()),
                f.get(12).and_then(|v| v.parse::<f64>().ok()),
            ) {
                out.insert(tid, (u + s) / hz);
            }
        }
        Some(out)
    }
    #[cfg(not(any(windows, unix)))]
    {
        None
    }
}

/// The busiest thread's load between two samples, in percent of one core.
/// Threads that exist in only one sample are ignored.
fn top_thread_pct(prev: &HashMap<u32, f64>, now: &HashMap<u32, f64>, wall: f64) -> f32 {
    let top = now
        .iter()
        .filter_map(|(id, c)| prev.get(id).map(|p| c - p))
        .fold(0.0_f64, f64::max);
    (top / wall * 100.0).clamp(0.0, 100.0) as f32
}

/// Frame-time statistics, kept apart from the drawing so they unit-test.
#[derive(Default)]
pub struct Stats {
    /// Frame intervals in ms, oldest first.
    frames: VecDeque<f32>,
    last: Option<Instant>,
    /// (when, frame count) for the fps window.
    window_start: Option<Instant>,
    window_frames: u32,
    pub fps: f32,
}

impl Stats {
    /// `n` game frames arrived at `now` (more than 1 when a redraw skipped
    /// some: the interval is split evenly).
    pub fn frame(&mut self, now: Instant, n: u32) {
        // The game was paused (settings open, minimized): not a slow frame.
        if self
            .last
            .is_some_and(|l| now.duration_since(l) > Duration::from_millis(500))
        {
            self.reset_gap();
        }
        if let Some(last) = self.last {
            let ms = now.duration_since(last).as_secs_f32() * 1000.0 / n.max(1) as f32;
            for _ in 0..n {
                if self.frames.len() == HISTORY {
                    self.frames.pop_front();
                }
                self.frames.push_back(ms);
            }
        }
        self.last = Some(now);
        let start = *self.window_start.get_or_insert(now);
        self.window_frames += n;
        let dt = now.duration_since(start);
        if dt >= FPS_WINDOW {
            self.fps = self.window_frames as f32 / dt.as_secs_f32();
            self.window_start = Some(now);
            self.window_frames = 0;
        }
    }

    /// A pause (overlay open, minimized) made a long gap: forget it rather
    /// than count it as one terrible frame.
    pub fn reset_gap(&mut self) {
        self.last = None;
        self.window_start = None;
        self.window_frames = 0;
    }

    pub fn latest_ms(&self) -> Option<f32> {
        self.frames.back().copied()
    }

    pub fn median_ms(&self) -> Option<f32> {
        let mut v: Vec<f32> = self.frames.iter().copied().collect();
        if v.is_empty() {
            return None;
        }
        v.sort_by(f32::total_cmp);
        Some(v[v.len() / 2])
    }

    /// "1% low" fps: the frame rate at the 99th percentile frame time of
    /// the history (the slowest 1% of frames are at least this slow).
    pub fn low_1pct_fps(&self) -> Option<f32> {
        let mut v: Vec<f32> = self.frames.iter().copied().collect();
        if v.len() < 30 {
            return None;
        }
        v.sort_by(f32::total_cmp);
        let i = ((v.len() as f32 * 0.99).ceil() as usize).clamp(1, v.len()) - 1;
        (v[i] > 0.0).then(|| 1000.0 / v[i])
    }

    fn graph(&self) -> impl Iterator<Item = f32> + '_ {
        let skip = self.frames.len().saturating_sub(GRAPH_SAMPLES);
        self.frames.iter().skip(skip).copied()
    }
}

pub struct Hud {
    ctx: egui::Context,
    renderer: Option<egui_wgpu::Renderer>,
    started: Instant,
    pub stats: Stats,
    cpu: Option<f32>,
    cpu_at: Option<(Instant, HashMap<u32, f64>)>,
    /// GPU busy estimate in microseconds, written by the waiter thread.
    gpu_us: Arc<AtomicU32>,
    /// Frame-submitted timestamps for the waiter thread (spawned on first
    /// use; capacity 1, so a slow GPU just skips samples).
    gpu_tx: Option<std::sync::mpsc::SyncSender<Instant>>,
    gpu_smooth: Option<f32>,
}

impl Default for Hud {
    fn default() -> Self {
        Self::new()
    }
}

impl Hud {
    pub fn new() -> Self {
        Self {
            ctx: egui::Context::default(),
            renderer: None,
            started: Instant::now(),
            stats: Stats::default(),
            cpu: None,
            cpu_at: None,
            gpu_us: Arc::new(AtomicU32::new(0)),
            gpu_tx: None,
            gpu_smooth: None,
        }
    }

    /// Call per drained game frame batch.
    pub fn frames(&mut self, n: u32) {
        if cfg().any() {
            self.stats.frame(Instant::now(), n);
        }
    }

    fn sample_cpu(&mut self) {
        let now = Instant::now();
        if let Some((t, _)) = &self.cpu_at
            && now.duration_since(*t) < CPU_PERIOD
        {
            return;
        }
        let Some(threads) = thread_cpu_secs() else {
            return;
        };
        if let Some((t, prev)) = &self.cpu_at {
            let wall = now.duration_since(*t).as_secs_f64();
            self.cpu = Some(top_thread_pct(prev, &threads, wall));
        }
        self.cpu_at = Some((now, threads));
    }

    /// Times how long the GPU takes to finish what was just submitted. A
    /// helper thread blocks in `device.poll(Wait)` (a queue callback only
    /// fires when something else polls, which reads as a full frame); the
    /// render thread never waits.
    fn sample_gpu(&mut self, gpu: &Gpu) {
        let us = self.gpu_us.swap(0, Ordering::Relaxed);
        if us > 0 {
            let ms = us as f32 / 1000.0;
            self.gpu_smooth = Some(self.gpu_smooth.map_or(ms, |s| s + (ms - s) * 0.1));
        }
        let tx = self.gpu_tx.get_or_insert_with(|| {
            let (tx, rx) = std::sync::mpsc::sync_channel::<Instant>(1);
            let (device, out) = (gpu.device.clone(), self.gpu_us.clone());
            std::thread::Builder::new()
                .name("pw64-perf-gpu".into())
                .spawn(move || {
                    for t0 in rx {
                        let _ = device.poll(wgpu::PollType::wait_indefinitely());
                        let us = t0.elapsed().as_micros().clamp(1, u128::from(u32::MAX));
                        out.store(us as u32, Ordering::Relaxed);
                    }
                })
                .ok();
            tx
        });
        let _ = tx.try_send(Instant::now());
    }

    /// Draws the overlay over the frame in `view`. Nothing enabled →
    /// returns before any egui or GPU work.
    pub fn render(&mut self, gpu: &Gpu, view: &wgpu::TextureView) {
        let c = cfg();
        if !c.any() {
            self.cpu_at = None;
            return;
        }
        if c.cpu {
            self.sample_cpu();
        }
        if c.gpu {
            self.sample_gpu(gpu);
        }
        let ppp = gpu.window.scale_factor() as f32;
        // A bit smaller than the settings zoom: this is a corner readout.
        let zoom = settings::ui_zoom(gpu.config.height, ppp);
        if (self.ctx.zoom_factor() - zoom).abs() > f32::EPSILON {
            self.ctx.set_zoom_factor(zoom);
        }
        let raw = settings::raw_input_zoom(
            gpu,
            ppp,
            zoom,
            self.started.elapsed().as_secs_f64(),
            Vec::new(),
        );
        let ctx = self.ctx.clone();
        let out = ctx.run(raw, |ctx| self.ui(ctx, c));
        settings::paint(
            gpu,
            &self.ctx,
            &mut self.renderer,
            out,
            view,
            wgpu::LoadOp::Load,
        );
    }

    fn ui(&self, ctx: &egui::Context, c: Cfg) {
        let st = &self.stats;
        let med = st.median_ms().unwrap_or(16.7);
        let fg = egui::Color32::from_rgb(240, 243, 250);
        let dim = egui::Color32::from_rgb(140, 150, 168);
        let bg = egui::Color32::from_rgba_unmultiplied(15, 18, 24, 175);
        let off = match c.corner {
            Corner::TopLeft => egui::vec2(8.0, 8.0),
            Corner::TopRight => egui::vec2(-8.0, 8.0),
            Corner::BottomLeft => egui::vec2(8.0, -8.0),
            Corner::BottomRight => egui::vec2(-8.0, -8.0),
        };
        egui::Area::new(egui::Id::new("pw64 perf"))
            .anchor(c.corner.anchor(), off)
            .interactable(false)
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(bg)
                    .corner_radius(5)
                    .inner_margin(egui::Margin::symmetric(8, 6))
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
                        // Every row is as wide as the graph: label left,
                        // value right, so the numbers line up.
                        if c.fps {
                            let v = format!("{:.0}", st.fps);
                            stat_row(ui, "FPS", &v, 17.0, fg, dim);
                        }
                        if c.frametime
                            && let Some(ms) = st.latest_ms()
                        {
                            stat_row(ui, "Frame", &format!("{ms:.1} ms"), 12.0, fg, dim);
                        }
                        if c.lows {
                            let v = st
                                .low_1pct_fps()
                                .map_or("--".to_string(), |f| format!("{f:.0}"));
                            stat_row(ui, "1% low", &v, 12.0, fg, dim);
                        }
                        if c.graph {
                            graph(ui, st, med);
                        }
                        if c.cpu {
                            let v = self.cpu.map_or("--".to_string(), |p| format!("{p:.0}%"));
                            stat_row(ui, "CPU core", &v, 12.0, fg, dim);
                        }
                        if c.gpu {
                            let v = match (self.gpu_smooth, st.fps > 0.0) {
                                // Busy time over the frame interval.
                                (Some(ms), true) => {
                                    format!("{:.0}%", (ms * st.fps / 10.0).clamp(0.0, 100.0))
                                }
                                _ => "--".to_string(),
                            };
                            stat_row(ui, "GPU", &v, 12.0, fg, dim);
                        }
                    });
            });
    }
}

/// One fixed-width row: dim label at the left, bright value at the right.
fn stat_row(
    ui: &mut egui::Ui,
    label: &str,
    value: &str,
    size: f32,
    fg: egui::Color32,
    dim: egui::Color32,
) {
    ui.allocate_ui_with_layout(
        egui::vec2(GRAPH_SAMPLES as f32, size + 3.0),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.set_min_width(GRAPH_SAMPLES as f32);
            ui.label(egui::RichText::new(label).size(11.0).color(dim));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    egui::RichText::new(value)
                        .monospace()
                        .size(size)
                        .strong()
                        .color(fg),
                );
            });
        },
    );
}

/// Frame-time bars: height ∝ ms (full height = 3× the median), green near
/// the median, yellow when 1.5× slower, red when 2.5× slower.
fn graph(ui: &mut egui::Ui, st: &Stats, median: f32) {
    let size = egui::vec2(GRAPH_SAMPLES as f32, 30.0);
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let p = ui.painter_at(rect);
    p.rect_filled(
        rect,
        2.0,
        egui::Color32::from_rgba_unmultiplied(0, 0, 0, 90),
    );
    // Right-aligned: the newest frame sits at the right edge.
    let n = st.graph().count();
    let x0 = rect.right() - n as f32;
    for (i, ms) in st.graph().enumerate() {
        let h = (ms / (median * 3.0)).clamp(0.04, 1.0) * rect.height();
        let col = if ms > median * 2.5 {
            egui::Color32::from_rgb(235, 90, 80)
        } else if ms > median * 1.5 {
            egui::Color32::from_rgb(235, 200, 80)
        } else {
            egui::Color32::from_rgb(110, 205, 120)
        };
        let x = x0 + i as f32;
        p.line_segment(
            [
                egui::pos2(x + 0.5, rect.bottom()),
                egui::pos2(x + 0.5, rect.bottom() - h),
            ],
            egui::Stroke::new(1.0_f32, col),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(s: &mut Stats, ms: &[f32]) {
        let mut t = Instant::now();
        s.frame(t, 1);
        for &m in ms {
            t += Duration::from_secs_f32(m / 1000.0);
            s.frame(t, 1);
        }
    }

    #[test]
    fn steady_60_fps() {
        let mut s = Stats::default();
        feed(&mut s, &[1000.0 / 60.0; 120]);
        assert!((s.fps - 60.0).abs() < 1.5, "{}", s.fps);
        assert!((s.median_ms().unwrap() - 16.67).abs() < 0.1);
        assert!((s.low_1pct_fps().unwrap() - 60.0).abs() < 1.5);
    }

    #[test]
    fn one_percent_low_catches_spikes() {
        let mut s = Stats::default();
        let mut v = vec![10.0_f32; 295];
        v.extend([50.0; 5]);
        feed(&mut s, &v);
        let low = s.low_1pct_fps().unwrap();
        assert!((low - 20.0).abs() < 0.5, "{low}");
    }

    #[test]
    fn low_needs_enough_samples() {
        let mut s = Stats::default();
        feed(&mut s, &[10.0; 10]);
        assert!(s.low_1pct_fps().is_none());
    }

    #[test]
    fn gap_is_forgotten() {
        let mut s = Stats::default();
        feed(&mut s, &[10.0; 40]);
        s.reset_gap();
        s.frame(Instant::now() + Duration::from_secs(30), 1);
        assert!(s.latest_ms().unwrap() < 11.0);
    }

    #[test]
    fn corner_parse_and_step() {
        for c in Corner::ALL {
            assert_eq!(Corner::parse(c.as_str()), Some(c));
        }
        assert_eq!(Corner::parse("middle"), None);
        assert_eq!(Corner::TopRight.step(1), Corner::BottomLeft);
        assert_eq!(Corner::TopLeft.step(-1), Corner::BottomRight);
    }

    #[test]
    fn thread_times_are_readable_and_top_thread_found() {
        let a = thread_cpu_secs().unwrap();
        assert!(!a.is_empty());
        let t0 = Instant::now();
        let mut x = 0u64;
        while t0.elapsed() < Duration::from_millis(300) {
            for i in 0..100_000u64 {
                x = x.wrapping_add(i * i);
            }
        }
        std::hint::black_box(x);
        let b = thread_cpu_secs().unwrap();
        // This test thread spun for 300 ms: it must be the top thread, well
        // above idle (the clock tick is coarse, so only a loose bound).
        let pct = top_thread_pct(&a, &b, t0.elapsed().as_secs_f64());
        assert!(pct > 40.0, "{pct}");
    }

    #[test]
    fn top_thread_ignores_new_threads() {
        let prev = HashMap::from([(1, 1.0), (2, 1.0)]);
        let now = HashMap::from([(1, 1.2), (2, 1.5), (3, 9.0)]);
        assert!((top_thread_pct(&prev, &now, 1.0) - 50.0).abs() < 0.01);
    }
}
