//! SI / RSP backends (native-build.md §6 task 7; AI is in `ai.rs`):
//! - SP: audio tasks go to `set_audio_task_handler` (the `pw64` exe → the
//!   `pw64-audio` HLE) and complete with SP done. Gfx tasks become pending
//!   and later run ([`run_pending_rsp`], from the OS core) through a host
//!   hook (`set_gfx_task_handler` — the `pw64` exe wires it to `pw64-gfx`),
//!   then complete with SP + DP done.
//! - SI: up to four standard controllers (idle unless the host publishes
//!   input via `set_controller1` / `set_controller`; how many are present
//!   comes from `set_controllers_present`, default 1); reads post
//!   `OS_EVENT_SI`.
//!   EEPROM is a `.eep` file (`PW64_EEP`, else the host default set via
//!   `set_default_eep_path`, else `pw64.eep` next to the CWD).

use crate::os::mesg::post_event;
use crate::os::{OS_EVENT_DP, OS_EVENT_SI, OS_EVENT_SP, reschedule, with};
use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// Player-facing messages the platform wants shown as toasts (save-file
/// problems): the `pw64` window drains [`take_toasts`] every frame and shows
/// them in its toast UI.
static TOASTS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Queues a player-facing toast for the window to show.
pub fn notify_toast(text: String) {
    if let Ok(mut q) = TOASTS.lock() {
        q.push(text);
    }
}

/// Takes every queued toast, oldest first (empty when none).
pub fn take_toasts() -> Vec<String> {
    TOASTS
        .lock()
        .map(|mut q| std::mem::take(&mut *q))
        .unwrap_or_default()
}

/// `OSTask.t.type` values.
pub const M_GFXTASK: u32 = 1;
pub const M_AUDTASK: u32 = 2;

type SpHook = Box<dyn FnMut(*mut c_void, u32)>;
type GfxHook = Box<dyn FnMut(&GfxTaskInfo)>;
type FbCopyHook = Box<dyn FnMut(u32, u32)>;
type FbPixelsHook = Box<dyn FnMut(u32, &[u32], u16)>;

thread_local! {
    static FB_COPY: RefCell<Option<FbCopyHook>> = const { RefCell::new(None) };
    static FB_PIXELS: RefCell<Option<FbPixelsHook>> = const { RefCell::new(None) };
    static SP_HOOK: RefCell<Option<SpHook>> = const { RefCell::new(None) };
    static GFX_TASK: RefCell<Option<GfxHook>> = const { RefCell::new(None) };
    static AUDIO_TASK: RefCell<Option<GfxHook>> = const { RefCell::new(None) };
    /// Started gfx task not yet run: (`OSTask*`, its info).
    static PENDING_GFX: Cell<Option<(usize, GfxTaskInfo)>> = const { Cell::new(None) };
}

/// `PW64_PROFILE_RETRACES=1`: wall time spent in the host gfx/audio task HLE
/// (temporary instrumentation, renderer.md "144 Hz profiling").
fn profile_on() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("PW64_PROFILE_RETRACES").is_some())
}

/// ns total and task count, gfx-task HLE / audio-task HLE.
static GFX_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static GFX_N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static AUD_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static AUD_N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The frame-budget split for `PW64_PROFILE_RETRACES` (empty when off): wall
/// time in the gfx-task HLE (DL interpretation + GPU render, via the host
/// hook) and the audio-task HLE, and how much of each run is that work.
pub fn profile_stats() -> String {
    if !profile_on() {
        return String::new();
    }
    use std::sync::atomic::Ordering::Relaxed;
    let (g, gn) = (GFX_NS.load(Relaxed), GFX_N.load(Relaxed));
    let (a, an) = (AUD_NS.load(Relaxed), AUD_N.load(Relaxed));
    format!(
        "[profile] gfx HLE {:.1} ms / {gn} tasks ({:.3} ms/task); audio HLE {:.1} ms / {an} tasks ({:.3} ms/task)",
        g as f64 / 1e6,
        g as f64 / 1e6 / gn.max(1) as f64,
        a as f64 / 1e6,
        a as f64 / 1e6 / an.max(1) as f64
    )
}

/// Called for every audio task (`M_AUDTASK`: `data_ptr`/`data_size` = the
/// Acmd list) at `osSpTaskStartGo`, before SP done is posted. The `pw64`
/// exe runs `pw64-audio`'s HLE here; without a handler tasks are silent.
pub fn set_audio_task_handler(h: impl FnMut(&GfxTaskInfo) + 'static) {
    AUDIO_TASK.with(|a| *a.borrow_mut() = Some(Box::new(h)));
}

/// Called with (`OSTask*`, task type) at every `osSpTaskStartGo`, before the
/// done events are posted.
pub fn set_sp_task_hook(hook: impl FnMut(*mut c_void, u32) + 'static) {
    SP_HOOK.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
}

/// Host layout of the task the scheduler starts (`OSScTask` holds an
/// `OSTask list`; x86_64 pointers are 8 bytes). Only what the HLE paths need.
/// Offsets mirror `decomp/include/libultra/PR/{sptask,sched}.h`.
#[derive(Debug, Clone, Copy)]
pub struct GfxTaskInfo {
    /// `OSTask.t.type` (`M_GFXTASK`).
    pub ty: u32,
    /// `OSScTask.flags` (`OS_SC_*` bits).
    pub flags: u32,
    /// `OSTask.t.data_ptr`: the display list.
    pub data_ptr: usize,
    /// `OSTask.t.data_size` in bytes.
    pub data_size: u32,
    /// `OSScTask.framebuffer` (N64-style address the VI will swap to).
    pub framebuffer: usize,
}

/// Called for every gfx task when it runs (deferred from `osSpTaskStartGo`
/// to [`run_pending_rsp`]), before the SP/DP done events are posted. HLE
/// renderers hook here.
pub fn set_gfx_task_handler(h: impl FnMut(&GfxTaskInfo) + 'static) {
    GFX_TASK.with(|g| *g.borrow_mut() = Some(Box::new(h)));
}

/// Called by `uvCopyFrameBuf` (graphics.c.patch → [`pw64_fb_copy`]) with the
/// (dst, src) framebuffer addresses (N64 K0 form) after its RDRAM memcpy:
/// the HLE renderer keeps framebuffers as GPU targets and copies those.
pub fn set_fb_copy_handler(h: impl FnMut(u32, u32) + 'static) {
    FB_COPY.with(|f| *f.borrow_mut() = Some(Box::new(h)));
}

/// `uvCopyFrameBuf`'s host half (see [`set_fb_copy_handler`]). A gfx task
/// the scheduler already started finishes first — on hardware the RDP runs
/// ahead of the CPU, and the game only copies after it waited for the frame
/// (`uvGfxWaitForMesg` / a 0.1 s busy loop in snap.c), so the source holds
/// the finished frame there too.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn pw64_fb_copy(dst: u32, src: u32) {
    run_pending_rsp();
    FB_COPY.with(|f| {
        if let Some(f) = f.borrow_mut().as_mut() {
            f(dst, src);
        }
    });
}

