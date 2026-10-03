//! Scheduler / message-queue semantics, with Rust closures as thread bodies.

use super::mesg::*;
use super::thread::*;
use super::*;
use std::cell::RefCell;
use std::rc::Rc;

const BLOCK: i32 = 1;
const NOBLOCK: i32 = 0;

type Log = Rc<RefCell<Vec<String>>>;

fn log() -> Log {
    Rc::new(RefCell::new(Vec::new()))
}

fn push(l: &Log, s: impl Into<String>) {
    l.borrow_mut().push(s.into());
}

fn queue(n: usize) -> *mut OSMesgQueue {
    let q = Box::leak(Box::new(std::mem::MaybeUninit::<OSMesgQueue>::uninit())).as_mut_ptr();
    let buf = Box::leak(vec![std::ptr::null_mut::<c_void>(); n].into_boxed_slice()).as_mut_ptr();
    // SAFETY: `q` and the `n`-slot buffer are leaked, so they outlive every
    // user of the queue in the tests.
    unsafe { osCreateMesgQueue(q, buf, n as i32) };
    q
}

/// Creates and starts a thread; the key doubles as the `OSThread*` handle.
fn spawn(key: usize, pri: i32, f: impl FnOnce() + 'static) {
    create_thread(key, key as i32, pri, Box::new(f));
    osStartThread(key as *mut c_void);
}

fn send(q: *mut OSMesgQueue, v: usize, flag: i32) -> i32 {
    // SAFETY: `q` comes from the `queue()` helper (initialised by
    // osCreateMesgQueue) and stays valid for the whole test.
    unsafe { osSendMesg(q, v as OSMesg, flag) }
}

fn recv(q: *mut OSMesgQueue, flag: i32) -> Option<usize> {
    let mut m: OSMesg = std::ptr::null_mut();
    // SAFETY: `q` comes from the `queue()` helper (initialised by
    // osCreateMesgQueue) and stays valid for the whole test.
    (unsafe { osRecvMesg(q, &mut m, flag) } == 0).then_some(m as usize)
}

fn entries(l: &Log) -> Vec<String> {
    l.borrow().clone()
}

#[test]
fn highest_priority_first_fifo_within() {
    reset();
    let l = log();
    for (key, pri) in [(100, 10), (101, 20), (102, 20), (103, 5)] {
        let l = l.clone();
        spawn(key, pri, move || push(&l, format!("{key}")));
    }
    run_ready();
    assert_eq!(entries(&l), ["101", "102", "100", "103"]);
}

#[test]
fn send_wakes_and_preempts_higher_priority_receiver() {
    reset();
    let l = log();
    let q = queue(1);
    let (l1, l2) = (l.clone(), l.clone());
    spawn(100, 10, move || {
        push(&l1, "H wait");
        let m = recv(q, BLOCK).unwrap();
        push(&l1, format!("H got {m}"));
    });
    spawn(101, 5, move || {
        push(&l2, "L send");
        assert_eq!(send(q, 42, BLOCK), 0);
        push(&l2, "L after");
    });
    run_ready();
    assert_eq!(entries(&l), ["H wait", "L send", "H got 42", "L after"]);
}

#[test]
fn lower_priority_receiver_does_not_preempt() {
    reset();
    let l = log();
    let q = queue(1);
    let (l1, l2) = (l.clone(), l.clone());
    spawn(100, 5, move || {
        let m = recv(q, BLOCK).unwrap();
        push(&l1, format!("L got {m}"));
    });
    spawn(101, 10, move || {
        send(q, 7, BLOCK);
        push(&l2, "H after");
    });
    run_ready();
    assert_eq!(entries(&l), ["H after", "L got 7"]);
}

