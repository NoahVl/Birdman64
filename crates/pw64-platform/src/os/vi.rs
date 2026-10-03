//! Minimal VI manager: retrace timing, `osViSetEvent`, framebuffer latching,
//! and the host **present tick** (framerate.md "Design"): with a present
//! rate set ([`super::set_present_rate`]), a swap latches at the next present
//! tick instead of the next retrace, and the tick posts the scheduler's
//! present message (`PW64_PRESENT_MSG`, sched.c.patch) so the game's frame
//! loop runs at the display rate. VI retraces stay 60 Hz (clock, audio,
//! input scripts). Presenting frames: [`super::set_retrace_hook`] /
//! [`super::set_present_hook`].

use super::{Kernel, mesg::OSMesgQueue, time::COUNT_RATE, with};
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};

/// NTSC field rate, as the game selects `OS_VI_NTSC_LAN1` (osTvType = NTSC).
pub const RETRACE_HZ: u64 = 60;
pub const RETRACE_COUNTS: u64 = COUNT_RATE / RETRACE_HZ;
/// Shortest present-tick interval (2 ms = 500 Hz): keeps the game's frame
/// time ≥ its `PW64_DT_MIN` (0.002 s, pw64_rate.h) when uncapped.
pub const MIN_PRESENT_COUNTS: u64 = COUNT_RATE / 500;

/// Presented frames, incremented by the window thread right after each
/// successful `present()` (`crates/birdman64/src/window.rs`): the OS core's
/// `Display` pacing ticks when this changes (V-Sync). Read only here.
pub static VBLANK: AtomicU64 = AtomicU64::new(0);

/// Passed to the retrace hook.
#[derive(Clone, Copy, Debug)]
pub struct Retrace {
    /// Retraces since boot (1-based).
    pub number: u64,
    /// Framebuffer being scanned out from this retrace on (N64 address).
    pub framebuffer: usize,
    /// `osViBlack(TRUE)` in effect.
    pub black: bool,
}

/// Passed to the present hook: a present tick just latched `framebuffer`.
#[derive(Clone, Copy, Debug)]
pub struct Present {
    /// Present ticks since boot (1-based).
    pub number: u64,
    /// Framebuffer shown from this tick on (N64 address).
    pub framebuffer: usize,
    /// `osViBlack(TRUE)` in effect.
    pub black: bool,
}

/// How often the present tick fires.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PresentRate {
    /// Every `interval` counts (≥ [`MIN_PRESENT_COUNTS`]).
    Fixed { interval: u64 },
    /// As soon as a swap is pending (the previous frame's gfx task is done),
    /// at most every [`MIN_PRESENT_COUNTS`]; with no swap pending, once per
    /// retrace interval as a keep-alive.
    Uncapped,
    /// Display-paced (V-Sync): a tick when the window presented ([`VBLANK`]
    /// changed since the last tick) — the tick can never beat the actual
    /// vblank, so no frame is repeated or dropped. If the counter goes
    /// silent for `2 * fallback_interval` counts (headless, minimised,
    /// window dumps), ticks fire like [`PresentRate::Fixed`] at
    /// `fallback_interval` would, keeping those runs deterministic.
    Display { fallback_interval: u64 },
}

impl PresentRate {
    /// Fixed/display interval from a refresh rate in millihertz
    /// (e.g. 143_998), clamped to [`MIN_PRESENT_COUNTS`].
    fn interval_of(mhz: u32) -> u64 {
        (COUNT_RATE * 1000 / u64::from(mhz.max(1))).max(MIN_PRESENT_COUNTS)
    }

    /// Fixed rate from a refresh rate in millihertz (e.g. 143_998).
    pub fn from_millihertz(mhz: u32) -> Self {
        Self::Fixed {
            interval: Self::interval_of(mhz),
        }
    }

    /// Display-paced rate (V-Sync) with a fallback at the monitor's interval.
    pub fn display_from_millihertz(mhz: u32) -> Self {
        Self::Display {
            fallback_interval: Self::interval_of(mhz),
        }
    }
}