/// Called by `snowDraw` (snow.c.patch → [`pw64_fb_pixels`]) with the
/// previous, finished framebuffer's address (N64 K0 form) and the snow
/// pixel indexes (`y*320 + x`) the CPU wrote into its RDRAM: the HLE
/// renderer keeps framebuffers as GPU targets and draws the pixels there.
pub fn set_fb_pixels_handler(h: impl FnMut(u32, &[u32], u16) + 'static) {
    FB_PIXELS.with(|f| *f.borrow_mut() = Some(Box::new(h)));
}

/// `snowDraw`'s host half (see [`set_fb_pixels_handler`]). A gfx task the
/// scheduler already started finishes first — on hardware the RDP runs
/// ahead of the CPU, and `uvGfxEnd` only calls the callback with a frame
/// it waited for, so the source holds the finished frame there too.
///
/// # Safety
/// `idx` must be null with `count <= 0`, or valid for `count` u32 entries
/// (the C passes its own snow-data array).
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn pw64_fb_pixels(fb: u32, idx: *const u32, count: i32, color: u32) {
    run_pending_rsp();
    FB_PIXELS.with(|f| {
        if let Some(f) = f.borrow_mut().as_mut() {
            if idx.is_null() || count <= 0 {
                return;
            }
            // SAFETY: the C passes its own array of `count` entries
            // (snow.c's `sSnowData->fbIdx`), valid for the call.
            let idx = unsafe { std::slice::from_raw_parts(idx, count as usize) };
            f(fb, idx, color as u16);
        }
    });
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn osSpTaskLoad(_task: *mut c_void) {
    crate::first_call("osSpTaskLoad");
}

/// Starts a task. Audio tasks run (HLE) and complete at once. A gfx task
/// only becomes pending: the RSP/RDP run in parallel with the CPU on
/// hardware, so it executes (and posts SP + DP done) at the next switch
/// back to the OS root loop, or before the next interrupt is delivered
/// ([`run_pending_rsp`]). Running it synchronously here made every thread
/// the scheduler had just woken wait for the whole display-list HLE: the
/// audio thread's `osAiSetNextBuffer` came 2–5 ms after the retrace,
/// most of the game's ~4.5 ms AI lead (audio.md).
///
/// # Safety
/// `task` must point at `&OSScTask.list` of a scheduler task whose OSTask
/// word fields sit at the host-layout offsets read below.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn osSpTaskStartGo(task: *mut c_void) {
    crate::first_call("osSpTaskStartGo");
    // The scheduler never starts a task while the RSP is busy; be safe.
    run_pending_rsp();
    // SAFETY: the C passes `&OSScTask.list` (OSTask). Host layout: `type`
    // u32 at 0; pointers are 8 bytes (sptask.h), so `data_ptr` sits at
    // offset 88 and `data_size` at 96. `OSScTask.framebuffer` is 8 bytes
    // before the OSTask, `OSScTask.flags` 12 bytes before (sched.h).
    let ty = unsafe { task.cast::<u32>().read() };
    let t = task as usize;
    let info = GfxTaskInfo {
        ty,
        // SAFETY: fixed host-layout offset reads into the OSScTask the C
        // passed (see the comment above): `flags` is 12 bytes before it.
        flags: unsafe { ((t - 12) as *const u32).read() },
        // SAFETY: `data_ptr` sits at OSTask offset 88 (sptask.h).
        data_ptr: unsafe { ((t + 88) as *const usize).read() },
        // SAFETY: `data_size` sits at OSTask offset 96 (sptask.h).
        data_size: unsafe { ((t + 96) as *const u32).read() },
        // SAFETY: `OSScTask.framebuffer` is 8 bytes before the OSTask.
        framebuffer: unsafe { ((t - 8) as *const usize).read() },
    };
    if ty == M_GFXTASK {
        PENDING_GFX.with(|p| p.set(Some((task as usize, info))));
    } else {
        run_task(task, &info);
    }
}

/// Runs the pending gfx task, if any, and posts its SP + DP done events.
/// The OS core calls this whenever control returns to its root loop and
/// before it delivers an interrupt, so the task still finishes before the
/// next retrace/timer is seen (as the scheduler expects), just after the
/// threads it woke had their turn.
/// Returns whether one ran.
pub fn run_pending_rsp() -> bool {
    let Some((task, info)) = PENDING_GFX.with(|p| p.take()) else {
        return false;
    };
    run_task(task as *mut c_void, &info);
    true
}

fn run_task(task: *mut c_void, info: &GfxTaskInfo) {
    let ty = info.ty;
    // `PW64_PROFILE_RETRACES`: time the HLE of this task (hooks included).
    let started = profile_on().then(std::time::Instant::now);
    if ty == M_GFXTASK {
        GFX_TASK.with(|g| {
            if let Some(g) = g.borrow_mut().as_mut() {
                g(info);
            }
        });
    } else if ty == M_AUDTASK {
        AUDIO_TASK.with(|a| {
            if let Some(a) = a.borrow_mut().as_mut() {
                a(info);
            }
        });
    }
    if let Some(t0) = started {
        let ns = t0.elapsed().as_nanos() as u64;
        use std::sync::atomic::Ordering::Relaxed;
        if ty == M_GFXTASK {
            GFX_N.fetch_add(1, Relaxed);
            GFX_NS.fetch_add(ns, Relaxed);
        } else if ty == M_AUDTASK {
            AUD_N.fetch_add(1, Relaxed);
            AUD_NS.fetch_add(ns, Relaxed);
        }
    }
    SP_HOOK.with(|h| {
        if let Some(h) = h.borrow_mut().as_mut() {
            h(task, ty);
        }
    });
    // Both done events before any switch: from a checkpoint in a low-priority
    // thread, SP done wakes the scheduler, which would preempt the caller
    // before DP done is posted, and a retrace delivered meanwhile finds the
    // RDP still busy (`_uvScHandleRetrace` then won't swap/start: a frame late).
    with(|k| {
        k.post_event(OS_EVENT_SP);
        if ty == M_GFXTASK {
            k.post_event(OS_EVENT_DP);
        }
    });
    reschedule();
}

/// Tasks are never yielded: a (still pending) gfx task runs to completion,
/// and its SP done then reads as "done, not yielded" (`osSpTaskYielded`).
#[unsafe(no_mangle)]
pub extern "C-unwind" fn osSpTaskYield() {}

/// `OSYieldResult`: 0 = the task was not yielded.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn osSpTaskYielded(_task: *mut c_void) -> u32 {
    0
}

