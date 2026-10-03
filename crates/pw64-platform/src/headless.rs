//! SI / RSP backends (native-build.md §6 task 7; AI is in `ai.rs`):
//! - SP: audio tasks go to `set_audio_task_handler` (the `pw64` exe → the
//!   `pw64-audio` HLE) and complete with SP done. Gfx tasks become pending
//!   and later run ([`run_pending_rsp`], from the OS core) through a host
//!   hook (`set_gfx_task_handler` — the `pw64` exe wires it to `pw64-gfx`),
//!   then complete with SP + DP done.
//! - SI: one standard controller (idle unless the host publishes input via
//!   `set_controller1`); reads post `OS_EVENT_SI`.
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

/// Mirrors `osContInit`: controller 1 present, 2-4 absent.
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
    // SAFETY: C passes a u8 and MAXCONTROLLERS status slots.
    unsafe {
        *bitpattern = 1;
        for i in 0..MAXCONTROLLERS {
            status.add(i).write(if i == 0 {
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

/// Controller 1 as the host last published it: `button | stick_x << 16 |
/// stick_y << 24`. One atomic word so the input thread and the OS core never
/// see a torn pad. 0 = idle (the headless default).
static PAD1: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Host input (the `pw64` exe's `input` module) publishes controller 1 here;
/// `osContGetReadData` delivers it. Callable from any thread.
pub fn set_controller1(button: u16, stick_x: i8, stick_y: i8) {
    let v = button as u32 | (stick_x as u8 as u32) << 16 | (stick_y as u8 as u32) << 24;
    PAD1.store(v, std::sync::atomic::Ordering::Relaxed);
}

/// Mirrors `osContGetReadData`: controller 1 = the host snapshot
/// (`set_controller1`), 2-4 absent.
///
/// # Safety
/// `pads` must be MAXCONTROLLERS writable `OSContPad` slots.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn osContGetReadData(pads: *mut OSContPad) {
    crate::first_call("osContGetReadData");
    let v = PAD1.load(std::sync::atomic::Ordering::Relaxed);
    if v != 0 {
        static LOGGED: std::sync::Once = std::sync::Once::new();
        LOGGED.call_once(|| eprintln!("[input] first non-idle pad delivered: {v:#010x}"));
    }
    for i in 0..MAXCONTROLLERS {
        let pad = if i == 0 {
            OSContPad {
                button: v as u16,
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
        assert_eq!(pads[1].errno, CONT_NO_RESPONSE_ERROR);
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
        std::fs::remove_file(&tmp).unwrap();
        std::fs::remove_file(&bak).unwrap();
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
}
