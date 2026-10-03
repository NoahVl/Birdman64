//! Host audio output: a ring of source-rate stereo frames (pushed by the AI
//! as the game queues buffers) drained by a `cpal` stream (WASAPI shared mode
//! on Windows) at the device rate.
//!
//! Pacing: the game paces itself against the *emulated* AI clock
//! (`osAiGetLength`, pw64-platform `ai.rs`), which follows the host clock
//! when the OS loop is throttled. The ring absorbs the burstiness (one game
//! frame per VI) and the small drift between the two clocks:
//! - resampling is cubic (Catmull-Rom), at `src_rate / device_rate` nudged by up to
//!   ±0.5% in proportion to how far the (smoothed) fill is from the target
//!   (≈ 60 ms), so the fill converges without audible pitch change;
//! - underrun (game stalled): output silence and re-prime to the target;
//! - overflow (> 250 ms, e.g. `PW64_NO_THROTTLE`): drop the oldest frames
//!   back to the target.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

const TARGET_SECS: f64 = 0.060;
const MAX_SECS: f64 = 0.250;
/// Max rate nudge (fraction) and its gain per unit of relative fill error.
const MAX_ADJUST: f64 = 0.005;
const ADJUST_GAIN: f64 = 0.004;

struct Ring {
    q: VecDeque<[i16; 2]>,
    src_rate: f64,
    frac: f64,
    playing: bool,
    fill_avg: f64,
    underruns: u64,
    dropped: u64,
    pushed: u64,
    /// Played at least once (startup priming isn't an underrun).
    started: bool,
    /// Device frames of silence output after an underrun.
    silent: u64,
    /// Lowest fill (source frames) seen at a callback while playing.
    min_fill: usize,
    /// Source frames consumed (played) so far.
    consumed: u64,
    /// Rate nudge of the last callback (fraction, ±MAX_ADJUST).
    adjust: f64,
}

impl Ring {
    fn new(src_rate: f64) -> Self {
        Self {
            q: VecDeque::new(),
            src_rate,
            frac: 0.0,
            playing: false,
            fill_avg: 0.0,
            underruns: 0,
            dropped: 0,
            pushed: 0,
            started: false,
            silent: 0,
            min_fill: usize::MAX,
            consumed: 0,
            adjust: 0.0,
        }
    }

    fn target(&self) -> usize {
        (self.src_rate * TARGET_SECS) as usize
    }

    fn push(&mut self, interleaved: &[i16], rate: u32) {
        self.src_rate = rate as f64;
        self.q
            .extend(interleaved.as_chunks::<2>().0.iter().copied());
        self.pushed += interleaved.len() as u64 / 2;
        let max = (self.src_rate * MAX_SECS) as usize;
        if self.q.len() > max {
            let n = self.q.len() - self.target();
            self.q.drain(..n);
            self.dropped += n as u64;
        }
    }

    /// Next output frame at `device_rate`.
    fn pull(&mut self, ratio: f64) -> [f32; 2] {
        if !self.playing {
            if self.q.len() >= self.target() {
                self.playing = true;
                self.started = true;
            } else {
                self.silent += self.started as u64;
                return [0.0; 2];
            }
        }
        if self.q.len() < 4 {
            self.playing = false;
            self.underruns += 1;
            self.silent += 1;
            return [0.0; 2];
        }
        // Catmull-Rom between q[1] and q[2]. Linear interpolation from
        // 22 kHz dulls the top octave (−4 dB at 8 kHz, −6.5 dB at 10 kHz)
        // and leaves loud images above 11 kHz; the cubic is flatter and
        // cleaner, closer to the DAC + analog filter of the real console.
        let (p0, p1, p2, p3) = (self.q[0], self.q[1], self.q[2], self.q[3]);
        let t = self.frac as f32;
        let out = [0, 1].map(|c| {
            let (a, b, cc, d) = (p0[c] as f32, p1[c] as f32, p2[c] as f32, p3[c] as f32);
            let v = b + 0.5
                * t
                * (cc - a + t * (2.0 * a - 5.0 * b + 4.0 * cc - d + t * (3.0 * (b - cc) + d - a)));
            (v / 32768.0).clamp(-1.0, 1.0)
        });
        self.frac += ratio;
        while self.frac >= 1.0 && !self.q.is_empty() {
            self.q.pop_front();
            self.consumed += 1;
            self.frac -= 1.0;
        }
        out
    }
}

