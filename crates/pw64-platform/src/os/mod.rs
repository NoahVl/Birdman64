//! libultra OS core on one host thread (native-build.md §4, task 4).
//!
//! Every `OSThread` is a stackful coroutine (corosensei, [`stack::LowStack`]).
//! Scheduling follows libultra on the single-core VR4300: the highest-priority
//! ready thread runs; equal priorities are FIFO; a thread only loses the CPU
//! when it blocks, yields, stops, lowers its priority, or wakes/starts a
//! higher-priority thread. "Interrupts" (VI retrace, timers, events) are
//! delivered by the host loop ([`run`]) when every thread is blocked, and at
//! OS-call checkpoints ([`checkpoint`]) so a busy thread can still be
//! preempted by the scheduler thread.
//!
//! Priority 0 (`OS_PRIORITY_IDLE`) threads never run: the host loop is the
//! idle thread. That parks the Kernel thread after `osSetThreadPri(NULL, 0)`
//! instead of spinning in its `while (1) {}` (system.c:328).
//!
//! All switches go through the root context: a thread suspends back to
//! [`run`], which resumes the next one.

pub mod mesg;
pub mod misc;
pub mod stack;
pub mod thread;
pub mod time;
pub mod vi;

// Crash tracer (native-build.md §8): vectored exception handler on Windows,
// signal handler on Linux; same `[crash]` report.
#[cfg(windows)]
pub mod crash;
#[cfg(target_os = "linux")]
#[path = "crash_linux.rs"]
pub mod crash;

use corosensei::{Coroutine, CoroutineResult};
use stack::LowStack;
use std::cell::RefCell;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};

/// Settings-overlay pause (`pw64/src/settings.rs`): while set, [`run`] stops
/// the OS — the clock is frozen (threads sit in their waits, no retraces or
/// timers fire) and on resume nothing is replayed. Set only from the window
/// thread while the overlay is open; read here between thread batches.
pub static PAUSED: AtomicBool = AtomicBool::new(false);

/// Pauses / resumes the OS core (the settings overlay).
pub fn set_paused(paused: bool) {
    PAUSED.store(paused, Ordering::Release);
}

pub(crate) type Yielder = corosensei::Yielder<(), ()>;
type Co = Coroutine<(), (), (), LowStack>;

/// `OS_PRIORITY_IDLE`: threads at this priority are never scheduled.
pub const PRI_IDLE: i32 = 0;
/// Priority at which the host runs a started gfx task (HLE of the RSP/RDP,
/// which on hardware run in parallel with the CPU): after every ready thread
/// above it — the scheduler (127), PI manager (150) and audio thread (110),
/// which must queue its AI buffer within ~4.5 ms of the retrace — and before
/// the game threads (app 10). Also before any interrupt delivery or idling.
pub const RSP_PRI: i32 = 100;
/// Number of `OS_EVENT_*` slots (libultra `OS_NUM_EVENTS`).
pub const NUM_EVENTS: usize = 23;
pub const OS_EVENT_SP: usize = 4;
pub const OS_EVENT_SI: usize = 5;
pub const OS_EVENT_DP: usize = 9;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Created or `osStopThread`ed; `osStartThread` makes it ready.
    Stopped,
    Ready,
    Running,
    /// Blocked in `osRecvMesg` on this queue (empty).
    WaitRecv(usize),
    /// Blocked in `osSendMesg` on this queue (full).
    WaitSend(usize),
    /// Entry function returned (libultra: `__osCleanupThread` destroys it).
    Dead,
}

pub(crate) struct Thread {
    /// Address of the C `OSThread` (the handle), or a synthetic key.
    key: usize,
    pub(crate) id: i32,
    pub(crate) pri: i32,
    pub(crate) state: State,
    /// Order stamp: when it became ready / started waiting (FIFO among equals).
    seq: u64,
    /// `None` while running (taken out by [`resume`]) or before creation.
    co: Option<Co>,
    yielder: *const Yielder,
}

