//! CPU count register and timers: `osGetCount`, `osGetTime`, `osSetTimer`,
//! `osStopTimer` (libultra os/{getcount,gettime,settimer,stoptimer}.c).
//!
//! The count runs at the real VR4300 rate, 46.875 MHz (`OS_CPU_COUNTER`).
//! Note the game's own `UV_CLK_TICK_FREQ` (uv_clocks.h) assumes ~45.75 MHz,
//! so on hardware its clock ran 2.4% fast; we keep hardware behaviour.
//! Counts come from the host monotonic clock plus the idle time the host
//! loop skipped (non-throttled runs). Wall-time jumps over 250 ms between
//! reads (system sleep/resume, a stalled host) are absorbed as paused time
//! (`STALL_COUNTS`), like the settings pause.

use super::{Kernel, mesg::OSMesgQueue, with};
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// `OS_CPU_COUNTER`: CP0 count increments per second (93.75 MHz / 2).
pub const COUNT_RATE: u64 = 46_875_000;

/// Wall-time jumps longer than this between consecutive reads never happen
/// on a live frame (a retrace is ~17 ms): they are host stalls (system
/// sleep/resume, a debugger or the OS freezing the process). Such a jump is
/// treated as paused time, like the settings pause, instead of letting the
/// count jump ahead: across a 32-bit wrap it lands `uvClkUpdate`'s clocks
/// tens of seconds off (decomp src/kernel/clocks.c).
const STALL_COUNTS: u64 = COUNT_RATE / 4; // 250 ms

pub fn counts_to_duration(counts: u64) -> Duration {
    Duration::from_nanos((counts as u128 * 1_000_000_000 / COUNT_RATE as u128) as u64)
}

pub(crate) struct Clock {
    start: Instant,
    /// Idle time jumped over (non-throttled runs): added to the wall count.
    skipped: u64,
    /// Wall time spent paused (settings overlay): subtracted from it.
    paused: u64,
    /// While paused: the count at the freeze. No count, timer or retrace
    /// advances during the pause, and on resume the count continues from
    /// here (no burst).
    frozen: Option<u64>,
    /// Wall count at the previous `now` read (`u64::MAX` before the first):
    /// the baseline for stall detection ([`STALL_COUNTS`]).
    last_read: AtomicU64,
    /// Counts absorbed as paused wall time by stall detection so far.
    stalled: AtomicU64,
}

impl Clock {
    pub(crate) fn new() -> Self {
        Self {
            start: Instant::now(),
            skipped: 0,
            paused: 0,
            frozen: None,
            last_read: AtomicU64::new(u64::MAX),
            stalled: AtomicU64::new(0),
        }
    }

    /// Host wall time since boot, in counts.
    fn wall(&self) -> u64 {
        (self.start.elapsed().as_nanos() * COUNT_RATE as u128 / 1_000_000_000) as u64
    }

    /// 64-bit count since boot (`osGetTime`).
    pub(crate) fn now(&self) -> u64 {
        let wall = self.wall();
        if let Some(frozen) = self.frozen {
            // No advance while frozen, but keep the stall baseline current:
            // the reads after a resume must not see the paused wall time as
            // a stall on top of what `unfreeze` already absorbed.
            self.last_read.store(wall, Ordering::Relaxed);
            return frozen;
        }
        let last = self.last_read.swap(wall, Ordering::Relaxed);
        if last != u64::MAX {
            let jump = wall.saturating_sub(last);
            if jump > STALL_COUNTS {
                self.stalled.fetch_add(jump, Ordering::Relaxed);
            }
        }
        // `paused + stalled` <= wall + skipped: each absorption moves both
        // sides by the same amount, so the count never goes backwards.
        wall + self.skipped - self.paused - self.stalled.load(Ordering::Relaxed)
    }

    /// Jumps forward (non-throttled idle).
    pub(crate) fn skip(&mut self, counts: u64) {
        self.skipped += counts;
        if let Some(f) = &mut self.frozen {
            *f += counts;
        }
    }

    /// Freezes the count at the current value (settings-overlay pause).
    /// Calling twice is a no-op until [`Self::unfreeze`].
    pub(crate) fn freeze(&mut self) {
        if self.frozen.is_none() {
            self.frozen = Some(self.now());
        }
    }

    /// Resumes exactly at the frozen count: the wall time spent frozen never
    /// reaches the game's clock, so timers and retraces continue where they
    /// left off (no burst).
    pub(crate) fn unfreeze(&mut self) {
        if let Some(f) = self.frozen.take() {
            let wall = self.wall();
            // f was wall + skipped - paused - stalled at an earlier wall: no
            // underflow. The stall baseline moves here too: the paused time
            // was already absorbed by this recompute, not by stall detection.
            self.paused = wall + self.skipped - f - self.stalled.load(Ordering::Relaxed);
            self.last_read.store(wall, Ordering::Relaxed);
        }
    }
}