#[test]
fn full_queue_blocks_sender_until_receive() {
    reset();
    let l = log();
    let q = queue(1);
    let (l1, l2) = (l.clone(), l.clone());
    spawn(100, 10, move || {
        send(q, 1, BLOCK);
        push(&l1, "sent 1");
        assert_eq!(send(q, 2, NOBLOCK), -1);
        send(q, 2, BLOCK);
        push(&l1, "sent 2");
    });
    spawn(101, 5, move || {
        let a = recv(q, BLOCK).unwrap();
        push(&l2, format!("got {a}"));
        let b = recv(q, BLOCK).unwrap();
        push(&l2, format!("got {b}"));
    });
    run_ready();
    // The receive frees a slot and wakes the higher-priority sender at once.
    assert_eq!(entries(&l), ["sent 1", "sent 2", "got 1", "got 2"]);
}

#[test]
fn jam_puts_message_first_and_noblock_fails_cleanly() {
    reset();
    let l = log();
    let q = queue(3);
    let l1 = l.clone();
    spawn(100, 10, move || {
        assert_eq!(recv(q, NOBLOCK), None);
        send(q, 1, NOBLOCK);
        send(q, 2, NOBLOCK);
        // SAFETY: valid queue from `queue()`, and 9 fits before the two
        // entries just sent (there is room).
        assert_eq!(unsafe { osJamMesg(q, 9 as OSMesg, NOBLOCK) }, 0);
        assert_eq!(send(q, 3, NOBLOCK), -1);
        // SAFETY: valid queue from `queue()`; only this coroutine touches it.
        assert_eq!(unsafe { (*q).valid_count }, 3);
        while let Some(m) = recv(q, NOBLOCK) {
            push(&l1, format!("{m}"));
        }
    });
    run_ready();
    assert_eq!(entries(&l), ["9", "1", "2"]);
}

#[test]
fn start_thread_preempts_and_idle_priority_parks() {
    reset();
    let l = log();
    let l1 = l.clone();
    spawn(100, 12, move || {
        let l2 = l1.clone();
        create_thread(101, 101, 20, Box::new(move || push(&l2, "high child")));
        let l3 = l1.clone();
        create_thread(102, 102, 10, Box::new(move || push(&l3, "low child")));
        osStartThread(101 as *mut c_void);
        push(&l1, "after high start");
        osStartThread(102 as *mut c_void);
        push(&l1, "after low start");
        osSetThreadPri(std::ptr::null_mut(), 0);
        push(&l1, "never");
    });
    run_ready();
    assert_eq!(
        entries(&l),
        [
            "high child",
            "after high start",
            "after low start",
            "low child"
        ]
    );
    assert_eq!(with(|k| k.threads[0].state), State::Ready);
}

#[test]
fn yield_round_robins_equal_priorities() {
    reset();
    let l = log();
    for key in [100, 101] {
        let l = l.clone();
        spawn(key, 10, move || {
            for i in 0..2 {
                push(&l, format!("{key}.{i}"));
                osYieldThread();
            }
        });
    }
    run_ready();
    assert_eq!(entries(&l), ["100.0", "101.0", "100.1", "101.1"]);
}

#[test]
fn stop_thread_and_restart() {
    reset();
    let l = log();
    let l1 = l.clone();
    spawn(100, 10, move || {
        push(&l1, "a");
        osStopThread(std::ptr::null_mut());
        push(&l1, "b");
    });
    run_ready();
    assert_eq!(entries(&l), ["a"]);
    osStartThread(100 as *mut c_void);
    run_ready();
    assert_eq!(entries(&l), ["a", "b"]);
    assert_eq!(osGetThreadPri(100 as *mut c_void), 10);
}