/// Fills one device callback (`channels`-interleaved) from the ring: the
/// same code path for the `cpal` stream and the virtual device.
fn fill<T: SizedSample + FromSample<f32>>(
    r: &mut Ring,
    device_rate: f64,
    channels: usize,
    data: &mut [T],
) {
    if r.playing {
        r.min_fill = r.min_fill.min(r.q.len());
    }
    // ~1 s time constant at typical 10 ms callbacks.
    r.fill_avg = r.fill_avg * 0.99 + r.q.len() as f64 * 0.01;
    let target = r.target() as f64;
    let err = (r.fill_avg - target) / target.max(1.0);
    let adjust = (err * ADJUST_GAIN).clamp(-MAX_ADJUST, MAX_ADJUST);
    r.adjust = adjust;
    let ratio = r.src_rate / device_rate * (1.0 + adjust);
    for frame in data.chunks_mut(channels) {
        let [l, rr] = r.pull(ratio);
        match frame.len() {
            1 => frame[0] = T::from_sample((l + rr) * 0.5),
            _ => {
                frame[0] = T::from_sample(l);
                frame[1] = T::from_sample(rr);
                for s in &mut frame[2..] {
                    *s = T::from_sample(0.0);
                }
            }
        }
    }
}

/// Cumulative ring counters (`Output::counters`); frames are source frames.
#[derive(Clone, Copy, Debug, Default)]
pub struct OutputCounters {
    pub pushed: u64,
    pub consumed: u64,
    pub dropped: u64,
    pub underruns: u64,
    pub silent_ms: f64,
    pub fill_ms: f64,
    /// Rate nudge of the last device callback (fraction).
    pub adjust: f64,
}

/// An open output device. Dropping it stops playback.
pub struct Output {
    /// `None` for the virtual device (its thread runs until exit).
    _stream: Option<cpal::Stream>,
    ring: Arc<Mutex<Ring>>,
    pub device_rate: u32,
    pub channels: u16,
    pub device_name: String,
}

