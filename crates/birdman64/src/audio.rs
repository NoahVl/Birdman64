//! Audio wiring (Phase 5): audio SP tasks → `pw64-audio` HLE over the C
//! address space; AI buffers (pw64-platform `ai.rs`) → `cpal` output and an
//! optional WAV dump.
//!
//! Env: `PW64_NO_AUDIO` no output device (HLE and AI timing still run);
//! `PW64_AUDIO_VIRTUAL` with it, a silent host-clocked device for underrun
//! stats; `PW64_DUMP_AUDIO=<file.wav>` write the AI stream (put it under
//! `tmp/`); `PW64_AUDIO_STATS` print the Acmd histogram at exit;
//! `PW64_AUDIO_METER=1|2` music vs SFX vs reverb-return RMS every 5 s of
//! audio (2: plus per-codebook levels at exit).

use pw64_audio::{AudioHle, Output, Rdram, WavWriter};
use pw64_game::memmap::{hle_readable, hle_writable};
use pw64_platform::{ai, headless, pi};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::{config, opts};

/// Output volume, permille (0..1000): set live by the settings screen, read
/// per AI buffer in the sink. An atomic so the window thread doesn't need the
/// audio Rc (which lives on the game thread).
static VOLUME: AtomicU32 = AtomicU32::new(1000);

/// Sets the output volume (0 = muted, 1.0 = full).
pub fn set_volume(v: f32) {
    VOLUME.store(
        (v * 1000.0).round().clamp(0.0, 1000.0) as u32,
        Ordering::Relaxed,
    );
}

/// Current output volume.
pub fn volume() -> f32 {
    VOLUME.load(Ordering::Relaxed) as f32 / 1000.0
}

/// Command-list addresses are `osVirtualToPhysical`/`K0_TO_PHYS` values
/// (host address − 0x80000000 mod 2^32, native-build.md §7): add it back.
#[derive(Default)]
struct AudioMem {
    /// First unmapped access of the current command.
    bad: Option<(u32, usize)>,
}

impl AudioMem {
    /// Host address of `[addr, addr + len)` if the HLE may access it: reads
    /// the RDRAM window, live thread stacks and the exe / module image;
    /// writes only the window and live stacks (`memmap::hle_writable`).
    fn host(addr: u32, len: usize, write: bool) -> Option<usize> {
        let h = addr.wrapping_add(0x8000_0000) as usize;
        let ok = if write {
            hle_writable(h, len)
        } else {
            hle_readable(h, len)
        };
        ok.then_some(h)
    }

    fn warn(&mut self, addr: u32, len: usize) {
        self.bad.get_or_insert((addr, len));
    }
}

impl Rdram for AudioMem {
    fn read(&mut self, addr: u32, out: &mut [u8]) {
        match Self::host(addr, out.len(), false) {
            // SAFETY: bounds-checked against the mapped C address space.
            Some(h) => unsafe {
                std::ptr::copy_nonoverlapping(h as *const u8, out.as_mut_ptr(), out.len())
            },
            None => {
                self.warn(addr, out.len());
                out.fill(0);
            }
        }
    }

    fn write(&mut self, addr: u32, data: &[u8]) {
        match Self::host(addr, data.len(), true) {
            // SAFETY: as above; the C does not touch these buffers while the
            // (synchronous) task runs.
            Some(h) => unsafe {
                std::ptr::copy_nonoverlapping(data.as_ptr(), h as *mut u8, data.len())
            },
            None => self.warn(addr, data.len()),
        }
    }
}

struct Audio {
    hle: AudioHle,
    out: Option<Output>,
    wav: Option<WavWriter>,
    wav_path: Option<std::path::PathBuf>,
    samples: u64,
    peak: i16,
    sum_sq: f64,
    /// Volume-scaled copy of the current AI buffer (reused).
    scaled: Vec<i16>,
    /// `PW64_AUDIO_STATS`: last periodic report (wall time, output counters).
    report: Option<(std::time::Instant, pw64_audio::output::OutputCounters)>,
}

/// Seconds between `PW64_AUDIO_STATS` reports.
const REPORT_SECS: f64 = 5.0;