pub(crate) struct Timer {
    key: usize,
    pub(crate) due: u64,
    interval: u64,
    mq: usize,
    msg: usize,
}

/// Fires due timers (libultra `__osTimerInterrupt`): posts without blocking,
/// then re-arms periodic ones.
pub(crate) fn fire_timers(k: &mut Kernel, now: u64) {
    let mut i = 0;
    while i < k.timers.len() {
        if k.timers[i].due > now {
            i += 1;
            continue;
        }
        let (mq, msg) = (k.timers[i].mq, k.timers[i].msg);
        if mq != 0 {
            k.post(mq, msg);
        }
        let t = &mut k.timers[i];
        if t.interval != 0 {
            t.due += t.interval;
            i += 1;
        } else {
            k.timers.swap_remove(i);
        }
    }
}

/// Mirrors `osGetCount`: the low 32 bits of the count.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn osGetCount() -> u32 {
    with(|k| k.clock.now() as u32)
}

/// Mirrors `osGetTime`.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn osGetTime() -> u64 {
    with(|k| k.clock.now())
}

/// Mirrors `osSetTimer`: after `countdown` counts (or `interval` if
/// countdown is 0) post `msg` to `mq`, then every `interval` counts if non-zero.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn osSetTimer(
    t: *mut c_void,
    countdown: u64,
    interval: u64,
    mq: *mut OSMesgQueue,
    msg: *mut c_void,
) -> i32 {
    with(|k| {
        let key = t as usize;
        k.timers.retain(|x| x.key != key);
        let first = if countdown != 0 { countdown } else { interval };
        let due = k.clock.now() + first;
        k.timers.push(Timer {
            key,
            due,
            interval,
            mq: mq as usize,
            msg: msg as usize,
        });
    });
    0
}

/// Mirrors `osStopTimer`: -1 if the timer was not active.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn osStopTimer(t: *mut c_void) -> i32 {
    with(|k| {
        let n = k.timers.len();
        k.timers.retain(|x| x.key != t as usize);
        if k.timers.len() == n { -1 } else { 0 }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The settings-overlay pause: a frozen clock doesn't advance, and on
    /// resume it continues from the frozen count (no timer/retrace burst).
    #[test]
    fn freeze_absorbs_wall_time() {
        let ms = |n: u64| COUNT_RATE / 1000 * n;
        let mut c = Clock::new();
        std::thread::sleep(Duration::from_millis(5));
        c.skip(ms(100));
        let t0 = c.now();
        assert!(t0 >= ms(105), "wall time and skipped idle both count");
        c.freeze();
        std::thread::sleep(Duration::from_millis(40));
        let frozen = c.now();
        assert!(frozen >= t0 && frozen < t0 + ms(20));
        std::thread::sleep(Duration::from_millis(10));
        assert_eq!(c.now(), frozen, "clock must not advance while frozen");
        c.unfreeze();
        // Resumes from the frozen count: the 50 ms paused never reach it.
        let r = c.now();
        assert!(r >= frozen && r < frozen + ms(20), "{r} vs {frozen}");
        // Twice more (the paused offset accumulates), incl. a skip while frozen.
        for _ in 0..2 {
            c.freeze();
            let f = c.now();
            std::thread::sleep(Duration::from_millis(30));
            c.skip(ms(1));
            assert_eq!(c.now(), f + ms(1));
            c.unfreeze();
            let r = c.now();
            assert!(r >= f + ms(1) && r < f + ms(21), "{r} vs {f}");
        }
    }

    /// P5: a host stall longer than the 250 ms threshold (system
    /// sleep/resume, a debugger stop) is paused time: the count continues
    /// where it was, so the 32-bit count and uvClkUpdate never see the jump.
    #[test]
    fn wall_stall_is_absorbed() {
        let ms = |n: u64| COUNT_RATE / 1000 * n;
        let mut c = Clock::new();
        let t0 = c.now();
        // One read far above the threshold: absorbed, the count stays put.
        std::thread::sleep(Duration::from_millis(300));
        let t1 = c.now();
        assert!(t1 >= t0, "count never goes backwards");
        assert!(t1 - t0 < ms(10), "stall absorbed: {t0} -> {t1}");
        // Reads under the threshold advance in real time again.
        std::thread::sleep(Duration::from_millis(10));
        let t2 = c.now();
        assert!(t2 - t1 >= ms(8), "post-stall reads advance: {t1} -> {t2}");
        // A later freeze/unfreeze still lands on the same count: the stall
        // did not corrupt the paused bookkeeping.
        c.freeze();
        let f = c.now();
        std::thread::sleep(Duration::from_millis(10));
        c.unfreeze();
        let t3 = c.now();
        assert!(t3 >= f && t3 < f + ms(20), "{t3} vs {f}");
    }
}