pub(crate) struct Kernel {
    threads: Vec<Thread>,
    current: Option<usize>,
    seq: u64,
    /// `osSetEventMesg` table: (queue, message).
    pub(crate) events: [Option<(usize, usize)>; NUM_EVENTS],
    pub(crate) clock: time::Clock,
    pub(crate) timers: Vec<time::Timer>,
    pub(crate) vi: vi::ViState,
    /// `osSetIntMask`; `OS_IM_NONE` (1) blocks checkpoint delivery.
    pub(crate) int_mask: u32,
    /// Messages dropped by `post` (full queue): SP/DP done events lost that
    /// way leave the scheduler's RSP status stuck (diagnostics, §8).
    pub(crate) drops: u64,
    /// Events whose message didn't fit (`post` failed): retried at the next
    /// delivery point. Hardware completion interrupts are level-triggered —
    /// the SP/DP status bit stays set until the scheduler clears it — so a
    /// done event must never be lost (that permanently wedges the game
    /// scheduler's RSP status).
    pub(crate) pending_events: [bool; NUM_EVENTS],
}

impl Kernel {
    fn new() -> Self {
        Self {
            threads: Vec::new(),
            current: None,
            seq: 0,
            events: [None; NUM_EVENTS],
            clock: time::Clock::new(),
            timers: Vec::new(),
            vi: vi::ViState::default(),
            int_mask: misc::OS_IM_ALL,
            drops: 0,
            pending_events: [false; NUM_EVENTS],
        }
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    /// Resolves a thread handle; null means the running thread.
    pub(crate) fn index(&self, key: usize) -> Option<usize> {
        if key == 0 {
            return self.current;
        }
        self.threads.iter().position(|t| t.key == key)
    }

    pub(crate) fn thread_mut(&mut self, idx: usize) -> &mut Thread {
        &mut self.threads[idx]
    }

    pub(crate) fn make_ready(&mut self, idx: usize) {
        let seq = self.next_seq();
        let t = &mut self.threads[idx];
        t.state = State::Ready;
        t.seq = seq;
    }

    /// Puts the running thread into `state` (caller then suspends).
    pub(crate) fn set_current_state(&mut self, state: State) {
        let idx = self
            .current
            .expect("blocking OS call outside a game thread");
        let seq = self.next_seq();
        let t = &mut self.threads[idx];
        t.state = state;
        t.seq = seq;
    }

    /// Wakes the longest-waiting thread of the highest priority in `state`
    /// (libultra keeps `mtqueue`/`fullqueue` priority-ordered, FIFO within).
    pub(crate) fn wake_one(&mut self, state: State) {
        let best = self
            .threads
            .iter()
            .enumerate()
            .filter(|(_, t)| t.state == state)
            .max_by_key(|(_, t)| (t.pri, std::cmp::Reverse(t.seq)))
            .map(|(i, _)| i);
        if let Some(i) = best {
            self.make_ready(i);
        }
    }

    /// Next thread to run: highest priority, FIFO within; never idle priority.
    fn pick(&self) -> Option<usize> {
        self.threads
            .iter()
            .enumerate()
            .filter(|(_, t)| t.state == State::Ready && t.pri > PRI_IDLE)
            .max_by_key(|(_, t)| (t.pri, std::cmp::Reverse(t.seq)))
            .map(|(i, _)| i)
    }

    /// Whether the running thread must give up the CPU now.
    fn should_preempt(&self) -> bool {
        let Some(cur) = self.current else {
            return false;
        };
        let cur_pri = self.threads[cur].pri;
        cur_pri <= PRI_IDLE || self.pick().is_some_and(|i| self.threads[i].pri > cur_pri)
    }

    /// Queues `msg` on `mq` without blocking, waking a receiver. The
    /// interrupt-side send (and `OS_MESG_NOBLOCK`). Returns false if full.
    pub(crate) fn post(&mut self, mq: usize, msg: usize) -> bool {
        // SAFETY: `mq` is an `OSMesgQueue*` the C code created.
        let ok = unsafe { mesg::push_back(mq as *mut mesg::OSMesgQueue, msg as *mut c_void) };
        if ok {
            self.wake_one(State::WaitRecv(mq));
        } else {
            self.drops += 1;
            let cur = self.current.map(|i| self.threads[i].id);
            trace!("queue {mq:#x} full: dropped msg {msg:#x} (runner {cur:?})");
        }
        ok
    }

    /// Posts the message registered for `OS_EVENT_*` `event`, if any. A full
    /// queue keeps the event pending (level-triggered, like the hardware
    /// interrupt bits) instead of dropping it.
    pub(crate) fn post_event(&mut self, event: usize) {
        let Some((mq, msg)) = self.events[event] else {
            return;
        };
        // Full queue: stay pending (level-triggered), retried at the next
        // delivery point.
        self.pending_events[event] = !self.post(mq, msg);
    }

    /// Retries every pending event (called wherever delivery happens).
    fn flush_pending_events(&mut self) {
        for event in 0..NUM_EVENTS {
            if self.pending_events[event] {
                self.post_event(event);
            }
        }
    }

    /// Delivers everything due by now: timers and VI retraces, then the
    /// events that didn't fit before, then the present tick. The present
    /// message goes last so a retrace due at the same time is handled first
    /// (audio start), and after retried SP/DP done events.
    fn deliver_due(&mut self) -> (Vec<vi::Retrace>, Option<vi::Present>) {
        let now = self.clock.now();
        time::fire_timers(self, now);
        let out = vi::retraces_due(self, now);
        if out.len() > 1 {
            trace!("deliver_due: {} retraces at once", out.len());
        }
        self.flush_pending_events();
        let present = vi::present_due(self, now);
        (out, present)
    }

    /// Whether [`Self::deliver_due`] has anything to deliver now.
    fn interrupt_due(&self) -> bool {
        self.next_event().is_some_and(|due| due <= self.clock.now())
            || self.pending_events.contains(&true)
    }

    /// Count at which the next timer or retrace is due.
    fn next_event(&self) -> Option<u64> {
        let t = self.timers.iter().map(|t| t.due).min();
        let v = self.vi.next_due();
        match (t, v) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

impl Drop for Kernel {
    fn drop(&mut self) {
        // Unfinished coroutines would force-unwind through C frames while the
        // thread-local is being destroyed; leak them (and their stacks).
        for t in &mut self.threads {
            if let Some(co) = t.co.take()
                && !co.done()
            {
                std::mem::forget(co);
            }
        }
    }
}

type RetraceHook = Box<dyn FnMut(&vi::Retrace)>;
type PresentHook = Box<dyn FnMut(&vi::Present)>;

thread_local! {
    static KERNEL: RefCell<Kernel> = RefCell::new(Kernel::new());
    static RETRACE_HOOK: RefCell<Option<RetraceHook>> = const { RefCell::new(None) };
    static PRESENT_HOOK: RefCell<Option<PresentHook>> = const { RefCell::new(None) };
    static PAUSE_HOOK: std::cell::Cell<Option<fn()>> = const { std::cell::Cell::new(None) };
}

/// Called on every iteration of [`run`]'s pause loop (the settings overlay):
/// no retrace or present hook fires while paused, so a host quit request
/// needs this to park the game thread (pw64 `park_if_quit`).
pub fn set_pause_hook(hook: fn()) {
    PAUSE_HOOK.with(|h| h.set(Some(hook)));
}

/// Per-retrace wall-time profile (`PW64_PROFILE_RETRACES=1`; temporary
/// instrumentation, renderer.md "144 Hz profiling"). With `PW64_NO_THROTTLE`
/// every loop iteration below serves ~one retrace, so elapsed/delta ≈ the
/// host cost of that retrace: C threads (coroutine OS), DL-HLE + audio-HLE
/// (run as the "RSP") and the retrace hook (VI present).
struct Profile {
    /// µs per delivered retrace, in delivery order.
    samples: Vec<f32>,
    /// Host time of loop iterations since the last retrace (with present
    /// ticks, most iterations serve a tick and no retrace).
    pending: std::time::Duration,
}

impl Profile {
    fn sample(&mut self, started: std::time::Instant, retraces_before: u64) {
        let now = with(|k| k.vi.retraces);
        let d = now - retraces_before;
        self.pending += started.elapsed();
        if d > 0 {
            let us = std::mem::take(&mut self.pending).as_nanos() as f32 / 1000.0;
            self.samples.push(us / d as f32);
        }
    }

    fn report(&self, retraces: u64) {
        let presents = with(|k| k.vi.presents);
        if presents > 0 {
            // Virtual time: retraces are 60 Hz of the OS clock.
            let secs = retraces as f64 / vi::RETRACE_HZ as f64;
            eprintln!(
                "[profile] {presents} present ticks ({:.1}/s of game time)",
                presents as f64 / secs.max(1e-9)
            );
        }
        if self.samples.is_empty() {
            return;
        }
        let stats = |s: &[f32], label: &str| {
            let mut s = s.to_vec();
            s.sort_by(|a, b| a.total_cmp(b));
            let n = s.len();
            let pct = |p: f32| s[((n as f32 * p) as usize).min(n - 1)];
            let total: f32 = s.iter().sum();
            eprintln!(
                "[profile] {label}: n {n}, avg {:.3} ms, min {:.3}, p50 {:.3}, p95 {:.3}, p99 {:.3}, max {:.3} ms/retrace",
                total / n as f32 / 1000.0,
                s[0] / 1000.0,
                pct(0.5) / 1000.0,
                pct(0.95) / 1000.0,
                pct(0.99) / 1000.0,
                s[n - 1] / 1000.0
            );
        };
        let all = &self.samples;
        eprintln!("[profile] {} VI retraces total", retraces);
        stats(all, "whole run");
        // Steady state (second half): past boot, title and menus.
        let half = all.len() / 2;
        stats(&all[half..], "last half");
    }
}

/// Runs `f` on the kernel state. Never suspend inside `f`.
pub(crate) fn with<R>(f: impl FnOnce(&mut Kernel) -> R) -> R {
    KERNEL.with(|k| f(&mut k.borrow_mut()))
}

/// Resets the kernel (tests; the game never restarts).
pub fn reset() {
    let old = KERNEL.with(|k| std::mem::replace(&mut *k.borrow_mut(), Kernel::new()));
    drop(old);
}

pub(crate) fn trace_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("PW64_TRACE_OS").is_some())
}

macro_rules! trace {
    ($($arg:tt)*) => {
        if $crate::os::trace_enabled() {
            eprintln!("[os] {}", format_args!($($arg)*));
        }
    };
}
pub(crate) use trace;

/// Creates (or re-creates) a stopped thread running `body` on a fresh low stack.
pub(crate) fn create_thread(key: usize, id: i32, pri: i32, body: Box<dyn FnOnce()>) {
    let old = with(|k| {
        let idx = match k.index(key).filter(|_| key != 0) {
            Some(i) => i,
            None => {
                k.threads.push(Thread {
                    key,
                    id,
                    pri,
                    state: State::Stopped,
                    seq: 0,
                    co: None,
                    yielder: std::ptr::null(),
                });
                k.threads.len() - 1
            }
        };
        let t = &mut k.threads[idx];
        assert!(
            k.current != Some(idx),
            "osCreateThread on the running thread {id}"
        );
        t.id = id;
        t.pri = pri;
        t.state = State::Stopped;
        let stack = LowStack::new();
        let (lo, hi) = stack.range();
        trace!("create thread id {id} pri {pri} on slot 0x{lo:x}..0x{hi:x}");
        let co = Co::with_stack(stack, move |y: &Yielder, ()| {
            with(|k| k.threads[idx].yielder = y as *const Yielder);
            body();
        });
        t.co.replace(co)
    });
    if let Some(co) = old
        && !co.done()
    {
        std::mem::forget(co);
    }
}

/// Suspends the running thread back to the root loop. The thread's state
/// must already say why (Ready/Wait*/Stopped).
pub(crate) fn suspend() {
    let y = with(|k| {
        let cur = k.current.expect("suspend outside a game thread");
        k.threads[cur].yielder
    });
    assert!(!y.is_null());
    // SAFETY: the yielder belongs to the running coroutine (we are on its stack).
    unsafe { (*y).suspend(()) };
}

/// Blocks the running thread in `state` until someone makes it ready.
pub(crate) fn block(state: State) {
    if let State::WaitSend(q) = state {
        with(|k| {
            let id = k.threads[k.current.expect("block outside a game thread")].id;
            trace!("thread {id} blocks sending to queue {q:#x}");
        });
    }
    with(|k| k.set_current_state(state));
    suspend();
}

/// Gives up the CPU if a higher-priority thread is ready (libultra's
/// `__osEnqueueAndYield` after a wake-up), or if our priority dropped to idle.
pub(crate) fn reschedule() {
    let yield_now = with(|k| {
        if k.should_preempt() {
            k.set_current_state(State::Ready);
            true
        } else {
            false
        }
    });
    if yield_now {
        suspend();
    }
}

/// OS-call checkpoint: delivers due interrupts (unless masked), then
/// reschedules. Retrace hooks run here too.
pub(crate) fn checkpoint() {
    let in_thread = with(|k| k.current.is_some());
    if !in_thread {
        return;
    }
    if with(|k| k.int_mask != misc::OS_IM_NONE && k.interrupt_due()) {
        deliver_interrupts();
    }
    reschedule();
}

/// Delivers everything due (timers, retraces, retried events) and runs the
/// retrace hooks. A started gfx task finishes first (headless.rs): the
/// scheduler must never see a retrace while the "RSP" still holds a task it
/// started before that retrace was due — on hardware it would long be done,
/// and a second retrace with an unfinished yield request makes `sched.c`
/// run its bad-display-list recovery.
fn deliver_interrupts() {
    if with(|k| k.interrupt_due()) {
        crate::headless::run_pending_rsp();
    }
    let (retraces, present) = with(|k| k.deliver_due());
    run_hooks(&retraces, present);
}

fn run_hooks(retraces: &[vi::Retrace], present: Option<vi::Present>) {
    if !retraces.is_empty() {
        RETRACE_HOOK.with(|h| {
            if let Some(h) = h.borrow_mut().as_mut() {
                for r in retraces {
                    h(r);
                }
            }
        });
    }
    if let Some(p) = present {
        PRESENT_HOOK.with(|h| {
            if let Some(h) = h.borrow_mut().as_mut() {
                h(&p);
            }
        });
    }
}

/// Called on every VI retrace (from the root loop or a checkpoint), e.g. to
/// present the framebuffer and pump window events. With a present rate set,
/// swaps latch at present ticks instead ([`set_present_hook`]); the retrace
/// then reports the framebuffer the last tick latched.
pub fn set_retrace_hook(hook: impl FnMut(&vi::Retrace) + 'static) {
    RETRACE_HOOK.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
}

/// Called at every present tick (after its swap latch), right after any
/// retrace hooks of the same delivery. Only fires with a present rate set.
pub fn set_present_hook(hook: impl FnMut(&vi::Present) + 'static) {
    PRESENT_HOOK.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
}

/// Sets the present-tick rate (framerate.md "Design", point 1), before
/// [`boot`]. `None` (the default, the `fps = 60` path) keeps the hardware
/// model exactly: swaps latch at VI retraces and the scheduler starts gfx
/// there, so the game runs at ≤ 60 fps. `Some(rate)`: the OS core posts the
/// scheduler's present message (registered by the patched sched.c) at that
/// rate and latches swaps there.
pub fn set_present_rate(rate: Option<vi::PresentRate>) {
    with(|k| {
        k.vi.present_rate = rate;
        let now = k.clock.now();
        match rate {
            Some(vi::PresentRate::Fixed { interval }) => {
                k.vi.present_next = now + interval;
            }
            Some(vi::PresentRate::Display { fallback_interval }) => {
                // The window has presented whatever happened before this
                // call; the first fallback deadline is two intervals out.
                k.vi.display_seen = vi::VBLANK.load(std::sync::atomic::Ordering::Relaxed);
                k.vi.present_next = now + 2 * fallback_interval;
            }
            _ => {}
        }
    });
}

/// The rate set by [`set_present_rate`].
pub fn present_rate() -> Option<vi::PresentRate> {
    with(|k| k.vi.present_rate)
}

/// Runs thread `idx` until it suspends or finishes. Panics propagate.
fn resume(idx: usize) {
    let mut co = with(|k| {
        k.current = Some(idx);
        let t = &mut k.threads[idx];
        t.state = State::Running;
        trace!("resume id {} (key {:#x})", t.id, t.key);
        t.co.take().expect("ready thread without a coroutine")
    });
    let r = co.resume(());
    with(|k| {
        k.current = None;
        let t = &mut k.threads[idx];
        match r {
            CoroutineResult::Yield(()) => t.co = Some(co),
            CoroutineResult::Return(()) => {
                trace!("thread id {} returned", t.id);
                t.state = State::Dead;
            }
        }
    });
}

/// Runs ready threads until all are blocked. Returns how many resumes ran.
pub fn run_ready() -> usize {
    let mut n = 0;
    loop {
        deliver_interrupts();
        let Some(i) = with(|k| k.pick()) else {
            // Idle: the "RSP" finishes a started gfx task (headless.rs).
            if crate::headless::run_pending_rsp() {
                continue;
            }
            return n;
        };
        // A started gfx task runs before any thread below RSP_PRI resumes;
        // its done events may wake a higher one first: pick again.
        if with(|k| k.threads[i].pri) < RSP_PRI && crate::headless::run_pending_rsp() {
            continue;
        }
        resume(i);
        n += 1;
    }
}

/// Real-time runs: this host thread is the whole N64 CPU, including the
/// audio thread that on hardware preempts everything at the retrace
/// interrupt. At normal priority, any CPU load (a compile, a browser) makes
/// Windows hand our wake-ups to other threads for a quantum (6 ms sleeps
/// ending after 20–40 ms): retraces arrive late, the AI starves and the
/// music plays slow with gaps (audio.md "B12"). The window thread and the
/// GPU driver's threads matter too (the game waits for the window when its
/// frame queue is full), so the whole process goes one class up. Everything
/// here is paced (sleeps), so it costs other programs little.
fn raise_priority() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{
            ABOVE_NORMAL_PRIORITY_CLASS, GetCurrentProcess, GetCurrentThread, SetPriorityClass,
            SetThreadPriority, THREAD_PRIORITY_HIGHEST,
        };
        // SAFETY: plain Win32 calls on the current process/thread pseudo-handles.
        let ok = unsafe {
            SetPriorityClass(GetCurrentProcess(), ABOVE_NORMAL_PRIORITY_CLASS) != 0
                && SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) != 0
        };
        if !ok {
            eprintln!("[os] could not raise the game's priority");
        }
    }
}