#[test]
fn vi_events_timers_and_deadlock() {
    reset();
    let l = log();
    let vq = queue(4);
    let tq = queue(1);
    let l1 = l.clone();
    spawn(100, 10, move || {
        vi::osViSetEvent(vq, 0x29a as OSMesg, 1);
        let timer = Box::leak(Box::new([0u64; 8])).as_mut_ptr().cast::<c_void>();
        let t0 = time::osGetTime();
        time::osSetTimer(timer, 5000, 0, tq, 5 as OSMesg);
        assert_eq!(recv(tq, BLOCK), Some(5));
        assert!(time::osGetTime() - t0 >= 5000);
        for _ in 0..3 {
            assert_eq!(recv(vq, BLOCK), Some(0x29a));
            push(&l1, "vi");
        }
    });
    let cfg = RunConfig {
        max_retraces: Some(3),
        throttle: false,
    };
    assert_eq!(run(&cfg), Stop::RetraceLimit);
    assert_eq!(entries(&l), ["vi", "vi", "vi"]);

    // Nothing registered to wake a blocked thread: deadlock.
    reset();
    let q = queue(1);
    spawn(100, 10, move || {
        recv(q, BLOCK);
    });
    let cfg = RunConfig {
        max_retraces: None,
        throttle: false,
    };
    assert_eq!(run(&cfg), Stop::Deadlock);
    assert!(dump_threads().contains("waiting to receive"));
}

#[test]
fn event_mesg_posts_from_host() {
    reset();
    let q = queue(2);
    osSetEventMesg(OS_EVENT_SP as u32, q, 0x51 as OSMesg);
    post_event(OS_EVENT_SP);
    post_event(OS_EVENT_DP); // unregistered: ignored
    // SAFETY: valid queue from `queue()`; the event was posted and consumed
    // before this read.
    assert_eq!(unsafe { (*q).valid_count }, 1);
}

#[test]
fn stacks_are_above_the_window_below_4gb() {
    reset();
    spawn(100, 10, || {
        let local = 0u8;
        let a = &raw const local as usize;
        assert!((0x8080_0000..0xA000_0000).contains(&a), "{a:#x}");
    });
    assert_eq!(run_ready(), 1);
}

#[test]
fn panics_propagate_out_of_threads() {
    reset();
    spawn(100, 10, || panic!("boom"));
    let r = std::panic::catch_unwind(run_ready);
    assert!(r.is_err());
    reset();
}

/// A leaked host-layout `OSScTask` (see `osSpTaskStartGo`) with an `M_GFXTASK`
/// `OSTask`; returns `&task.list`.
fn fake_gfx_task() -> *mut c_void {
    let buf = Box::leak(Box::new([0u64; 32]));
    let list = (buf.as_mut_ptr() as usize + 16) as *mut u32;
    // SAFETY: `list` points 16 bytes into the leaked 256-byte buffer, so the
    // write is in bounds.
    unsafe { list.write(crate::headless::M_GFXTASK) };
    list.cast()
}

/// SP done → 1, DP done → 2 on `q`; gfx tasks log "gfx" to `l`.
fn gfx_events(q: *mut OSMesgQueue, l: &Log) {
    osSetEventMesg(OS_EVENT_SP as u32, q, 1 as OSMesg);
    osSetEventMesg(OS_EVENT_DP as u32, q, 2 as OSMesg);
    let l = l.clone();
    crate::headless::set_gfx_task_handler(move |_| push(&l, "gfx"));
}

/// A started gfx task runs after the threads at or above `RSP_PRI` (the
/// audio thread) and before lower ones (the app), like the parallel RSP.
#[test]
fn gfx_task_runs_after_high_threads_before_low() {
    reset();
    let l = log();
    let q = queue(8);
    gfx_events(q, &l);
    let (l1, l2, l3) = (l.clone(), l.clone(), l.clone());
    spawn(100, 10, move || push(&l3, "app"));
    spawn(101, 110, move || push(&l2, "audio"));
    spawn(102, 127, move || {
        // SAFETY: `fake_gfx_task()` returns a leaked, zeroed buffer whose
        // first word is a valid M_GFXTASK type, what `osSpTaskStartGo` reads.
        unsafe { crate::headless::osSpTaskStartGo(fake_gfx_task()) };
        push(&l1, "started");
        let (a, b) = (recv(q, BLOCK), recv(q, BLOCK));
        push(&l1, format!("sched {a:?} {b:?}"));
    });
    run_ready();
    assert_eq!(
        entries(&l),
        ["started", "audio", "gfx", "sched Some(1) Some(2)", "app"]
    );
}