const MAXCONTROLLERS: usize = 4;
const CONT_TYPE_NORMAL: u16 = 0x0005;
const CONT_NO_RESPONSE_ERROR: u8 = 0x8;

/// libultra `OSContStatus`.
#[repr(C)]
pub struct OSContStatus {
    pub ty: u16,
    pub status: u8,
    pub errno: u8,
}

/// libultra `OSContPad` (6 bytes).
#[repr(C)]
pub struct OSContPad {
    pub button: u16,
    pub stick_x: i8,
    pub stick_y: i8,
    pub errno: u8,
}

/// Mirrors `osContInit`: the first `present` controllers (set via
/// `set_controllers_present`) report as present, the rest absent.
///
/// # Safety
/// `bitpattern` must be a writable u8 and `status` MAXCONTROLLERS writable
/// `OSContStatus` slots.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn osContInit(
    _mq: *mut c_void,
    bitpattern: *mut u8,
    status: *mut OSContStatus,
) -> i32 {
    crate::first_call("osContInit");
    let present = PRESENT.load(std::sync::atomic::Ordering::Relaxed) as usize;
    // SAFETY: C passes a u8 and MAXCONTROLLERS status slots.
    unsafe {
        *bitpattern = ((1u16 << present) - 1) as u8;
        for i in 0..MAXCONTROLLERS {
            status.add(i).write(if i < present {
                OSContStatus {
                    ty: CONT_TYPE_NORMAL,
                    status: 0,
                    errno: 0,
                }
            } else {
                OSContStatus {
                    ty: 0,
                    status: 0,
                    errno: CONT_NO_RESPONSE_ERROR,
                }
            });
        }
    }
    0
}

/// Mirrors `osContStartReadData`: the read "completes" at once (SI event).
#[unsafe(no_mangle)]
pub extern "C-unwind" fn osContStartReadData(_mq: *mut c_void) -> i32 {
    crate::first_call("osContStartReadData");
    post_event(OS_EVENT_SI);
    0
}

/// Controllers as the host last published them: index = N64 controller
/// number - 1, each `button | stick_x << 16 | stick_y << 24`. One atomic
/// word per pad so the input thread and the OS core never see a torn pad.
/// 0 = idle (the headless default).
static PADS: [std::sync::atomic::AtomicU32; MAXCONTROLLERS] =
    [const { std::sync::atomic::AtomicU32::new(0) }; MAXCONTROLLERS];

/// How many controllers `osContInit` reports as present (and
/// `osContGetReadData` delivers): 1 while the game is single-player,
/// 2 with split-screen input. Must be set before the game boots —
/// `osContInit` runs once.
static PRESENT: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(1);

/// Sets how many controllers are present (`1..=4`, clamped). Call once
/// before the game boots (`osContInit` runs once).
pub fn set_controllers_present(n: u8) {
    PRESENT.store(
        n.clamp(1, MAXCONTROLLERS as u8),
        std::sync::atomic::Ordering::Relaxed,
    );
}

/// Host input (the `pw64` exe's `input` module) publishes controller `n`
/// (0-based) here; `osContGetReadData` delivers it. Calls for a controller
/// beyond `MAXCONTROLLERS` are ignored. Callable from any thread.
pub fn set_controller(n: usize, button: u16, stick_x: i8, stick_y: i8) {
    use std::sync::atomic::Ordering::Relaxed;
    if let Some(pad) = PADS.get(n) {
        let v = button as u32 | (stick_x as u8 as u32) << 16 | (stick_y as u8 as u32) << 24;
        let old = pad.swap(v, Relaxed) as u16;
        let newly = button & !old;
        if newly != 0 {
            LATCH_AT[n].store(READS.load(Relaxed), Relaxed);
            LATCH[n].fetch_or(newly as u32, Relaxed);
            // The game last saw these down (a release and a new press both
            // fell between two reads): give it one "up" read first, or the
            // second press of a quick double tap would not be an edge.
            let gap = newly as u32 & DELIVERED[n].load(Relaxed);
            GAP[n].fetch_or(gap, Relaxed);
        }
    }
}

/// Drops latched presses (and pending "up" reads): the host calls it when
/// the game's input is rerouted (pause / settings overlay, focus loss,
/// suspend), so a press made meanwhile can't reach the game on resume.
pub fn clear_latch() {
    use std::sync::atomic::Ordering::Relaxed;
    for (latch, gap) in LATCH.iter().zip(&GAP) {
        latch.store(0, Relaxed);
        gap.store(0, Relaxed);
    }
}

/// Buttons pressed since the last reads (per pad): a press released before
/// the game polled (a quick screen tap or key tap is shorter than a game
/// frame) still reaches the game once. `LATCH_AT` = `READS` when latched.
static LATCH: [std::sync::atomic::AtomicU32; MAXCONTROLLERS] =
    [const { std::sync::atomic::AtomicU32::new(0) }; MAXCONTROLLERS];
static LATCH_AT: [std::sync::atomic::AtomicU64; MAXCONTROLLERS] =
    [const { std::sync::atomic::AtomicU64::new(0) }; MAXCONTROLLERS];
/// Latched buttons first delivered as "up" for `PRESENT` reads (see
/// `set_controller`); the latch then counts its reads from there.
static GAP: [std::sync::atomic::AtomicU32; MAXCONTROLLERS] =
    [const { std::sync::atomic::AtomicU32::new(0) }; MAXCONTROLLERS];
/// The buttons each pad's last read delivered.
static DELIVERED: [std::sync::atomic::AtomicU32; MAXCONTROLLERS] =
    [const { std::sync::atomic::AtomicU32::new(0) }; MAXCONTROLLERS];
/// `osContGetReadData` calls so far. `uvReadController` reads all pads once
/// per controller it polls, so a latch is cleared only after `PRESENT`
/// reads: every polled controller's own read then saw it.
static READS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Controller 1 shorthand (`set_controller(0, ..)`): every existing caller
/// and test publishes controller 1.
pub fn set_controller1(button: u16, stick_x: i8, stick_y: i8) {
    set_controller(0, button, stick_x, stick_y);
}