/// Host main-loop settings.
#[derive(Clone, Debug)]
pub struct RunConfig {
    /// Stop after this many VI retraces.
    pub max_retraces: Option<u64>,
    /// Sleep until the next event (real-time pacing). If false, idle time is
    /// skipped: the clock jumps to the next event.
    pub throttle: bool,
}

/// Why [`run`] returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// Every thread is blocked and no timer or retrace can wake one.
    Deadlock,
    RetraceLimit,
}

/// The host main loop: runs threads, and while all are blocked waits for (or
/// skips to) the next timer / VI retrace / present tick. While [`PAUSED`] is set, nothing
/// runs and the clock is frozen (see [`time::Clock::freeze`]).
pub fn run(cfg: &RunConfig) -> Stop {
    if cfg.throttle {
        raise_priority();
    }
    let mut prof = std::env::var_os("PW64_PROFILE_RETRACES").map(|_| Profile {
        samples: Vec::new(),
        pending: std::time::Duration::ZERO,
    });
    let stop = loop {
        if PAUSED.load(Ordering::Acquire) {
            with(|k| k.clock.freeze());
            if let Some(hook) = PAUSE_HOOK.with(|h| h.get()) {
                hook();
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
            continue;
        }
        with(|k| k.clock.unfreeze());
        let mark = prof
            .is_some()
            .then(|| (std::time::Instant::now(), with(|k| k.vi.retraces)));
        run_ready();
        if let (Some(p), Some((started, r0))) = (prof.as_mut(), mark) {
            p.sample(started, r0);
        }
        if cfg
            .max_retraces
            .is_some_and(|m| with(|k| k.vi.retraces) >= m)
        {
            break Stop::RetraceLimit;
        }
        let Some(due) = with(|k| k.next_event()) else {
            break Stop::Deadlock;
        };
        let now = with(|k| k.clock.now());
        if due > now {
            if cfg.throttle {
                let d = time::counts_to_duration(due - now);
                // Display pacing: `due` is only the fallback deadline; the
                // window's present must end the wait at once, not at the
                // next retrace/fallback (that halved the V-Sync rate).
                match with(|k| k.vi.display_wait()) {
                    Some(seen) => vi::sleep_until_vblank(d, seen),
                    None => std::thread::sleep(d),
                }
            } else {
                with(|k| k.clock.skip(due - now));
            }
        }
    };
    if let Some(p) = &prof {
        p.report(with(|k| k.vi.retraces));
        let hs = crate::headless::profile_stats();
        if !hs.is_empty() {
            eprintln!("{hs}");
        }
    }
    stop
}

/// Starts `entry(arg)` as the boot thread (N64 `bootproc` runs before any
/// `OSThread` exists; here it gets a thread of its own at priority 1).
pub fn boot(entry: unsafe extern "C-unwind" fn(*mut c_void), arg: *mut c_void) {
    const BOOT_KEY: usize = 1;
    #[cfg(any(windows, target_os = "linux"))]
    crash::install(); // report RIP/address for a fault inside the C
    let arg = arg as usize;
    create_thread(
        BOOT_KEY,
        -1,
        1,
        // SAFETY: the C boot entry point.
        Box::new(move || unsafe { entry(arg as *mut c_void) }),
    );
    with(|k| {
        let i = k.index(BOOT_KEY).unwrap();
        k.make_ready(i);
    });
}

/// One line per thread: id, priority, state (for stop/deadlock reports).
pub fn dump_threads() -> String {
    with(|k| {
        let mut s = String::new();
        for t in &k.threads {
            let st = match t.state {
                State::WaitRecv(q) => format!("waiting to receive on queue {q:#x}"),
                State::WaitSend(q) => format!("waiting to send on full queue {q:#x}"),
                other => format!("{other:?}"),
            };
            s += &format!(
                "  thread id {:>2} pri {:>3} @ {:#x}: {st}\n",
                t.id, t.pri, t.key
            );
        }
        s += &format!(
            "  VI retraces {}, present ticks {}, count {:#x}, dropped msgs {}\n",
            k.vi.retraces,
            k.vi.presents,
            k.clock.now(),
            k.drops
        );
        s
    })
}

#[cfg(test)]
mod tests;