/// The root loop runs a pending gfx task before it delivers a retrace that
/// fell due meanwhile: the scheduler sees SP + DP done, then the retrace.
#[test]
fn gfx_task_finishes_before_the_next_retrace() {
    reset();
    let l = log();
    let q = queue(8);
    gfx_events(q, &l);
    vi::osViSetEvent(q, 666 as OSMesg, 1);
    let l1 = l.clone();
    spawn(102, 127, move || {
        // SAFETY: `fake_gfx_task()` returns a leaked, zeroed buffer whose
        // first word is a valid M_GFXTASK type, what `osSpTaskStartGo` reads.
        unsafe { crate::headless::osSpTaskStartGo(fake_gfx_task()) };
        for _ in 0..3 {
            push(&l1, format!("{:?}", recv(q, BLOCK)));
        }
    });
    // The audio thread runs before the "RSP" and overruns into the retrace.
    spawn(101, 110, || with(|k| k.clock.skip(vi::RETRACE_COUNTS)));
    run_ready();
    assert_eq!(entries(&l), ["gfx", "Some(1)", "Some(2)", "Some(666)"]);
}

/// A low-priority thread's checkpoint runs the pending task: SP done wakes
/// the scheduler, but DP done must be posted before the caller is preempted
/// and the root loop delivers the due timer.
#[test]
fn gfx_done_events_are_not_split_by_a_due_interrupt() {
    reset();
    let l = log();
    let q = queue(8);
    gfx_events(q, &l);
    let l1 = l.clone();
    spawn(102, 127, move || {
        for _ in 0..3 {
            push(&l1, format!("{:?}", recv(q, BLOCK)));
        }
    });
    let other = queue(1);
    spawn(100, 10, move || {
        // SAFETY: `fake_gfx_task()` returns a leaked, zeroed buffer whose
        // first word is a valid M_GFXTASK type, what `osSpTaskStartGo` reads.
        unsafe { crate::headless::osSpTaskStartGo(fake_gfx_task()) };
        let timer = Box::leak(Box::new([0u64; 8])).as_mut_ptr().cast::<c_void>();
        time::osSetTimer(timer, 1, 0, q, 3 as OSMesg);
        with(|k| k.clock.skip(10));
        // An OS call with the timer due: its checkpoint runs the task first.
        send(other, 0, NOBLOCK);
    });
    run_ready();
    assert_eq!(entries(&l), ["gfx", "Some(1)", "Some(2)", "Some(3)"]);
}

// --- Present tick (vi.rs, framerate.md "Design") ---------------------------

const PRESENT_MSG: usize = 670;
const VIDEO_MSG: usize = 666;

/// Everything queued on `q` right now, in order (host side, no blocking).
fn drain_q(q: *mut OSMesgQueue) -> Vec<usize> {
    std::iter::from_fn(|| recv(q, NOBLOCK)).collect()
}

/// Frozen clock: only `advance` moves it, so due times are exact.
fn advance(counts: u64) {
    with(|k| k.clock.skip(counts));
}

fn current_fb() -> usize {
    vi::osViGetCurrentFramebuffer() as usize
}

