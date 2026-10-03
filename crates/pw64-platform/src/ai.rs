//! AI (audio interface): `osAiSetFrequency`, `osAiSetNextBuffer`,
//! `osAiGetLength` over an emulated DAC.
//!
//! The DAC drains its 2-entry DMA FIFO at the programmed rate against the
//! OS clock (`osGetTime`: host time, plus skipped idle when unthrottled), so
//! `osAiGetLength` reports the real remaining length of the current buffer
//! and the game's `__amHandleFrameMsg` pacing (frame = rate/60 − left + 100)
//! works as on hardware. Queued samples are handed to a host sink at submit
//! time (`set_ai_sink`; the `pw64` exe feeds `pw64-audio`'s output + WAV
//! dump). Without a sink they are discarded; timing is unaffected.
//!
//! Late submits: the game keeps only ~100 samples (4.5 ms) of lead, which
//! is enough on hardware because the audio thread preempts everything on
//! the retrace interrupt. The cooperative OS core only delivers retraces at
//! OS calls, so a long host stretch (level loading, PNG dumps, the host OS
//! descheduling us) can run the DAC dry. Two wrong fixes:
//! - restart the DAC at `now` (what HW would do): that time is lost for
//!   good, the game makes fewer samples than the host device plays, and
//!   the output ring underruns;
//! - keep the clock continuous (the late buffer counts as partly played):
//!   the game's pacing (`frame = 468 − left`, `left` = the *front* buffer)
//!   then settles into a one-buffer-deep equilibrium with every submit
//!   ~11 ms late and zero lead, forever (2/3 of buffers "late").
//!
//! (Merely pausing the DAC, as HW would, is no better: `left` = the whole
//! new buffer → minimum 352-sample frames → starved every frame again.)
//!
//! So the DAC runs on its own clock, `OS time − lag`. A late submit (lag
//! ≤ [`MAX_CATCH_UP`]) rewinds it to [`LEAD_FRAMES`] before the last buffer
//! ended and replays that tail, so the game sees exactly its on-time state
//! (tail of ~100 frames playing, new buffer queued) and keeps its 2-deep
//! pacing. The lag (gap + lead) is repaid by running the DAC
//! [`REPAY_DIV`]⁻¹ fast, which the game follows with bigger frames. Only
//! the pacing clock moves: every sample still reaches the sink once, and
//! the stream stays in step with the host clock. Longer gaps (audio really
//! stopped) restart the clock.

use crate::os::time::{COUNT_RATE, osGetTime};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::ffi::c_void;

/// `osViClock` (NTSC VI clock, Hz), the AI DAC's input clock.
pub const VI_NTSC_CLOCK: u64 = 48_681_812;

/// Most DAC lag (OS counts, 150 ms) that is repaid instead of lost (module
/// doc). Beyond the host output ring's 60 ms target: the ring refills as
/// the lag is repaid.
pub const MAX_CATCH_UP: u64 = COUNT_RATE * 3 / 20;

/// The lead the game keeps at each retrace (`EXTRA_SAMPLES`, audio_manager.c).
pub const LEAD_FRAMES: u64 = 100;

/// While lagging, the DAC clock runs `1 + 1/REPAY_DIV` fast (10%: 100 ms
/// repaid in 1 s; the game can make up to 480 samples/frame vs 368, +30%).
pub const REPAY_DIV: u64 = 10;

type Sink = Box<dyn FnMut(&[i16], u32)>;

struct Ai {
    /// `AI_DACRATE` divider (+1): rate = VI clock / dac_rate.
    dac_rate: u64,
    /// Queued buffers, in stereo frames; the front one is playing.
    fifo: VecDeque<u64>,
    /// DAC time (see `dac_time`) at which the front buffer started.
    start: u64,
    /// DAC lag behind the OS clock at OS count `lag_at` (module doc).
    lag: u64,
    lag_at: u64,
    stats: AiStats,
}