impl Output {
    /// Opens the default output device (shared mode, its mix format).
    pub fn open() -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or("no default output device")?;
        let name = device
            .description()
            .map(|d| d.to_string())
            .unwrap_or_else(|_| "?".into());
        let supported = device
            .default_output_config()
            .map_err(|e| format!("default output config: {e}"))?;
        let config: cpal::StreamConfig = supported.config();
        let ring = Arc::new(Mutex::new(Ring::new(22050.0)));
        use cpal::SampleFormat as F;
        let stream = match supported.sample_format() {
            F::F32 => build::<f32>(&device, &config, ring.clone()),
            F::I16 => build::<i16>(&device, &config, ring.clone()),
            F::I32 => build::<i32>(&device, &config, ring.clone()),
            F::U16 => build::<u16>(&device, &config, ring.clone()),
            f => return Err(format!("unsupported sample format {f}")),
        }?;
        stream.play().map_err(|e| format!("play: {e}"))?;
        Ok(Self {
            _stream: Some(stream),
            ring,
            device_rate: config.sample_rate,
            channels: config.channels,
            device_name: name,
        })
    }

    /// A silent stand-in for a device, to measure live output health
    /// without sound (`PW64_AUDIO_VIRTUAL`): a thread wakes every `period`
    /// of host wall-clock time (like a WASAPI shared-mode callback, ~10 ms)
    /// and pulls exactly the frames a `device_rate` clock has consumed
    /// since it started, through the same `fill` as the cpal callback.
    pub fn open_virtual(device_rate: u32, period: std::time::Duration) -> Self {
        let ring = Arc::new(Mutex::new(Ring::new(22050.0)));
        let r = ring.clone();
        std::thread::Builder::new()
            .name("pw64-audio-virtual".into())
            .spawn(move || {
                let t0 = std::time::Instant::now();
                let rate = device_rate as f64;
                let (mut done, mut k) = (0u64, 0u32);
                let mut buf: Vec<f32> = Vec::new();
                loop {
                    k += 1;
                    let next = t0 + period * k;
                    if let Some(d) = next.checked_duration_since(std::time::Instant::now()) {
                        std::thread::sleep(d);
                    }
                    let due = (t0.elapsed().as_secs_f64() * rate) as u64;
                    buf.resize((due - done) as usize * 2, 0.0);
                    done = due;
                    fill(&mut r.lock().unwrap(), rate, 2, &mut buf);
                }
            })
            .expect("spawn virtual audio thread");
        Self {
            _stream: None,
            ring,
            device_rate,
            channels: 2,
            device_name: format!("virtual ({} ms callbacks)", period.as_millis()),
        }
    }

    /// Queues interleaved L/R samples at `rate` Hz (the AI rate).
    pub fn push(&self, interleaved: &[i16], rate: u32) {
        self.ring.lock().unwrap().push(interleaved, rate);
    }

    /// Raw counters, for periodic reports (`PW64_AUDIO_STATS`).
    pub fn counters(&self) -> OutputCounters {
        let r = self.ring.lock().unwrap();
        OutputCounters {
            pushed: r.pushed,
            consumed: r.consumed,
            dropped: r.dropped,
            underruns: r.underruns,
            silent_ms: r.silent as f64 * 1000.0 / self.device_rate.max(1) as f64,
            fill_ms: r.q.len() as f64 * 1000.0 / r.src_rate.max(1.0),
            adjust: r.adjust,
        }
    }

    /// Frames pushed / dropped (overflow), underrun count, current fill.
    pub fn stats(&self) -> String {
        let r = self.ring.lock().unwrap();
        let ms = |frames: f64, rate: f64| frames * 1000.0 / rate.max(1.0);
        format!(
            "pushed {} frames, dropped {}, underruns {} ({:.0} ms silence), fill {} (avg {:.0}, min {:.1} ms, target {})",
            r.pushed,
            r.dropped,
            r.underruns,
            ms(r.silent as f64, self.device_rate as f64),
            r.q.len(),
            r.fill_avg,
            if r.min_fill == usize::MAX {
                0.0
            } else {
                ms(r.min_fill as f64, r.src_rate)
            },
            r.target()
        )
    }
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    ring: Arc<Mutex<Ring>>,
) -> Result<cpal::Stream, String>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = config.channels as usize;
    let device_rate = config.sample_rate as f64;
    device
        .build_output_stream::<T, _, _>(
            *config,
            move |data: &mut [T], _| fill(&mut ring.lock().unwrap(), device_rate, channels, data),
            |e| eprintln!("[audio] stream error: {e}"),
            None,
        )
        .map_err(|e| format!("build stream: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring() -> Ring {
        Ring::new(1000.0)
    }

    #[test]
    fn primes_then_interpolates_then_underruns() {
        let mut r = ring();
        // target = 60 frames at 1 kHz.
        let pcm: Vec<i16> = (0..60).flat_map(|i| [i * 100, -i * 100]).collect();
        r.push(&pcm[..20], 1000);
        assert_eq!(r.pull(0.5), [0.0; 2]); // not primed yet
        r.push(&pcm[20..], 1000);
        let [l, _] = r.pull(0.5); // frame 1 (frame 0 is the cubic's lead-in)
        assert!((l - 100.0 / 32768.0).abs() < 1e-6);
        // Halfway between frames 1 and 2 (a ramp stays linear).
        let [l, rr] = r.pull(0.5);
        assert!((l - 150.0 / 32768.0).abs() < 1e-6 && (rr + 150.0 / 32768.0).abs() < 1e-6);
        while r.playing {
            r.pull(1.0);
        }
        assert_eq!(r.underruns, 1);
    }

    #[test]
    fn overflow_drops_to_target() {
        let mut r = ring();
        r.push(&vec![0i16; 2 * 300], 1000); // > 250 ms
        assert_eq!(r.q.len(), 60);
        assert_eq!(r.dropped, 240);
    }
}