impl Audio {
    /// `PW64_AUDIO_STATS`: every [`REPORT_SECS`] of wall time, the frames
    /// produced vs played per second, underruns and fill (live health of a
    /// windowed/throttled run; the exit summary can't show when it went wrong).
    fn report(&mut self) {
        let (Some((t0, c0)), Some(o)) = (self.report, &self.out) else {
            return;
        };
        let dt = t0.elapsed().as_secs_f64();
        if dt < REPORT_SECS {
            return;
        }
        let c = o.counters();
        let s = ai::ai_stats();
        eprintln!(
            "[audio] {dt:.1} s: produced {:.0}/s, played {:.0}/s, underruns +{} (+{:.0} ms silence), dropped +{}, fill {:.0} ms, nudge {:+.2}%, AI late {} restarts {}",
            (c.pushed - c0.pushed) as f64 / dt,
            (c.consumed - c0.consumed) as f64 / dt,
            c.underruns - c0.underruns,
            c.silent_ms - c0.silent_ms,
            c.dropped - c0.dropped,
            c.fill_ms,
            c.adjust * 100.0,
            s.starved,
            s.restarts
        );
        self.report = Some((std::time::Instant::now(), c));
    }
}

thread_local! {
    static AUDIO: RefCell<Option<Rc<RefCell<Audio>>>> = const { RefCell::new(None) };
}

/// Installs the audio task handler and AI sink (call on the game thread,
/// before `os::boot`).
pub fn install() {
    // `pw64.toml [graphics] volume` (settings screen default 1.0).
    set_volume(config::get().graphics.volume);
    let lut = pi::rom_bytes()
        .and_then(pw64_audio::lut::from_rom)
        .unwrap_or_else(|| {
            eprintln!("[audio] resampler table not found in the ROM; using linear interpolation");
            pw64_audio::lut::linear()
        });
    let out = if opts::no_audio() {
        // Silent device clocked by the host (48 kHz, 10 ms callbacks) to
        // measure live underruns without sound.
        std::env::var_os("PW64_AUDIO_VIRTUAL")
            .map(|_| Output::open_virtual(48_000, std::time::Duration::from_millis(10)))
    } else {
        match Output::open() {
            Ok(o) => {
                eprintln!(
                    "[audio] output: {} ({} Hz, {} ch)",
                    o.device_name, o.device_rate, o.channels
                );
                Some(o)
            }
            Err(e) => {
                eprintln!("[audio] no output ({e}); samples discarded");
                None
            }
        }
    };
    let wav_path = std::env::var_os("PW64_DUMP_AUDIO").map(std::path::PathBuf::from);
    let report = out
        .as_ref()
        .filter(|_| std::env::var_os("PW64_AUDIO_STATS").is_some())
        .map(|o| (std::time::Instant::now(), o.counters()));
    let mut hle = AudioHle::new(lut);
    if std::env::var_os("PW64_AUDIO_METER").is_some() {
        hle.meter = Some(Box::default());
    }
    let a = Rc::new(RefCell::new(Audio {
        hle,
        out,
        wav: None,
        wav_path,
        samples: 0,
        peak: 0,
        sum_sq: 0.0,
        scaled: Vec::new(),
        report,
    }));
    AUDIO.with(|g| *g.borrow_mut() = Some(a.clone()));
    let h = a.clone();
    headless::set_audio_task_handler(move |info| {
        let n = info.data_size as usize / 8;
        // SAFETY: `data_ptr` is the synthesizer's Acmd list (host pointer,
        // `data_size` bytes of `{u32 w0, w1}` pairs).
        let list = unsafe { std::slice::from_raw_parts(info.data_ptr as *const [u32; 2], n) };
        let hle = &mut h.borrow_mut().hle;
        hle.tasks += 1;
        let mut mem = AudioMem::default();
        for (i, &[w0, w1]) in list.iter().enumerate() {
            hle.exec(&mut mem, w0, w1);
            if let Some((addr, len)) = mem.bad.take() {
                static WARNED: std::sync::Once = std::sync::Once::new();
                WARNED.call_once(|| {
                    eprintln!(
                        "[audio] task {} cmd {i}/{n} {w0:08x} {w1:08x}: unmapped {addr:#x} (+{len:#x}) ignored (logged once)",
                        hle.tasks
                    )
                });
            }
        }
        // Meter window: every 300 tasks (5 s of audio).
        if hle.tasks.is_multiple_of(300)
            && let Some(m) = &mut hle.meter
        {
            eprintln!("[audio] meter tasks {}..{}:", hle.tasks - 300, hle.tasks);
            print_meter(m, false);
            m.reset();
        }
    });
    ai::set_ai_sink(move |pcm, rate| {
        let mut a = a.borrow_mut();
        let a = &mut *a;
        // Volume lives outside the HLE: scale the AI stream (into a reused
        // buffer; the common 100% path stays copy-free) before output and WAV.
        let vol = volume();
        let mut scaled = std::mem::take(&mut a.scaled);
        let pcm = if vol < 1.0 {
            scaled.clear();
            scaled.extend(pcm.iter().map(|&s| (s as f32 * vol).round() as i16));
            &scaled[..]
        } else {
            pcm
        };
        if let Some(o) = &a.out {
            o.push(pcm, rate);
        }
        if a.wav.is_none()
            && let Some(p) = a.wav_path.take()
        {
            match WavWriter::create(&p, rate) {
                Ok(w) => a.wav = Some(w),
                Err(e) => eprintln!("[audio] {}: {e}", p.display()),
            }
        }
        if let Some(w) = &mut a.wav
            && let Err(e) = w.append(pcm)
        {
            eprintln!("[audio] WAV write failed: {e}");
            a.wav = None;
        }
        a.samples += pcm.len() as u64;
        for &s in pcm {
            a.peak = a.peak.max(s.saturating_abs());
            a.sum_sq += (s as f64) * (s as f64);
        }
        a.scaled = scaled;
        a.report();
    });
}