/// DAC health counters (`ai_stats`).
#[derive(Clone, Copy, Debug, Default)]
pub struct AiStats {
    /// Buffers accepted.
    pub buffers: u64,
    /// Buffers queued after the DAC ran dry (late submits, after the first).
    pub starved: u64,
    /// Longest such gap, in stereo frames.
    pub max_gap_frames: u64,
    /// Gaps longer than [`MAX_CATCH_UP`]: the clock restarted (real silence).
    pub restarts: u64,
    /// Buffers rejected because both FIFO slots were busy (a skip on HW).
    pub lost: u64,
}

thread_local! {
    static AI: RefCell<Ai> = const {
        RefCell::new(Ai {
            dac_rate: 2208,
            fifo: VecDeque::new(),
            start: 0,
            lag: 0,
            lag_at: 0,
            stats: AiStats { buffers: 0, starved: 0, max_gap_frames: 0, restarts: 0, lost: 0 },
        })
    };
    static SINK: RefCell<Option<Sink>> = const { RefCell::new(None) };
}

/// Receives each buffer the game queues: interleaved L/R host-endian s16
/// and the AI rate in Hz (rounded down, as `osAiSetFrequency` returns it).
pub fn set_ai_sink(sink: impl FnMut(&[i16], u32) + 'static) {
    SINK.with(|s| *s.borrow_mut() = Some(Box::new(sink)));
}

impl Ai {
    /// OS counts to play `frames` stereo frames.
    fn counts(&self, frames: u64) -> u64 {
        frames * self.dac_rate * COUNT_RATE / VI_NTSC_CLOCK
    }

    /// Lag still unpaid at OS count `now` (repaid at 1/REPAY_DIV).
    fn lag(&self, now: u64) -> u64 {
        self.lag
            .saturating_sub(now.saturating_sub(self.lag_at) / REPAY_DIV)
    }

    /// The DAC's clock at OS count `now`: it runs 10% fast while repaying,
    /// and only steps back at a late submit (the rewind, with the FIFO
    /// empty, so nothing queued is un-played).
    fn dac_time(&self, now: u64) -> u64 {
        now - self.lag(now)
    }

    /// Retires finished buffers; the next one starts where the last ended.
    fn update(&mut self, now: u64) {
        let now = self.dac_time(now);
        while let Some(&f) = self.fifo.front() {
            let end = self.start + self.counts(f);
            if now < end {
                break;
            }
            self.fifo.pop_front();
            self.start = end;
        }
    }

    fn remaining_bytes(&mut self, now: u64) -> u32 {
        self.update(now);
        let Some(&f) = self.fifo.front() else {
            return 0;
        };
        let now = self.dac_time(now);
        // Rounded to nearest so `counts` round-trips (whole frames).
        let div = self.dac_rate * COUNT_RATE;
        let played = ((now - self.start) * VI_NTSC_CLOCK + div / 2) / div;
        (f.saturating_sub(played) * 4) as u32
    }

    /// Queues a buffer of `frames` stereo frames; false if the FIFO is full.
    fn submit(&mut self, now: u64, frames: u64) -> bool {
        self.update(now);
        if self.fifo.len() >= 2 {
            self.stats.lost += 1;
            return false;
        }
        let dac = self.dac_time(now);
        if self.fifo.is_empty() {
            // `start` is where the last buffer ended (`update`).
            let gap = dac.saturating_sub(self.start);
            let lag = self.lag(now) + gap;
            if self.stats.buffers == 0 {
                self.start = dac;
            } else if gap > 0 {
                self.stats.starved += 1;
                // u128: an hours-long gap × the VI clock overflows u64.
                let gap_frames = (gap as u128 * VI_NTSC_CLOCK as u128
                    / (self.dac_rate * COUNT_RATE) as u128) as u64;
                self.stats.max_gap_frames = self.stats.max_gap_frames.max(gap_frames);
                let lag = lag + self.counts(LEAD_FRAMES);
                if lag <= MAX_CATCH_UP && self.start >= self.counts(LEAD_FRAMES) {
                    // Late submit: rewind the DAC to LEAD_FRAMES before the
                    // last buffer ended and replay that tail, so the game
                    // sees its normal on-time state; repay later (module doc).
                    (self.lag, self.lag_at) = (lag, now);
                    self.start = self.dac_time(now);
                    self.fifo.push_back(LEAD_FRAMES);
                } else {
                    // Audio really stopped: that time is lost, and so is
                    // any unpaid lag (the host ring re-primes after its
                    // underrun; repaying on would only overfill it).
                    self.stats.restarts += 1;
                    (self.lag, self.lag_at) = (0, now);
                    self.start = now;
                }
            }
        }
        self.stats.buffers += 1;
        self.fifo.push_back(frames);
        true
    }
}