/// With a present rate, swaps latch at present ticks, not at retraces; a
/// retrace due together with a tick is posted (and hooked) first.
#[test]
fn present_tick_latches_swaps_and_follows_the_retrace() {
    reset();
    with(|k| k.clock.freeze());
    let r = vi::RETRACE_COUNTS;
    let l = log();
    let (l1, l2) = (l.clone(), l.clone());
    set_retrace_hook(move |x| push(&l1, format!("r{}", x.number)));
    set_present_hook(move |x| push(&l2, format!("p{} {:#x}", x.number, x.framebuffer)));
    set_present_rate(Some(vi::PresentRate::Fixed {
        interval: r + r / 2,
    }));
    let q = queue(16);
    vi::osViSetEvent(q, VIDEO_MSG as OSMesg, 1);
    assert_eq!(
        vi::pw64_present_active(),
        0,
        "not before the scheduler registers"
    );
    vi::pw64_present_set_event(q, PRESENT_MSG as OSMesg);
    assert_eq!(vi::pw64_present_active(), 1);

    vi::osViSwapBuffer(0x1000 as *mut c_void);
    advance(r); // retrace 1 only: no latch
    run_ready();
    assert_eq!(drain_q(q), [VIDEO_MSG]);
    assert_eq!(current_fb(), 0);
    advance(r / 2); // tick 1 (1.5 R): latches
    run_ready();
    assert_eq!(drain_q(q), [PRESENT_MSG]);
    assert_eq!(current_fb(), 0x1000);

    vi::osViSwapBuffer(0x2000 as *mut c_void);
    advance(r / 2); // retrace 2
    run_ready();
    assert_eq!(drain_q(q), [VIDEO_MSG]);
    assert_eq!(current_fb(), 0x1000);
    advance(r); // retrace 3 and tick 2 together (3 R): retrace first
    run_ready();
    assert_eq!(drain_q(q), [VIDEO_MSG, PRESENT_MSG]);
    assert_eq!(current_fb(), 0x2000);
    assert_eq!(entries(&l), ["r1", "p1 0x1000", "r2", "r3", "p2 0x2000"]);
}

/// No present rate (the fps = 60 path): swaps latch at the retrace as on
/// hardware, and the registered present message is never posted.
#[test]
fn without_a_present_rate_retraces_latch() {
    reset();
    with(|k| k.clock.freeze());
    let q = queue(16);
    vi::osViSetEvent(q, VIDEO_MSG as OSMesg, 1);
    vi::pw64_present_set_event(q, PRESENT_MSG as OSMesg);
    assert_eq!(vi::pw64_present_active(), 0);
    vi::osViSwapBuffer(0x1000 as *mut c_void);
    for _ in 0..3 {
        advance(vi::RETRACE_COUNTS);
        run_ready();
    }
    assert_eq!(drain_q(q), [VIDEO_MSG; 3]);
    assert_eq!(current_fb(), 0x1000);
    assert_eq!(with(|k| k.vi.presents), 0);
}

/// Deferred-gfx invariant (native-build.md §8) holds for present ticks: a
/// started gfx task finishes (SP + DP done) before the tick is posted.
#[test]
fn gfx_task_finishes_before_the_next_present_tick() {
    reset();
    with(|k| k.clock.freeze());
    let l = log();
    let q = queue(8);
    gfx_events(q, &l);
    let interval = vi::RETRACE_COUNTS / 3;
    set_present_rate(Some(vi::PresentRate::Fixed { interval }));
    vi::pw64_present_set_event(q, PRESENT_MSG as OSMesg);
    let l1 = l.clone();
    spawn(102, 127, move || {
        // SAFETY: `fake_gfx_task()` returns a leaked, zeroed buffer whose
        // first word is a valid M_GFXTASK type, what `osSpTaskStartGo` reads.
        unsafe { crate::headless::osSpTaskStartGo(fake_gfx_task()) };
        for _ in 0..3 {
            push(&l1, format!("{:?}", recv(q, BLOCK)));
        }
    });
    // The audio thread runs before the "RSP" and overruns into the tick.
    spawn(101, 110, move || advance(interval));
    run_ready();
    assert_eq!(entries(&l), ["gfx", "Some(1)", "Some(2)", "Some(670)"]);
}