#[derive(Default)]
pub(crate) struct ViState {
    /// `osViSetEvent` queue/message/retrace interval.
    event: Option<(usize, usize, u64)>,
    next_due: u64,
    pub(crate) retraces: u64,
    current_fb: usize,
    next_fb: usize,
    black: bool,
    mode: usize,
    /// Host-selected present rate (None: swaps latch at retraces, as on HW).
    pub(crate) present_rate: Option<PresentRate>,
    /// `pw64_present_set_event` (sched.c.patch): queue + message.
    present_event: Option<(usize, usize)>,
    /// Fixed rate: count at which the next tick is due.
    pub(crate) present_next: u64,
    /// Display rate: the `VBLANK` value seen at the last tick (0 at boot;
    /// a change means the window presented and a tick is due now).
    pub(crate) display_seen: u64,
    /// Count of the last tick (uncapped pacing).
    last_present: u64,
    pub(crate) presents: u64,
}

impl ViState {
    /// Count of the next retrace or present tick.
    pub(crate) fn next_due(&self) -> Option<u64> {
        let r = self.event.map(|_| self.next_due);
        match (r, self.present_due()) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Present ticks drive the swap latch (rate set and the scheduler has
    /// registered its present message).
    pub(crate) fn present_active(&self) -> bool {
        self.present_rate.is_some() && self.present_event.is_some()
    }

    /// Display pacing active: the [`VBLANK`] value seen at the last tick, so
    /// the host's idle wait can end as soon as the window presents.
    pub(crate) fn display_wait(&self) -> Option<u64> {
        (self.present_active() && matches!(self.present_rate, Some(PresentRate::Display { .. })))
            .then_some(self.display_seen)
    }

    fn present_due(&self) -> Option<u64> {
        if !self.present_active() {
            return None;
        }
        Some(match self.present_rate? {
            PresentRate::Fixed { .. } => self.present_next,
            PresentRate::Uncapped if self.next_fb != self.current_fb => {
                self.last_present + MIN_PRESENT_COUNTS
            }
            PresentRate::Uncapped => self.last_present + RETRACE_COUNTS,
            // Display: a tick as soon as the window presented again; while
            // the counter is silent, the fallback deadline (`present_next`,
            // see the state update in `present_due` below).
            PresentRate::Display { .. } => {
                if VBLANK.load(Ordering::Relaxed) != self.display_seen {
                    0
                } else {
                    self.present_next
                }
            }
        })
    }
}

/// Sleeps up to `d`, returning early once [`VBLANK`] differs from `seen`
/// (polled every 0.5 ms: the window thread has no handle to wake us, and
/// `park_timeout` is millisecond-granular on Windows).
pub(crate) fn sleep_until_vblank(d: std::time::Duration, seen: u64) {
    let end = std::time::Instant::now() + d;
    while VBLANK.load(Ordering::Relaxed) == seen {
        let left = end.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            break;
        }
        std::thread::sleep(left.min(std::time::Duration::from_micros(500)));
    }
}

/// Performs every retrace due by `now`: latches the swapped framebuffer
/// (unless present ticks do that) and posts the `osViSetEvent` message every
/// `retraceCount` fields.
pub(crate) fn retraces_due(k: &mut Kernel, now: u64) -> Vec<Retrace> {
    let mut out = Vec::new();
    let Some((mq, msg, every)) = k.vi.event else {
        return out;
    };
    // After a long stall (debugger, slow frame) don't replay a burst.
    if now > k.vi.next_due + 4 * RETRACE_COUNTS {
        k.vi.next_due = now;
    }
    let latch = !k.vi.present_active();
    while k.vi.next_due <= now {
        k.vi.next_due += RETRACE_COUNTS;
        k.vi.retraces += 1;
        if latch {
            k.vi.current_fb = k.vi.next_fb;
        }
        if k.vi.retraces.is_multiple_of(every.max(1)) {
            k.post(mq, msg);
        }
        out.push(Retrace {
            number: k.vi.retraces,
            framebuffer: k.vi.current_fb,
            black: k.vi.black,
        });
    }
    out
}

/// Fires the present tick if due by `now` (at most one per call: a backlog
/// of ticks would only latch the same swap again): latch the pending swap,
/// then post the scheduler's present message.
pub(crate) fn present_due(k: &mut Kernel, now: u64) -> Option<Present> {
    let due = k.vi.present_due()?;
    if due > now {
        return None;
    }
    let (mq, msg) = k.vi.present_event?;
    if let Some(PresentRate::Fixed { interval }) = k.vi.present_rate {
        // Keep the phase; after a stall (slow frame, debugger) resync
        // instead of firing a burst.
        k.vi.present_next += interval;
        if k.vi.present_next <= now {
            k.vi.present_next = now + interval;
        }
    }
    if let Some(PresentRate::Display { fallback_interval }) = k.vi.present_rate {
        let v = VBLANK.load(Ordering::Relaxed);
        let changed = v != k.vi.display_seen;
        k.vi.display_seen = v;
        if changed {
            // Display pacing: the fallback only takes over once the window
            // has been silent for two fallback intervals.
            k.vi.present_next = now + 2 * fallback_interval;
        } else {
            // Fallback cadence: like `Fixed { interval: fallback_interval }`
            // (phase kept, resync after a stall).
            k.vi.present_next += fallback_interval;
            if k.vi.present_next <= now {
                k.vi.present_next = now + fallback_interval;
            }
        }
    }
    k.vi.last_present = now;
    k.vi.presents += 1;
    k.vi.current_fb = k.vi.next_fb;
    k.post(mq, msg);
    Some(Present {
        number: k.vi.presents,
        framebuffer: k.vi.current_fb,
        black: k.vi.black,
    })
}

/// Registers the scheduler's present message (sched.c.patch, from
/// `_uvScCreateScheduler`): posted to `mq` at every present tick while a
/// present rate is set.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn pw64_present_set_event(mq: *mut OSMesgQueue, msg: *mut c_void) {
    with(|k| {
        k.vi.present_event = Some((mq as usize, msg as usize));
        let now = k.clock.now();
        k.vi.last_present = now;
        if let Some(PresentRate::Fixed { interval }) = k.vi.present_rate {
            k.vi.present_next = now + interval;
        }
    });
}