/// Mirrors `osAiSetFrequency`: the DAC divider is `round(clock / freq)`;
/// returns the rate actually produced (22050 → 22047).
#[unsafe(no_mangle)]
pub extern "C-unwind" fn osAiSetFrequency(freq: u32) -> i32 {
    let dac = ((VI_NTSC_CLOCK as f64 / freq.max(1) as f64) + 0.5) as u64;
    if dac < 132 {
        return -1; // AI_MIN_DAC_RATE
    }
    AI.with(|a| a.borrow_mut().dac_rate = dac);
    (VI_NTSC_CLOCK / dac) as i32
}

/// Mirrors `osAiSetNextBuffer`: -1 if both FIFO slots are busy (the
/// buffer is lost, as on hardware), else queue it.
///
/// # Safety
/// `buf` must be readable for `n` bytes (the audio DMA drains it).
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn osAiSetNextBuffer(buf: *mut c_void, n: u32) -> i32 {
    let now = osGetTime();
    let (accepted, rate) = AI.with(|a| {
        let mut a = a.borrow_mut();
        let ok = a.submit(now, n as u64 / 4);
        (ok, (VI_NTSC_CLOCK / a.dac_rate) as u32)
    });
    if !accepted {
        return -1;
    }
    SINK.with(|s| {
        if let Some(sink) = s.borrow_mut().as_mut() {
            // SAFETY: the C passes a buffer of `n` bytes of s16 samples
            // (`AudioInfo.data`, 8-byte aligned heap memory).
            let pcm = unsafe { std::slice::from_raw_parts(buf as *const i16, n as usize / 2) };
            sink(pcm, rate);
        }
    });
    0
}

/// DAC health counters so far (this thread's AI).
pub fn ai_stats() -> AiStats {
    AI.with(|a| a.borrow().stats)
}