/// Mirrors `osContGetReadData`: the first `present` controllers carry the
/// host snapshots (`set_controller` / `set_controller1`), the rest are
/// absent.
///
/// # Safety
/// `pads` must be MAXCONTROLLERS writable `OSContPad` slots.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn osContGetReadData(pads: *mut OSContPad) {
    crate::first_call("osContGetReadData");
    let v = PADS[0].load(std::sync::atomic::Ordering::Relaxed);
    if v != 0 {
        static LOGGED: std::sync::Once = std::sync::Once::new();
        LOGGED.call_once(|| eprintln!("[input] first non-idle pad delivered: {v:#010x}"));
    }
    let present = PRESENT.load(std::sync::atomic::Ordering::Relaxed) as usize;
    let reads = READS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    for (i, snapshot) in PADS.iter().enumerate() {
        let pad = if i < present {
            use std::sync::atomic::Ordering::Relaxed;
            let v = snapshot.load(Relaxed);
            let latched = LATCH[i].load(Relaxed);
            let gap = GAP[i].load(Relaxed);
            let button = (v as u16 | latched as u16) & !(gap as u16);
            if reads - LATCH_AT[i].load(Relaxed) >= present as u64 {
                if gap != 0 {
                    // The "up" reads are done: the press counts from here.
                    GAP[i].fetch_and(!gap, Relaxed);
                    LATCH_AT[i].store(reads, Relaxed);
                } else if latched != 0 {
                    LATCH[i].fetch_and(!latched, Relaxed);
                }
            }
            DELIVERED[i].store(button as u32, Relaxed);
            OSContPad {
                button,
                stick_x: (v >> 16) as u8 as i8,
                stick_y: (v >> 24) as u8 as i8,
                errno: 0,
            }
        } else {
            OSContPad {
                button: 0,
                stick_x: 0,
                stick_y: 0,
                errno: CONT_NO_RESPONSE_ERROR,
            }
        };
        // SAFETY: C passes MAXCONTROLLERS pads.
        unsafe { pads.add(i).write(pad) };
    }
}

/// Size of the backed EEPROM (`osEepromProbe` reports it present). The
/// game uses 2 × 0x100-byte save files (`uvFileRead`/`Write` bound it at
/// 0x208). 0x800 bytes = a 16 Kbit EEPROM, the size emulators write `.eep`
/// files at; shorter files (4 Kbit, 0x200) load as a prefix.
const EEP_SIZE: usize = 0x800;

/// The host's default `.eep` location, used when `PW64_EEP` is unset (the
/// `pw64` exe points it at its data dir via `set_default_eep_path` before
/// booting the OS core; anything else keeps `pw64.eep` in the cwd).
static DEFAULT_EEP_PATH: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// The on-disk `.eep` could not be read this session (S5: a sharing
/// violation from antivirus/backup, an offline OneDrive placeholder,
/// permissions): while this stands, writes never touch the file, so a save
/// the game cannot currently see cannot be wiped by a zero-image rewrite.
static EEP_READ_FAILED: AtomicBool = AtomicBool::new(false);

/// The first file write of the session has backed the on-disk image up to
/// `<eep>.bak` yet (S5).
static EEP_BACKED_UP: AtomicBool = AtomicBool::new(false);

/// The "couldn't save" toast was shown this session (S6): once, not per save.
static SAVE_FAIL_TOASTED: AtomicBool = AtomicBool::new(false);

/// [`eep_edit_and_freeze`] or [`import_eep`] replaced the save: the game
/// still holds the old saves in RAM, so from then on its writes are dropped
/// (neither the file nor the image an export hands out gets them) until the
/// process exits: a restart, or on Android the player reopening the app.
static EEP_FROZEN: AtomicBool = AtomicBool::new(false);

/// The save file could not be read this session (writes are skipped to
/// protect it); host-side save edits must not run either.
pub fn eep_read_failed() -> bool {
    EEP_READ_FAILED.load(Ordering::Relaxed)
}

/// What [`eep_edit_and_freeze`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EepEdit {
    /// The edit changed nothing: no backup, no write, not frozen.
    Unchanged,
    /// The file now holds the edited image; the previous image is in
    /// `backup` (a new file, never an existing one overwritten).
    Written { backup: std::path::PathBuf },
}

/// Edits the save image from outside the game (the settings screen's
/// "Unlock everything"), for a restart to load. `edit` gets a copy of the
/// image and returns whether it changed it. When it did: the old image is
/// written to a fresh `<eep><backup_suffix>` (then `-2`, `-3`, … so an older
/// backup is never overwritten), the new one replaces the file atomically,
/// and further game writes are kept off the disk for the rest of the
/// process (the game still holds the old saves in RAM). Any failure leaves
/// the file as it was. Refused while the file couldn't be read.
pub fn eep_edit_and_freeze(
    edit: impl FnOnce(&mut [u8]) -> bool,
    backup_suffix: &str,
) -> Result<EepEdit, String> {
    if EEP_READ_FAILED.load(Ordering::Relaxed) {
        return Err("the save file couldn't be read this session".into());
    }
    if EEP_FROZEN.load(Ordering::Relaxed) {
        return Err("the save was already replaced: start Birdman64 again first".into());
    }
    let path = eep_path();
    let mut data = eep_data().lock().unwrap();
    let r = edit_image(&path, &mut data, edit, backup_suffix);
    if matches!(r, Ok(EepEdit::Written { .. })) {
        EEP_FROZEN.store(true, Ordering::Relaxed);
    }
    r
}

/// `<eep><suffix>`: where [`eep_edit_and_freeze`] puts its first backup
/// (the later ones get `-2`, `-3`, …).
pub fn eep_backup_path(suffix: &str) -> std::path::PathBuf {
    let mut name = eep_path().into_os_string();
    name.push(suffix);
    std::path::PathBuf::from(name)
}

/// A backup [`eep_edit_and_freeze`] wrote, read back as a whole EEPROM
/// image. Anything but a readable file of exactly `EEP_SIZE` bytes (what
/// every backup is) is refused, so a stray or truncated file never becomes
/// the save.
pub fn eep_read_backup(path: &std::path::Path) -> Result<[u8; EEP_SIZE], String> {
    let bytes = std::fs::read(path).map_err(|e| format!("couldn't read the backup: {e}"))?;
    <[u8; EEP_SIZE]>::try_from(bytes.as_slice()).map_err(|_| {
        format!(
            "the backup is {} bytes, not a {EEP_SIZE}-byte save",
            bytes.len()
        )
    })
}

/// [`eep_edit_and_freeze`]'s body on an explicit path + image (testable
/// without the process-wide EEPROM cache). `data` is updated only after the
/// file write succeeded.
fn edit_image(
    path: &std::path::Path,
    data: &mut [u8; EEP_SIZE],
    edit: impl FnOnce(&mut [u8]) -> bool,
    backup_suffix: &str,
) -> Result<EepEdit, String> {
    let mut new = *data;
    if !edit(&mut new) || new == *data {
        return Ok(EepEdit::Unchanged);
    }
    let backup = write_new_backup(path, &data[..], backup_suffix)
        .map_err(|e| format!("couldn't back up the save file: {e}"))?;
    write_atomic(path, &new).map_err(|e| format!("couldn't write the save file: {e}"))?;
    *data = new;
    Ok(EepEdit::Written { backup })
}