/// Uncapped: a tick as soon as a swap is pending, but ≥ MIN_PRESENT_COUNTS
/// after the last one; with nothing pending only a retrace-rate keep-alive.
#[test]
fn uncapped_ticks_follow_swaps() {
    reset();
    with(|k| k.clock.freeze());
    let min = vi::MIN_PRESENT_COUNTS;
    set_present_rate(Some(vi::PresentRate::Uncapped));
    let q = queue(16);
    vi::pw64_present_set_event(q, PRESENT_MSG as OSMesg);
    vi::osViSwapBuffer(0x1000 as *mut c_void);
    advance(min - 1);
    run_ready();
    assert!(drain_q(q).is_empty(), "min interval since the last tick");
    advance(1);
    run_ready();
    assert_eq!(drain_q(q), [PRESENT_MSG]);
    assert_eq!(current_fb(), 0x1000);
    // Nothing pending: no tick until the keep-alive.
    advance(min);
    run_ready();
    assert!(drain_q(q).is_empty());
    // A swap after the min interval: ticks at once.
    vi::osViSwapBuffer(0x2000 as *mut c_void);
    run_ready();
    assert_eq!(drain_q(q), [PRESENT_MSG]);
    assert_eq!(current_fb(), 0x2000);
    advance(vi::RETRACE_COUNTS);
    run_ready();
    assert_eq!(drain_q(q), [PRESENT_MSG], "keep-alive");
}

/// A fixed-rate tick keeps its phase, and after a stall fires once (no
/// burst) and resyncs.
#[test]
fn fixed_ticks_keep_phase_and_skip_backlog() {
    reset();
    with(|k| k.clock.freeze());
    let i = time::COUNT_RATE / 144;
    set_present_rate(Some(vi::PresentRate::Fixed { interval: i }));
    let q = queue(16);
    vi::pw64_present_set_event(q, PRESENT_MSG as OSMesg);
    for _ in 0..5 {
        advance(i);
        run_ready();
    }
    assert_eq!(drain_q(q).len(), 5);
    advance(10 * i); // stall
    run_ready();
    assert_eq!(drain_q(q), [PRESENT_MSG]);
    advance(i - 1);
    run_ready();
    assert!(drain_q(q).is_empty());
    advance(1);
    run_ready();
    assert_eq!(drain_q(q), [PRESENT_MSG]);
    assert_eq!(with(|k| k.vi.presents), 7);
}

/// `vi::VBLANK` is process-global and tests run in parallel: every test that
/// uses Display pacing holds this, so another test's increments can't tick it.
fn vblank_lock() -> std::sync::MutexGuard<'static, ()> {
    static L: std::sync::Mutex<()> = std::sync::Mutex::new(());
    L.lock().unwrap_or_else(|e| e.into_inner())
}

/// Sleeping for the Display fallback ends as soon as the window presents.
#[test]
fn display_wait_wakes_on_vblank() {
    let _g = vblank_lock();
    let seen = vi::VBLANK.load(std::sync::atomic::Ordering::Relaxed);
    let bump = std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_millis(5));
        vi::VBLANK.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    });
    let t = std::time::Instant::now();
    vi::sleep_until_vblank(std::time::Duration::from_secs(2), seen);
    bump.join().unwrap();
    assert!(t.elapsed() < std::time::Duration::from_millis(500));
}