/// Mirrors `osAiGetLength`: bytes left in the buffer being played.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn osAiGetLength() -> u32 {
    let now = osGetTime();
    AI.with(|a| a.borrow_mut().remaining_bytes(now))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fifo_drains_at_dac_rate() {
        let mut a = Ai {
            dac_rate: 2208,
            fifo: VecDeque::from([400, 400]),
            start: 1000,
            lag: 0,
            lag_at: 0,
            stats: AiStats::default(),
        };
        assert_eq!(a.remaining_bytes(1000), 1600);
        let half = a.counts(200);
        assert_eq!(a.remaining_bytes(1000 + half), 800);
        // First buffer done: the second plays from its end, not from `now`.
        let t = 1000 + a.counts(400) + a.counts(100);
        assert_eq!(a.remaining_bytes(t), 1200);
        assert_eq!(a.fifo.len(), 1);
        assert_eq!(a.remaining_bytes(t + a.counts(1000)), 0);
        assert!(a.fifo.is_empty());
    }

    #[test]
    fn late_submit_rewinds_then_repays_long_gap_restarts() {
        let mut a = Ai {
            dac_rate: 2208,
            fifo: VecDeque::new(),
            start: 0,
            lag: 0,
            lag_at: 0,
            stats: AiStats::default(),
        };
        assert!(a.submit(1000, 400));
        // Submitted 100 frames after the first buffer ran out: the DAC
        // rewinds to LEAD_FRAMES before its end and replays that tail.
        let t = 1000 + a.counts(400) + a.counts(100);
        assert!(a.submit(t, 400));
        assert_eq!(a.remaining_bytes(t), LEAD_FRAMES as u32 * 4);
        assert_eq!(a.fifo.len(), 2);
        assert_eq!((a.stats.starved, a.stats.restarts), (1, 0));
        // Repaying: 100 frames of host time play 110 (tail + 10).
        assert_eq!(a.remaining_bytes(t + a.counts(100)), 390 * 4);
        // Fully repaid after 10× the lag: the DAC is back on the OS clock,
        // and the real buffers played back to back (nothing lost).
        let t1 = t + a.counts(2500);
        assert_eq!(a.lag(t1), 0);
        assert_eq!(a.dac_time(t1), t1);
        a.update(t1);
        assert!(a.fifo.is_empty() && a.start == 1000 + a.counts(400) + a.counts(400));
        // A gap beyond MAX_CATCH_UP restarts the clock at `now`.
        let t2 = t1 + MAX_CATCH_UP + 1;
        assert!(a.submit(t2, 400));
        assert_eq!(a.remaining_bytes(t2), 400 * 4);
        assert_eq!((a.stats.starved, a.stats.restarts), (2, 1));
        // Both FIFO slots busy: rejected.
        assert!(a.submit(t2, 400));
        assert!(!a.submit(t2, 400));
        assert_eq!(a.stats.lost, 1);
    }

    /// A restart while still repaying forgives the unpaid lag: the DAC is
    /// back on the OS clock and the new buffer plays in full from `now`.
    #[test]
    fn restart_while_repaying_drops_the_lag() {
        let mut a = Ai {
            dac_rate: 2208,
            fifo: VecDeque::new(),
            start: 0,
            lag: 0,
            lag_at: 0,
            stats: AiStats::default(),
        };
        assert!(a.submit(1000, 400));
        let t = 1000 + a.counts(400) + a.counts(1000); // late: ~45 ms lag
        assert!(a.submit(t, 400));
        assert!(a.lag(t) > 0);
        // Everything played out, then a gap that pushes the lag past the cap.
        let t2 = t + a.counts(500) + MAX_CATCH_UP;
        assert!(a.submit(t2, 400));
        assert_eq!(a.stats.restarts, 1);
        assert_eq!((a.lag(t2), a.dac_time(t2)), (0, t2));
        assert_eq!(a.remaining_bytes(t2), 400 * 4);
    }

    /// The game's `__amHandleFrameMsg` pacing against the DAC, with a 70 ms
    /// host stall: it must return to its on-time 2-deep state (the old
    /// continuous-clock catch-up stayed late on every frame after a stall).
    #[test]
    fn game_pacing_recovers_after_stall() {
        let mut a = Ai {
            dac_rate: 2208,
            fifo: VecDeque::new(),
            start: 0,
            lag: 0,
            lag_at: 0,
            stats: AiStats::default(),
        };
        let retrace = COUNT_RATE / 60;
        let (mut next, mut t, mut late_after) = (480u64, 0, 0);
        for k in 1..600u64 {
            t = k * retrace + COUNT_RATE / 1000; // 1 ms after the retrace
            if k == 100 {
                t += COUNT_RATE * 7 / 100;
            } else if (101..105).contains(&k) {
                continue; // retraces swallowed by the stall
            }
            let before = a.stats.starved;
            assert!(a.submit(t, next));
            if k > 110 {
                late_after += a.stats.starved - before;
            }
            let left = a.remaining_bytes(t) as u64 / 4;
            next = ((368 + 100 + 16 - left as i64) as u64 & !0xF).max(352);
        }
        assert_eq!(late_after, 0);
        assert_eq!(a.lag(t), 0);
        assert!(a.stats.starved <= 2, "{:?}", a.stats);
    }

    #[test]
    fn frequency_rounds_like_hardware() {
        assert_eq!(osAiSetFrequency(22050), 22047);
    }
}