/// `PW64_AUDIO_METER`: per-bank voice levels (the two banks' `.ctl` copies
/// are split at the largest gap between codebook addresses) and the aux
/// (reverb) return vs the dry main bus. RMS values are per output sample
/// (L+R energy / interleaved samples), comparable with the AI stream RMS
/// (a voice's RMS ignores cancellation with other voices).
fn print_meter(m: &pw64_audio::abi::Meter, per_book: bool) {
    let split = m.split();
    // bus[2] = frames × 2 (the L and R mixes) = interleaved AI samples.
    let samples = m.bus[2].max(1.0);
    let rms = |e: f64| (e / samples).sqrt();
    let mut groups = [[0f64; 3]; 2];
    for (k, v) in &m.voices {
        let g = &mut groups[(*k >= split) as usize];
        for i in 0..3 {
            g[i] += v[i];
        }
    }
    let [mu, sfx] = groups;
    eprintln!(
        "[audio] meter RMS: music dry {:.0} wet {:.0} | SFX dry {:.0} wet {:.0} | main dry {:.0} + aux return {:.0} (bank split {split:#x})",
        rms(mu[0]),
        rms(mu[1]),
        rms(sfx[0]),
        rms(sfx[1]),
        rms(m.bus[0]),
        rms(m.bus[1])
    );
    if per_book && std::env::var("PW64_AUDIO_METER").is_ok_and(|v| v == "2") {
        for (k, v) in &m.voices {
            eprintln!(
                "[audio]   book {k:#010x}: dry {:.0} wet {:.0} n {:.0}",
                rms(v[0]),
                rms(v[1]),
                v[2]
            );
        }
    }
}

/// Prints a summary (tasks, AI stream level, output stats, histogram).
pub fn finish() {
    AUDIO.with(|g| {
        let Some(rc) = g.borrow_mut().take() else {
            return;
        };
        // Never drop the cpal stream: the process exits next, and dropping
        // it from a TLS destructor during exit panics (WASAPI teardown).
        let a: &'static RefCell<Audio> = Box::leak(Box::new(rc));
        let a = a.borrow();
        let rms = (a.sum_sq / a.samples.max(1) as f64).sqrt();
        eprintln!(
            "[audio] {} tasks, AI stream {} frames, peak {}, RMS {:.0}",
            a.hle.tasks,
            a.samples / 2,
            a.peak,
            rms
        );
        let s = ai::ai_stats();
        eprintln!(
            "[audio] AI: {} buffers, {} lost (FIFO full), {} late (max gap {} frames; DAC rewound + repaid), {} clock restarts (lag > 150 ms: lost)",
            s.buffers, s.lost, s.starved, s.max_gap_frames, s.restarts
        );
        if let Some(o) = &a.out {
            eprintln!("[audio] output: {}", o.stats());
        }
        if let Some(m) = &a.hle.meter {
            eprintln!("[audio] meter last window:");
            print_meter(m, true);
        }
        if std::env::var_os("PW64_AUDIO_STATS").is_some() {
            let hist: Vec<String> = a
                .hle
                .histogram
                .iter()
                .enumerate()
                .filter(|(_, n)| **n > 0)
                .map(|(i, n)| format!("{} {n}", pw64_audio::abi::NAMES.get(i).unwrap_or(&"?")))
                .collect();
            eprintln!("[audio] Acmd histogram: {}", hist.join(", "));
        }
    });
}