/// Display pacing (V-Sync): a tick right after each `VBLANK` increment (the
/// window presented); none while the counter is silent, until the fallback
/// kicks in two intervals in; after a long silence, one tick per interval —
/// never a burst.
#[test]
fn display_ticks_follow_the_vblank_counter() {
    let _g = vblank_lock();
    reset();
    with(|k| k.clock.freeze());
    let fb = vi::RETRACE_COUNTS / 2; // fallback: 30 Hz
    set_present_rate(Some(vi::PresentRate::Display {
        fallback_interval: fb,
    }));
    let q = queue(16);
    vi::pw64_present_set_event(q, PRESENT_MSG as OSMesg);
    vi::osViSwapBuffer(0x1000 as *mut c_void);

    // Silent counter: no tick until the fallback deadline (2 * fb).
    advance(2 * fb - 1);
    run_ready();
    assert!(drain_q(q).is_empty(), "silent window: no tick yet");
    advance(1);
    run_ready();
    assert_eq!(drain_q(q), [PRESENT_MSG], "fallback after two intervals");
    assert_eq!(current_fb(), 0x1000);

    // A long gap (stall, minimised window) fires the fallback cadence: one
    // per interval, no backlog burst.
    advance(10 * fb);
    run_ready();
    assert_eq!(drain_q(q).len(), 1);
    advance(fb - 1);
    run_ready();
    assert!(drain_q(q).is_empty());
    advance(1);
    run_ready();
    assert_eq!(drain_q(q), [PRESENT_MSG]);

    // The window presented again: the very next increment ticks at once,
    // and exactly once (no second tick without another increment).
    vi::VBLANK.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    run_ready();
    assert_eq!(drain_q(q), [PRESENT_MSG]);
    advance(fb);
    run_ready();
    assert!(drain_q(q).is_empty(), "silent since: fallback not yet due");
    vi::VBLANK.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    run_ready();
    assert_eq!(drain_q(q), [PRESENT_MSG]);
    // Two increments before the first tick is delivered still give one tick
    // (at most one per vblank observed).
    vi::VBLANK.fetch_add(2, std::sync::atomic::Ordering::Relaxed);
    run_ready();
    assert_eq!(drain_q(q), [PRESENT_MSG]);
    assert_eq!(with(|k| k.vi.presents), 6);
}

/// While the OS core is paused (settings overlay), `run` delivers nothing —
/// no retraces, no present ticks, not even the Display fallback — and the
/// clock resumes exactly where it left off: the pause's wall time contributes
/// nothing (no burst of missed events on resume).
#[test]
fn paused_fires_no_present_ticks() {
    let _g = vblank_lock();
    reset();
    with(|k| k.clock.freeze());
    let l = log();
    let fb = vi::RETRACE_COUNTS;
    set_present_rate(Some(vi::PresentRate::Display {
        fallback_interval: fb,
    }));
    let (l1, l2) = (l.clone(), l.clone());
    set_retrace_hook(move |x| push(&l1, format!("r{}", x.number)));
    set_present_hook(move |x| push(&l2, format!("p{}", x.number)));
    let q = queue(16);
    vi::osViSetEvent(q, VIDEO_MSG as OSMesg, 1);
    vi::pw64_present_set_event(q, PRESENT_MSG as OSMesg);
    // The receiver: without a thread nothing resumes, so the hooks never
    // run; it only drains so the queue never fills.
    spawn(102, 10, move || while recv(q, BLOCK).is_some() {});
    vi::osViSwapBuffer(0x1000 as *mut c_void);

    // The count is ~0; the fallback would fire two intervals (two retraces)
    // in. Paused for 100 ms of wall time, it must not fire at all, and the
    // missed time must not pile up on resume: p1 stays between r2 and r3.
    set_paused(true);
    let unpause = std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_millis(100));
        set_paused(false);
    });
    let stop = run(&RunConfig {
        max_retraces: Some(4),
        throttle: false,
    });
    unpause.join().unwrap();
    set_paused(false);
    assert_eq!(stop, Stop::RetraceLimit);
    // p1 lands exactly at the fallback deadline (retrace 2, not at resume):
    // the paused wall time contributed nothing. p2/p3 are the fallback
    // cadence (Fixed at `fallback_interval`) while the window stays silent.
    assert_eq!(entries(&l), ["r1", "r2", "p1", "r3", "p2", "r4", "p3"]);
    assert_eq!(with(|k| k.vi.presents), 3);
}