/// Whether present ticks (not retraces) latch swaps and run gfx: the
/// patched `_uvScHandleRetrace` then leaves the swap flip to
/// `_uvScHandlePresent`.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn pw64_present_active() -> i32 {
    with(|k| k.vi.present_active()) as i32
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn osCreateViManager(_pri: i32) {}

/// Records the mode pointer (an `osViModeTable` entry); not interpreted yet.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn osViSetMode(mode: *mut c_void) {
    with(|k| k.vi.mode = mode as usize);
}

/// Mirrors `osViSetEvent`: post `msg` to `mq` every `retrace_count` retraces.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn osViSetEvent(mq: *mut OSMesgQueue, msg: *mut c_void, retrace_count: u32) {
    with(|k| {
        if k.vi.event.is_none() {
            k.vi.next_due = k.clock.now() + RETRACE_COUNTS;
        }
        k.vi.event = Some((mq as usize, msg as usize, retrace_count as u64));
    });
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn osViSetSpecialFeatures(_features: u32) {}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn osViBlack(active: u8) {
    with(|k| k.vi.black = active != 0);
}

/// Takes effect at the next retrace (or present tick), like the hardware.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn osViSwapBuffer(fb: *mut c_void) {
    with(|k| k.vi.next_fb = fb as usize);
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn osViGetCurrentFramebuffer() -> *mut c_void {
    with(|k| k.vi.current_fb as *mut c_void)
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn osViGetNextFramebuffer() -> *mut c_void {
    with(|k| k.vi.next_fb as *mut c_void)
}

/// The `osViSetMode` pointer (N64 `OSViMode*`), 0 if unset.
pub fn current_mode() -> usize {
    with(|k| k.vi.mode)
}