/// Writes `bytes` to the first free `<path><suffix>`, `<path><suffix>-2`, …
/// (`create_new`: an existing backup is never replaced), synced to disk.
fn write_new_backup(
    path: &std::path::Path,
    bytes: &[u8],
    suffix: &str,
) -> std::io::Result<std::path::PathBuf> {
    use std::io::Write;
    for n in 1..1000 {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        if n > 1 {
            name.push(format!("-{n}"));
        }
        let p = std::path::PathBuf::from(name);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&p)
        {
            Ok(mut f) => {
                let r = f.write_all(bytes).and_then(|()| f.sync_all());
                if let Err(e) = r {
                    drop(f);
                    let _ = std::fs::remove_file(&p);
                    return Err(e);
                }
                return Ok(p);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("too many backups"))
}

/// The current save image (the whole `EEP_SIZE`), for an export. `Err` when
/// the file could not be read this session: the image is then blank, and
/// exporting it would hand the player an empty "backup".
pub fn export_eep() -> Result<Vec<u8>, String> {
    let data = eep_data().lock().unwrap();
    if EEP_READ_FAILED.load(Ordering::Relaxed) {
        return Err("Birdman64 couldn't read your save file this session.".to_string());
    }
    Ok(data.to_vec())
}

/// Replaces the save with `bytes` (a `.eep` from another Birdman64 or an
/// emulator: exactly 0x200 or 0x800 bytes, not blank). The old file is kept
/// as `<eep>.before-import`. The host must restart afterwards: the running
/// game keeps its old save in RAM, and further game writes are dropped.
pub fn import_eep(bytes: &[u8]) -> Result<(), String> {
    if bytes.len() != 0x200 && bytes.len() != EEP_SIZE {
        return Err(format!(
            "That file is {} bytes; a Pilotwings 64 save (.eep) is 2048 bytes.",
            bytes.len()
        ));
    }
    if bytes.iter().all(|&b| b == 0) {
        return Err("That save file is empty.".to_string());
    }
    let image = eep_image(bytes);
    let mut data = eep_data().lock().unwrap();
    let path = eep_path();
    let mut old = path.as_os_str().to_owned();
    old.push(".before-import");
    if let Err(e) = std::fs::copy(&path, &old)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        return Err(format!("Couldn't back up your current save: {e}"));
    }
    write_atomic(&path, &image).map_err(|e| format!("Couldn't write the save: {e}"))?;
    *data = image;
    EEP_FROZEN.store(true, Ordering::Relaxed);
    Ok(())
}

/// Sets the fallback EEPROM path for when `PW64_EEP` is unset. Call before
/// the EEPROM is first used (i.e. before `os::boot`); later calls are
/// ignored. `PW64_EEP` keeps priority over this.
pub fn set_default_eep_path(p: std::path::PathBuf) {
    let _ = DEFAULT_EEP_PATH.set(p);
}

/// `PW64_EEP`, else the host default (`set_default_eep_path`), else
/// `pw64.eep` in the current directory.
fn eep_path() -> std::path::PathBuf {
    std::env::var_os("PW64_EEP")
        .map(std::path::PathBuf::from)
        .or_else(|| DEFAULT_EEP_PATH.get().cloned())
        .unwrap_or_else(|| std::path::PathBuf::from("pw64.eep"))
}

/// A save image read from disk becomes the EEPROM contents: the first
/// `EEP_SIZE` bytes, zero-filled when the file is shorter (a 4 Kbit = 0x200
/// emulator file loads as a prefix). No header, no padding — save.c's two
/// `PilotwingsSaveFile`s sit at offsets 0x0/0x100, which is also the exact
/// layout of Project64/mupen64plus `.eep` files, so those can be dropped in
/// as-is. Files longer than `EEP_SIZE` keep only the first 0x800 bytes.
fn eep_image(bytes: &[u8]) -> [u8; EEP_SIZE] {
    let mut data = [0u8; EEP_SIZE];
    let n = bytes.len().min(EEP_SIZE);
    data[..n].copy_from_slice(&bytes[..n]);
    data
}

/// The EEPROM contents, loaded once from (or created as zeros in) the file.
fn eep_data() -> &'static Mutex<[u8; EEP_SIZE]> {
    use std::sync::OnceLock;
    static EEP: OnceLock<Mutex<[u8; EEP_SIZE]>> = OnceLock::new();
    EEP.get_or_init(|| {
        let path = eep_path();
        let data = match std::fs::read(&path) {
            Ok(bytes) => eep_image(&bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if let Err(e) = std::fs::write(&path, [0u8; EEP_SIZE]) {
                    eprintln!("[platform] could not create {path:?}: {e}");
                } else {
                    eprintln!("[platform] created EEPROM file {}", path.display());
                }
                [0u8; EEP_SIZE]
            }
            Err(e) => {
                eprintln!("[platform] {path:?}: {e}");
                EEP_READ_FAILED.store(true, Ordering::Relaxed);
                notify_toast(
                    "Birdman64 couldn't read your save file (pw64.eep), so it won't save this \
                     session to protect it. Close programs that may lock it (backup or antivirus) \
                     and restart."
                        .to_string(),
                );
                [0u8; EEP_SIZE]
            }
        };
        Mutex::new(data)
    })
}

/// Mirrors `osEepromProbe`: EEPROM present (`CONT_EEPROM`), unless
/// `PW64_NO_EEPROM` restores the old "not found" flow.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn osEepromProbe(_mq: *mut c_void) -> i32 {
    crate::first_call("osEepromProbe");
    if std::env::var_os("PW64_NO_EEPROM").is_some() {
        0
    } else {
        1
    }
}

/// Mirrors `osEepromLongRead` (address is in 8-byte blocks, length a
/// multiple of `EEPROM_BLOCK_SIZE`). Backed by the `.eep` file.
///
/// # Safety
/// `buffer` must be writable for `length` bytes; `mq` is ignored (null fine).
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn osEepromLongRead(
    _mq: *mut c_void,
    address: u8,
    buffer: *mut u8,
    length: i32,
) -> i32 {
    crate::first_call("osEepromLongRead");
    let (off, len) = (address as usize * 8, length.max(0) as usize);
    if off + len > EEP_SIZE {
        return -1;
    }
    // SAFETY: the C passes a buffer of `length` bytes.
    let dst = unsafe { std::slice::from_raw_parts_mut(buffer, len) };
    let data = eep_data().lock().unwrap();
    dst.copy_from_slice(&data[off..off + len]);
    0
}

/// Mirrors `osEepromLongWrite` (address is in 8-byte blocks, length a
/// multiple of `EEPROM_BLOCK_SIZE`). Backed by the `.eep` file.
///
/// # Safety
/// `buffer` must be readable for `length` bytes; `mq` is ignored (null fine).
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn osEepromLongWrite(
    _mq: *mut c_void,
    address: u8,
    buffer: *mut u8,
    length: i32,
) -> i32 {
    crate::first_call("osEepromLongWrite");
    let (off, len) = (address as usize * 8, length.max(0) as usize);
    if off + len > EEP_SIZE {
        return -1;
    }
    if EEP_FROZEN.load(Ordering::Relaxed) {
        return 0; // a replaced save is waiting for the restart / reopen
    }
    // SAFETY: the C passes a buffer of `length` bytes.
    let src = unsafe { std::slice::from_raw_parts(buffer, len) };
    let path = eep_path();
    {
        let mut data = eep_data().lock().unwrap();
        data[off..off + len].copy_from_slice(src);
        if let Err(e) = persist(
            &path,
            &*data,
            EEP_READ_FAILED.load(Ordering::Relaxed),
            &EEP_BACKED_UP,
        ) {
            eprintln!("[platform] EEPROM write to {path:?} failed: {e}");
            if !SAVE_FAIL_TOASTED.swap(true, Ordering::Relaxed) {
                notify_toast(format!("Couldn't save your progress: {e}"));
            }
            return -1;
        }
        // A failed write leaves the file stale: persist always writes the
        // full image, so the next save catches the file up on its own.
    }
    0
}

/// One save-image write to disk. `read_failed` (S5): the on-disk file could
/// not be read this session, so nothing is written and `Ok` is returned (the
/// caller keeps the image in memory and reports success). Otherwise the first
/// call copies the old file to `<path>.bak` (`backed_up` flips once), then
/// [`write_atomic`] replaces it.
fn persist(
    path: &std::path::Path,
    bytes: &[u8],
    read_failed: bool,
    backed_up: &AtomicBool,
) -> std::io::Result<()> {
    if read_failed {
        return Ok(());
    }
    if !backed_up.swap(true, Ordering::Relaxed) {
        let mut bak = path.as_os_str().to_owned();
        bak.push(".bak");
        if let Err(e) = std::fs::copy(path, &bak)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!(
                "[platform] could not back up {} to {}: {e}",
                path.display(),
                bak.display()
            );
        }
    }
    write_atomic(path, bytes)
}

/// Whether a failed rename can succeed on a retry: access denied, or the
/// Windows sharing/lock violations (32/33) another program's brief hold of
/// the file produces (antivirus, backup, sync clients).
fn rename_retryable(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::PermissionDenied || matches!(e.raw_os_error(), Some(32 | 33))
}

/// Temp file + rename, so a crash or power loss mid-write never leaves a
/// truncated save (the rename replaces the old file in one step). The temp
/// file is synced to disk before the rename, and a rename blocked by another
/// program is retried up to 10 times over about 2 s before giving up.
fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    const ATTEMPTS: u32 = 10;
    let mut last_err = None;
    for attempt in 0..ATTEMPTS {
        match std::fs::rename(&tmp, path) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_err = Some(e);
                if attempt + 1 < ATTEMPTS && rename_retryable(last_err.as_ref().unwrap()) {
                    std::thread::sleep(std::time::Duration::from_millis(220));
                } else {
                    break;
                }
            }
        }
    }
    let _ = std::fs::remove_file(&tmp);
    Err(last_err.unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controller1_snapshot_round_trips() {
        set_controller1(0x8010, -80, 42);
        let mut pads: [OSContPad; MAXCONTROLLERS] = std::array::from_fn(|_| OSContPad {
            button: 0,
            stick_x: 0,
            stick_y: 0,
            errno: 0xff,
        });
        // SAFETY: `pads` is a MAXCONTROLLERS-sized array, the buffer the C
        // caller would pass.
        unsafe { osContGetReadData(pads.as_mut_ptr()) };
        set_controller1(0, 0, 0);
        assert_eq!(
            (
                pads[0].button,
                pads[0].stick_x,
                pads[0].stick_y,
                pads[0].errno
            ),
            (0x8010, -80, 42, 0)
        );
        // Single-player (the default): a published controller 2 is not
        // delivered, and `osContInit` reports pad 1 only.
        set_controller(1, 0x8010, -80, 42);
        // SAFETY: scratch buffer as above.
        unsafe { osContGetReadData(pads.as_mut_ptr()) };
        assert_eq!(pads[1].errno, CONT_NO_RESPONSE_ERROR);
        assert_eq!(pads[1].button, 0);
        let mut pattern: u8 = 0;
        let mut status: [OSContStatus; MAXCONTROLLERS] = std::array::from_fn(|_| OSContStatus {
            ty: 0,
            status: 0,
            errno: 0xff,
        });
        // SAFETY: scratch buffers the C caller would pass.
        unsafe { osContInit(std::ptr::null_mut(), &mut pattern, status.as_mut_ptr()) };
        assert_eq!(pattern, 0b1);
        assert_eq!(status[1].errno, CONT_NO_RESPONSE_ERROR);
        // Split-screen: two present — controller 2 delivers, `osContInit`
        // reports pads 1-2.
        set_controllers_present(2);
        // SAFETY: scratch buffer as above.
        unsafe { osContGetReadData(pads.as_mut_ptr()) };
        assert_eq!(
            (
                pads[1].button,
                pads[1].stick_x,
                pads[1].stick_y,
                pads[1].errno
            ),
            (0x8010, -80, 42, 0)
        );
        // SAFETY: scratch buffers as above.
        unsafe { osContInit(std::ptr::null_mut(), &mut pattern, status.as_mut_ptr()) };
        assert_eq!(pattern, 0b11);
        assert_eq!(status[1].errno, 0);
        assert_eq!(status[2].errno, CONT_NO_RESPONSE_ERROR);
        // A tap released before any read is delivered once per polled
        // controller (two reads with two present), then cleared.
        set_controller(1, 0, 0, 0);
        set_controller1(0x8000, 0, 0);
        set_controller1(0, 0, 0);
        let mut seen = [0u16; 3];
        for s in &mut seen {
            // SAFETY: scratch buffer as above.
            unsafe { osContGetReadData(pads.as_mut_ptr()) };
            *s = pads[0].button;
        }
        assert_eq!(seen, [0x8000, 0x8000, 0]);
        // Double tap: the 2nd tap lands right after the 1st was delivered
        // (both before the game saw A up): one "up" pair of reads, then the
        // 2nd press, so the game sees two edges.
        let read = |pads: &mut [OSContPad; MAXCONTROLLERS]| {
            // SAFETY: scratch buffer as above.
            unsafe { osContGetReadData(pads.as_mut_ptr()) };
            pads[0].button
        };
        set_controller1(0x8000, 0, 0);
        set_controller1(0, 0, 0);
        assert_eq!([read(&mut pads), read(&mut pads)], [0x8000, 0x8000]);
        set_controller1(0x8000, 0, 0);
        set_controller1(0, 0, 0);
        let seen: Vec<u16> = (0..6).map(|_| read(&mut pads)).collect();
        assert_eq!(seen, [0, 0, 0x8000, 0x8000, 0, 0]);
        // A latched press dropped by `clear_latch` never arrives.
        set_controller1(0x8000, 0, 0);
        set_controller1(0, 0, 0);
        clear_latch();
        assert_eq!(read(&mut pads), 0);
        // Restore the globals for the other tests (they share the statics).
        set_controllers_present(1);
        set_controller(1, 0, 0, 0);
        assert_eq!(std::mem::size_of::<OSContPad>(), 6);
    }

    /// Emulator `.eep` import (synthetic bytes only): a Project64 /
    /// mupen64plus 16 Kbit file is our exact on-disk layout — save.c
    /// `PilotwingsSaveFile` magic at offset 0, file 2 at 0x100, checksum byte
    /// at 0xFF, no header or padding.
    #[test]
    fn eep_image_accepts_emulator_layout() {
        let mut file = vec![0u8; 0x800];
        file[0] = b'P';
        file[1] = b'W';
        file[0xFF] = 0x42; // save.c checksum byte
        file[0x100] = b'P';
        file[0x101] = b'W';
        file[0x1FF] = 0x7E;
        let data = eep_image(&file);
        assert_eq!(&data[..2], b"PW");
        assert_eq!(data[0xFF], 0x42);
        assert_eq!(&data[0x100..0x102], b"PW");
        assert_eq!(data[0x1FF], 0x7E);
        // A 4 Kbit (0x200) file loads as the prefix of the same image.
        let small = eep_image(&file[..0x200]);
        assert_eq!(&small[..0x200], &file[..0x200]);
        assert!(small[0x200..].iter().all(|&b| b == 0));
        // An oversized file keeps only the first 0x800 bytes.
        let mut big = file.clone();
        big.extend_from_slice(&[0xAA; 0x10]);
        assert_eq!(eep_image(&big), data);
        // Short files are zero-filled to 0x800.
        let tiny = eep_image(b"PW");
        assert_eq!(&tiny[..2], b"PW");
        assert!(tiny[2..].iter().all(|&b| b == 0));
    }

    /// Writes land on disk and are read back (the persistence path behind the
    /// game's `saveFileWrite` → `uvFileWrite` → `osEepromLongWrite`).
    /// Synthetic bytes in a temp file; the env var must be set before the
    /// EEPROM cache initializes (no other test touches it).
    #[test]
    fn eeprom_writes_persist_to_disk() {
        let tmp = std::env::temp_dir().join("pw64-eep-write-test.bin");
        let _ = std::fs::remove_file(&tmp);
        std::fs::write(&tmp, [0u8; EEP_SIZE]).unwrap();
        // SAFETY: this test is the only user of the EEPROM cache and runs
        // before it is initialized.
        unsafe { std::env::set_var("PW64_EEP", &tmp) };
        // save.c `saveFileInit(0)`: magic 'p','w' written at file offset 0.
        let mut block = [0u8; 0x100];
        block[0] = b'p';
        block[1] = b'w';
        // SAFETY: the backend ignores the mq argument (null is fine); the
        // block buffer is valid for the full 0x100 bytes.
        let r = unsafe { osEepromLongWrite(std::ptr::null_mut(), 0, block.as_mut_ptr(), 0x100) };
        assert_eq!(r, 0);
        // ... and `saveFileInit(1)`: block address 0x20 = byte offset 0x100.
        // SAFETY: as above; the second block goes to EEPROM address 0x20
        // (byte offset 0x100).
        let r = unsafe { osEepromLongWrite(std::ptr::null_mut(), 0x20, block.as_mut_ptr(), 0x100) };
        assert_eq!(r, 0);
        let on_disk = std::fs::read(&tmp).unwrap();
        assert_eq!(on_disk.len(), EEP_SIZE);
        assert_eq!(&on_disk[..2], b"pw");
        assert_eq!(&on_disk[0x100..0x102], b"pw");
        assert!(on_disk[2..0x100].iter().all(|&b| b == 0));
        assert!(on_disk[0x102..EEP_SIZE].iter().all(|&b| b == 0));
        // Read back through the same backend.
        let mut buf = [0u8; 0x200];
        // SAFETY: reads the first 0x200 bytes into the buffer above (the mq
        // argument is ignored, null is fine).
        let r = unsafe { osEepromLongRead(std::ptr::null_mut(), 0, buf.as_mut_ptr(), 0x200) };
        assert_eq!(r, 0);
        assert_eq!(&buf[..2], b"pw");
        assert_eq!(&buf[0x100..0x102], b"pw");
        // S5: the first write backed the then-zero image up exactly once.
        let mut bak = tmp.as_os_str().to_owned();
        bak.push(".bak");
        let bak = std::path::PathBuf::from(bak);
        assert_eq!(std::fs::read(&bak).unwrap(), vec![0u8; EEP_SIZE]);

        // Export hands out the image; import rejects odd sizes and blank
        // files, keeps the old save, pads a 0x200 file and then freezes
        // game writes (the same cache, so it lives in this test).
        let exported = export_eep().unwrap();
        assert_eq!(exported, on_disk);
        assert!(import_eep(&[1; 100]).is_err());
        assert!(import_eep(&[0; EEP_SIZE]).is_err());
        let mut other = vec![0u8; 0x200];
        other[0] = b'x';
        import_eep(&other).unwrap();
        let mut before = tmp.as_os_str().to_owned();
        before.push(".before-import");
        let before = std::path::PathBuf::from(before);
        assert_eq!(std::fs::read(&before).unwrap(), on_disk);
        let now = std::fs::read(&tmp).unwrap();
        assert_eq!(now.len(), EEP_SIZE);
        assert_eq!(now[0], b'x');
        assert_eq!(export_eep().unwrap(), now);
        // SAFETY: as above; the block buffer is valid for 0x100 bytes.
        let r = unsafe { osEepromLongWrite(std::ptr::null_mut(), 0, block.as_mut_ptr(), 0x100) };
        assert_eq!(r, 0);
        assert_eq!(std::fs::read(&tmp).unwrap()[0], b'x', "writes are dropped");

        std::fs::remove_file(&tmp).unwrap();
        std::fs::remove_file(&bak).unwrap();
        std::fs::remove_file(&before).unwrap();
    }

    /// S5: with the read-failure guard standing, nothing is written: no
    /// `.bak` copy, no temp file, and the on-disk save keeps its contents.
    /// A directory named pw64.eep stands in for the unreadable file (a
    /// sharing violation or an offline placeholder read the same way).
    #[test]
    fn read_failure_guard_skips_writes_and_backup() {
        let dir = std::env::temp_dir().join("pw64-eep-guard-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let eep = dir.join("pw64.eep");
        std::fs::create_dir(&eep).unwrap();
        let e = std::fs::read(&eep).unwrap_err();
        assert_ne!(e.kind(), std::io::ErrorKind::NotFound, "reads fail");
        let backed_up = AtomicBool::new(false);
        persist(&eep, &[0xAA; EEP_SIZE], true, &backed_up).unwrap();
        assert!(!backed_up.load(Ordering::Relaxed), "no write, no backup");
        assert!(!dir.join("pw64.eep.bak").exists());
        assert!(!dir.join("pw64.eep.tmp").exists());
        assert!(eep.is_dir(), "on-disk save untouched");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// S5: the first write copies the old image to `<path>.bak` exactly once.
    #[test]
    fn first_write_backs_up_the_old_image() {
        let dir = std::env::temp_dir().join("pw64-eep-bak-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let eep = dir.join("pw64.eep");
        std::fs::write(&eep, b"old image").unwrap();
        let backed_up = AtomicBool::new(false);
        persist(&eep, b"new image", false, &backed_up).unwrap();
        assert!(backed_up.load(Ordering::Relaxed));
        assert_eq!(
            std::fs::read(dir.join("pw64.eep.bak")).unwrap(),
            b"old image"
        );
        assert_eq!(std::fs::read(&eep).unwrap(), b"new image");
        // A later write does not copy again.
        persist(&eep, b"newer image", false, &backed_up).unwrap();
        assert_eq!(
            std::fs::read(dir.join("pw64.eep.bak")).unwrap(),
            b"old image"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// S6: a rename blocked by another program (a file held with no sharing,
    /// as antivirus/backup tools do) is retried for about 2 s; the old save
    /// stays intact and no temp file is left behind. Windows-only: on Unix a
    /// rename over an open file always succeeds.
    #[cfg(windows)]
    #[test]
    fn write_atomic_retries_sharing_violations() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = std::env::temp_dir().join("pw64-eep-retry-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let eep = dir.join("pw64.eep");
        std::fs::write(&eep, b"old image").unwrap();
        // Hold the target with no sharing at all: replacing it fails.
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&eep)
            .unwrap();
        let started = std::time::Instant::now();
        let err = write_atomic(&eep, b"new image").unwrap_err();
        assert!(
            rename_retryable(&err),
            "the blocking error must be retryable: {err}"
        );
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(1500),
            "retried for about 2 s"
        );
        assert!(!dir.join("pw64.eep.tmp").exists(), "temp file cleaned up");
        drop(lock);
        assert_eq!(
            std::fs::read(&eep).unwrap(),
            b"old image",
            "old save intact"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A host-side save edit: no-op edits write nothing; a real one backs
    /// the old image up to a new file (a second edit gets `-2`, the first
    /// backup survives), then replaces the save; a failed backup leaves
    /// both the file and the in-memory image alone.
    #[test]
    fn edit_image_backs_up_without_overwriting() {
        let dir = std::env::temp_dir().join("pw64-eep-edit-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let eep = dir.join("pw64.eep");
        let mut data = [0u8; EEP_SIZE];
        data[0] = 1;
        std::fs::write(&eep, data).unwrap();
        // Unchanged: nothing written.
        let r = edit_image(&eep, &mut data, |_| true, ".before-x");
        assert_eq!(r, Ok(EepEdit::Unchanged));
        assert!(!dir.join("pw64.eep.before-x").exists());
        // First edit.
        let r = edit_image(
            &eep,
            &mut data,
            |d| {
                d[0] = 2;
                true
            },
            ".before-x",
        );
        let b1 = dir.join("pw64.eep.before-x");
        assert_eq!(r, Ok(EepEdit::Written { backup: b1.clone() }));
        assert_eq!(std::fs::read(&b1).unwrap()[0], 1);
        assert_eq!(std::fs::read(&eep).unwrap()[0], 2);
        assert_eq!(data[0], 2);
        // Second edit: a new backup, the first one kept.
        let r = edit_image(
            &eep,
            &mut data,
            |d| {
                d[0] = 3;
                true
            },
            ".before-x",
        );
        let b2 = dir.join("pw64.eep.before-x-2");
        assert_eq!(r, Ok(EepEdit::Written { backup: b2.clone() }));
        assert_eq!(std::fs::read(&b1).unwrap()[0], 1);
        assert_eq!(std::fs::read(&b2).unwrap()[0], 2);
        assert_eq!(std::fs::read(&eep).unwrap()[0], 3);
        // A backup that can't be made (its directory is missing): the save
        // and the image stay as they are.
        let gone = dir.join("missing").join("pw64.eep");
        let r = edit_image(
            &gone,
            &mut data,
            |d| {
                d[0] = 4;
                true
            },
            ".before-x",
        );
        assert!(r.is_err());
        assert_eq!(data[0], 3);
        assert_eq!(std::fs::read(&eep).unwrap()[0], 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Restore = an edit that copies a backup back: an edit then a restore
    /// gives the original bytes; the restore's own backup never replaces an
    /// older one (`-2`); a backup of the wrong size or a missing one is
    /// refused before anything is written.
    #[test]
    fn restore_from_backup_round_trips() {
        let dir = std::env::temp_dir().join("pw64-eep-restore-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let eep = dir.join("pw64.eep");
        let original: [u8; EEP_SIZE] = std::array::from_fn(|i| i as u8);
        let mut data = original;
        std::fs::write(&eep, data).unwrap();
        let fill = |d: &mut [u8]| {
            d.fill(0x55);
            true
        };
        edit_image(&eep, &mut data, fill, ".before-u").unwrap();
        let ub = dir.join("pw64.eep.before-u");
        let backup = eep_read_backup(&ub).unwrap();
        let restore = |d: &mut [u8]| {
            d.copy_from_slice(&backup);
            true
        };
        let r = edit_image(&eep, &mut data, restore, ".before-r").unwrap();
        assert_eq!(
            r,
            EepEdit::Written {
                backup: dir.join("pw64.eep.before-r")
            }
        );
        assert_eq!(std::fs::read(&eep).unwrap(), original);
        assert_eq!(data, original);
        assert!(ub.exists(), "the unlock backup stays");
        // Unlock + restore again: the second restore backup gets -2.
        edit_image(&eep, &mut data, fill, ".before-u").unwrap();
        let r = edit_image(&eep, &mut data, restore, ".before-r").unwrap();
        assert_eq!(
            r,
            EepEdit::Written {
                backup: dir.join("pw64.eep.before-r-2")
            }
        );
        assert_eq!(
            std::fs::read(dir.join("pw64.eep.before-r")).unwrap(),
            [0x55; EEP_SIZE]
        );
        // Invalid backups are refused.
        std::fs::write(&ub, [0u8; 0x200]).unwrap();
        assert!(eep_read_backup(&ub).unwrap_err().contains("512 bytes"));
        assert!(eep_read_backup(&dir.join("missing")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
