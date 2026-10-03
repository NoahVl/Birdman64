//! Crash tracer: reports the faulting RIP and access address of a native
//! crash (native-build.md §7, §8) to stderr and the data dir's crash.log.
//! RIPs in the game module print as `pw64game+0x<off>` and the module's
//! `.map` path is logged (T14: the offsets are the map's linked addresses,
//! so a dev can rebuild the deterministic module and read the function off
//! the map). The exe itself has no symbols at crash time: link it with
//! `PW64_MAP=<path>` (pw64-game build.rs adds `/MAP`) and read its RIPs off
//! the map.
//!
//! Only real crashes are reported (a vectored handler sees every
//! first-chance exception of every thread, including ones a GPU driver,
//! overlay or CPU-probing library handles itself):
//! - The unhandled-exception filter reports anything that reaches it. It
//!   is NOT reached for faults on a coroutine stack: corosensei's unwind
//!   info links a coroutine to its parent stack for backtraces only, the
//!   SEH dispatcher stops at the stack boundary (the TEB limits are the
//!   coroutine's), and the process dies on the second chance without
//!   calling the filter.
//! - So the vectored handler also reports, but only error-severity
//!   exceptions whose RIP is in our code (exe image or game module, which
//!   have no SEH handlers for hardware faults) or whose RSP is on a
//!   coroutine stack (the dispatcher can't reach a handler above it).
//!
//! The report is formatted into a stack buffer and written with plain
//! Win32 calls: no allocation in the handler (heap may be the culprit).
//!
//! `PW64_WATCH=<addr>` arms an x64 hardware write watchpoint (DR0) on that
//! address: every 4-byte write logs RIP/RSP, which pins down who corrupts a
//! given stack word. It is armed by editing the CONTEXT RECORD of a raised
//! benign exception (legal — the OS applies the edited context on continue).

use core::ffi::c_void;
use core::fmt::Write as _;
use std::os::windows::ffi::OsStrExt as _;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use windows_sys::Win32::Foundation::EXCEPTION_ACCESS_VIOLATION;
use windows_sys::Win32::System::Diagnostics::Debug::{
    AddVectoredExceptionHandler, EXCEPTION_POINTERS, LPTOP_LEVEL_EXCEPTION_FILTER, RaiseException,
    SetUnhandledExceptionFilter,
};

mod sys {
    use core::ffi::c_void;
    unsafe extern "system" {
        pub fn GetModuleHandleW(name: *const u16) -> *mut c_void;
        pub fn GetStdHandle(which: u32) -> *mut c_void;
        pub fn CreateFileW(
            name: *const u16,
            access: u32,
            share: u32,
            sec: *const c_void,
            disposition: u32,
            flags: u32,
            template: *mut c_void,
        ) -> *mut c_void;
        pub fn WriteFile(
            h: *mut c_void,
            buf: *const u8,
            len: u32,
            written: *mut u32,
            overlapped: *mut c_void,
        ) -> i32;
        pub fn CloseHandle(h: *mut c_void) -> i32;
    }
    pub const STD_ERROR_HANDLE: u32 = -12i32 as u32;
    pub const FILE_APPEND_DATA: u32 = 0x4;
    /// FILE_SHARE_READ | WRITE | DELETE: same as std's OpenOptions.
    pub const SHARE_ALL: u32 = 0x7;
    pub const OPEN_ALWAYS: u32 = 4;
    pub const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
    pub const INVALID_HANDLE: *mut c_void = -1isize as *mut c_void;
}

/// crash.log as a NUL-terminated UTF-16 path (built once, outside the
/// handler; `pw64` calls this before booting the game, see main.rs).
static CRASH_LOG: OnceLock<Vec<u16>> = OnceLock::new();

/// Directs the native crash report to `path` (the data dir's `crash.log`).
/// Best effort: the handler below must never fail the crash.
pub fn set_crash_log_path(path: PathBuf) {
    let _ = CRASH_LOG.set(path.as_os_str().encode_wide().chain([0]).collect());
}

/// The loaded game module's image range and its `.map` file, set once the
/// dylib loader has the module up (`pw64-game` dylib.rs; empty range until
/// then, which matches nothing).
static MODULE_RANGE: (AtomicUsize, AtomicUsize) = (AtomicUsize::new(0), AtomicUsize::new(0));
/// The `.map` path, pre-rendered (no allocation in the handler).
static MODULE_MAP: OnceLock<String> = OnceLock::new();
/// This exe's image range (set by [`install`]).
static EXE_RANGE: (AtomicUsize, AtomicUsize) = (AtomicUsize::new(0), AtomicUsize::new(0));

/// Registers the loaded game module for crash symbolization: RIPs inside
/// `range` print as `pw64game+0x<off>` (the offset is the address to look up
/// in the `.map` the builder writes next to the module, first-run-build.md
/// T14). Called once, right after `pw64_game::dylib::load` succeeds.
pub fn set_game_module(range: (usize, usize), map: PathBuf) {
    MODULE_RANGE.0.store(range.0, Ordering::Relaxed);
    MODULE_RANGE.1.store(range.1, Ordering::Relaxed);
    let _ = MODULE_MAP.set(map.display().to_string());
}

fn load_range(r: &(AtomicUsize, AtomicUsize)) -> (usize, usize) {
    (r.0.load(Ordering::Relaxed), r.1.load(Ordering::Relaxed))
}

/// The module range as set: (0, 0) when unset (an empty range never
/// contains a RIP).
fn module_range() -> (usize, usize) {
    load_range(&MODULE_RANGE)
}

/// Pure RIP formatting for the crash lines (mirror: crash_linux.rs): inside
/// the module image `pw64game+0x<off>`, otherwise the raw address.
fn write_rip(
    out: &mut impl core::fmt::Write,
    rip: u64,
    range: (usize, usize),
) -> core::fmt::Result {
    if (range.0 as u64..range.1 as u64).contains(&rip) {
        write!(out, "pw64game+0x{:x}", rip - range.0 as u64)
    } else {
        write!(out, "0x{rip:x}")
    }
}

/// First-chance filter for the vectored handler (pure; see the module
/// docs): error severity, not a benign code, and either the RIP is in our
/// code or the RSP is on a coroutine stack.
fn is_our_crash(
    code: u32,
    rip: u64,
    rsp: u64,
    exe: (usize, usize),
    module: (usize, usize),
) -> bool {
    // C++ EH (0xE06D7363) and every non-error severity code (debug
    // strings, SetThreadName, RPC/COM status codes) are not crashes.
    if code == 0xE06D7363 || code & 0xC000_0000 != 0xC000_0000 {
        return false;
    }
    let inside = |r: (usize, usize), a: u64| (r.0 as u64..r.1 as u64).contains(&a);
    let stacks = (super::stack::SEARCH_START, super::stack::SEARCH_END);
    inside(exe, rip) || inside(module, rip) || inside(stacks, rsp)
}

/// Our own "arm the watchpoint" exception code (user codes are 0x8xxxxxxx).
const ARM_CODE: u32 = 0x8123_4567;
/// STATUS_SINGLE_STEP: fires for a debug register hit.
const STATUS_SINGLE_STEP: u32 = 0x8000_0004;
/// EXCEPTION_CONTINUE_EXECUTION.
const CONTINUE_EXEC: i32 = -1;
/// EXCEPTION_CONTINUE_SEARCH (0): let the default handler crash the process.
const CONTINUE_SEARCH: i32 = 0;

static WATCH_ADDR: AtomicU64 = AtomicU64::new(0);
static WATCH_HITS: AtomicU64 = AtomicU64::new(0);
/// One report per process: the vectored handler and the filter may both
/// see the same crash, and a fault inside the report must not recurse.
static REPORTED: AtomicBool = AtomicBool::new(false);
/// The filter that was installed before ours (chained).
static PREV_FILTER: AtomicUsize = AtomicUsize::new(0);

/// Formats into a fixed buffer: no allocation inside the handler.
struct Buf {
    b: [u8; 768],
    n: usize,
}

impl core::fmt::Write for Buf {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let k = s.len().min(self.b.len() - self.n);
        self.b[self.n..self.n + k].copy_from_slice(&s.as_bytes()[..k]);
        self.n += k;
        Ok(())
    }
}

/// Writes all of `bytes` to `h`, best effort.
fn write_handle(h: *mut c_void, bytes: &[u8]) {
    if h.is_null() || h == sys::INVALID_HANDLE {
        return;
    }
    let mut written = 0;
    // SAFETY: valid handle, buffer and length; synchronous write.
    unsafe {
        sys::WriteFile(
            h,
            bytes.as_ptr(),
            bytes.len() as u32,
            &mut written,
            core::ptr::null_mut(),
        )
    };
}

/// Prints the `[crash]` lines and appends them to crash.log (once).
fn report(info: &EXCEPTION_POINTERS) {
    if REPORTED.swap(true, Ordering::AcqRel) {
        return;
    }
    // SAFETY: the OS owns both records; valid while the handler runs.
    let (rec, ctx) = unsafe { (&*info.ExceptionRecord, info.ContextRecord.as_ref()) };
    let code = rec.ExceptionCode as u32;
    let (rip, rsp) = ctx.map_or((0, 0), |c| (c.Rip, c.Rsp));
    let mut buf = Buf { b: [0; 768], n: 0 };
    let _ = write!(buf, "[crash] exception 0x{code:08x} at RIP ");
    let _ = write_rip(&mut buf, rip, module_range());
    let _ = writeln!(buf, " RSP 0x{rsp:x}");
    if code == EXCEPTION_ACCESS_VIOLATION as u32 && rec.NumberParameters >= 2 {
        let kind = rec.ExceptionInformation[0];
        let _ = writeln!(
            buf,
            "[crash] access violation: {} of 0x{:x}",
            if kind == 0 { "read" } else { "write" },
            rec.ExceptionInformation[1]
        );
    } else if code == 0xC000_00FD {
        let _ = writeln!(buf, "[crash] stack overflow");
    }
    // Where the module's `.map` is (cache layout: next to the module): with
    // the `pw64game+0x<off>` form above, the faulting function is a lookup.
    if let Some(map) = MODULE_MAP.get() {
        let _ = writeln!(buf, "[crash] module map: {map}");
    }
    // A long map path fills the buffer: keep the last line terminated.
    if buf.n == buf.b.len() {
        buf.b[buf.n - 1] = b'\n';
    }
    let text = &buf.b[..buf.n];
    // SAFETY: plain Win32 calls with valid arguments; failures are ignored.
    unsafe {
        write_handle(sys::GetStdHandle(sys::STD_ERROR_HANDLE), text);
        if let Some(path) = CRASH_LOG.get() {
            let h = sys::CreateFileW(
                path.as_ptr(),
                sys::FILE_APPEND_DATA,
                sys::SHARE_ALL,
                core::ptr::null(),
                sys::OPEN_ALWAYS,
                sys::FILE_ATTRIBUTE_NORMAL,
                core::ptr::null_mut(),
            );
            if h != sys::INVALID_HANDLE {
                write_handle(h, b"-----\nBirdman64 native crash\n");
                write_handle(h, text);
                sys::CloseHandle(h);
            }
        }
    }
}

unsafe extern "system" fn handler(info: *mut EXCEPTION_POINTERS) -> i32 {
    // SAFETY: the OS passes a valid EXCEPTION_POINTERS for the duration of
    // the handler.
    let Some(info) = (unsafe { info.as_ref() }) else {
        return CONTINUE_SEARCH;
    };
    // SAFETY: the OS owns the exception record; valid while the handler runs.
    let Some(rec) = (unsafe { info.ExceptionRecord.as_ref() }) else {
        return CONTINUE_SEARCH;
    };
    let code = rec.ExceptionCode as u32;
    // SAFETY: the OS owns the context record; valid while the handler runs.
    let ctx = unsafe { info.ContextRecord.as_mut() };
    if code == ARM_CODE {
        // DR0/DR7 edits are applied when we continue.
        if let Some(ctx) = ctx {
            ctx.Dr0 = WATCH_ADDR.load(Ordering::Relaxed);
            // L0 | RW0 = write (01) | LEN0 = 4 bytes (11)
            ctx.Dr7 = 0xD0001;
        }
        return CONTINUE_EXEC;
    }
    if code == STATUS_SINGLE_STEP && WATCH_ADDR.load(Ordering::Relaxed) != 0 {
        let addr = WATCH_ADDR.load(Ordering::Relaxed);
        let hits = WATCH_HITS.fetch_add(1, Ordering::Relaxed);
        if hits < 32 {
            let (rip, rsp, dr6) = match ctx {
                Some(c) => (c.Rip, c.Rsp, c.Dr6),
                None => (0, 0, 0),
            };
            eprintln!("[watch] write to 0x{addr:x} at RIP 0x{rip:x} RSP 0x{rsp:x} DR6 0x{dr6:x}");
        } else if hits == 32 {
            // Enough: disarm so the rest of the run is unaffected.
            if let Some(c) = ctx {
                c.Dr7 = 0;
            }
            eprintln!("[watch] hit cap reached, disarmed");
        }
        return CONTINUE_EXEC;
    }
    let (rip, rsp) = ctx.map_or((0, 0), |c| (c.Rip, c.Rsp));
    if is_our_crash(code, rip, rsp, load_range(&EXE_RANGE), module_range()) {
        report(info);
    }
    CONTINUE_SEARCH
}

/// Unhandled-exception filter: whatever reaches it crashes the process.
unsafe extern "system" fn filter(info: *const EXCEPTION_POINTERS) -> i32 {
    // SAFETY: the OS passes valid pointers for the duration of the filter.
    if let Some(i) = unsafe { info.as_ref() }
        && !i.ExceptionRecord.is_null()
    {
        report(i);
    }
    let prev = PREV_FILTER.load(Ordering::Relaxed);
    if prev != 0 {
        // SAFETY: the value SetUnhandledExceptionFilter returned: a filter
        // function pointer, chained with the same arguments.
        let prev: LPTOP_LEVEL_EXCEPTION_FILTER = unsafe { core::mem::transmute(prev) };
        if let Some(f) = prev {
            // SAFETY: as above.
            return unsafe { f(info) };
        }
    }
    CONTINUE_SEARCH
}

/// `(start, end)` of this exe's loaded image (mirror of pw64-game memmap).
fn exe_image_range() -> (usize, usize) {
    // SAFETY: GetModuleHandleW(null) is the exe's mapped PE header; e_lfanew
    // (DOS header +0x3C) points at "PE\0\0", SizeOfImage is at optional
    // header +56 (PE sig 4 + file header 20 = 24).
    unsafe {
        let base = sys::GetModuleHandleW(core::ptr::null()) as *const u8;
        if base.is_null() {
            return (0, 0);
        }
        let nt = base.add((base.add(0x3C) as *const u32).read_unaligned() as usize);
        let size = (nt.add(24 + 56) as *const u32).read_unaligned() as usize;
        (base as usize, base as usize + size)
    }
}

/// Registers the handlers (once). `first=1` keeps the vectored handler
/// ahead of the default ones; both return CONTINUE_SEARCH, so the process
/// behaviour is unchanged.
pub fn install() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let exe = exe_image_range();
        EXE_RANGE.0.store(exe.0, Ordering::Relaxed);
        EXE_RANGE.1.store(exe.1, Ordering::Relaxed);
        // SAFETY: correct-signature Windows calls: `handler`/`filter` have
        // the system ABI these registrations expect.
        unsafe {
            let prev = SetUnhandledExceptionFilter(Some(filter));
            PREV_FILTER.store(prev.map_or(0, |f| f as usize), Ordering::Relaxed);
            AddVectoredExceptionHandler(1, Some(handler));
        }
    });
    if let Ok(v) = std::env::var("PW64_WATCH") {
        let v = v.trim().trim_start_matches("0x").trim_start_matches("0X");
        if let Ok(addr) = u64::from_str_radix(v, 16) {
            WATCH_ADDR.store(addr, Ordering::Relaxed);
            // SAFETY: our handler recognises the code and continues;
            // RaiseException takes no pointers here (null extra info).
            unsafe { RaiseException(ARM_CODE, 0, 0, std::ptr::null()) };
            eprintln!("[watch] armed on 0x{addr:x}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{is_our_crash, write_rip};

    /// T14: a RIP inside the module image formats as `pw64game+0x<off>`,
    /// where the offset is the address to look up in the module's `.map`
    /// (the module is linked at 0xB0000000, so map address = offset).
    #[test]
    fn rip_in_module_range_formats_as_offset() {
        let range = (0xB000_0000, 0xB010_0000);
        let fmt = |rip: u64| {
            let mut s = String::new();
            let _ = write_rip(&mut s, rip, range);
            s
        };
        assert_eq!(fmt(0xB000_0000), "pw64game+0x0");
        assert_eq!(fmt(0xB000_0001), "pw64game+0x1");
        assert_eq!(fmt(0xB000_1234), "pw64game+0x1234");
        assert_eq!(fmt(0xB00F_FFFF), "pw64game+0xfffff");
        // Outside the module (e.g. the exe at 0xC0000000): raw address.
        assert_eq!(fmt(0xC000_1234), "0xc0001234");
        assert_eq!(fmt(0), "0x0");
        // Empty range (module not loaded): always raw.
        let mut s = String::new();
        let _ = write_rip(&mut s, 0xB000_1234, (0, 0));
        assert_eq!(s, "0xb0001234");
    }

    /// P2: first-chance exceptions in a driver/overlay on a normal stack
    /// are not crashes; faults in our code or on a coroutine stack are.
    #[test]
    fn first_chance_filter() {
        let exe = (0xC000_0000, 0xC100_0000);
        let module = (0xB000_0000, 0xB010_0000);
        let host_rsp = 0x14_f000;
        let driver_rip = 0x7ffd_1234_5678;
        let av = 0xC000_0005;
        // Driver AV on a host thread: handled or not, the filter decides.
        assert!(!is_our_crash(av, driver_rip, host_rsp, exe, module));
        // Illegal instruction from CPU probing in a library: same.
        assert!(!is_our_crash(
            0xC000_001D,
            driver_rip,
            host_rsp,
            exe,
            module
        ));
        // Our code.
        assert!(is_our_crash(av, 0xC000_1000, host_rsp, exe, module));
        assert!(is_our_crash(av, 0xB000_1000, host_rsp, exe, module));
        // A system DLL (memcpy) on a coroutine stack: no handler can see it.
        assert!(is_our_crash(av, driver_rip, 0x8090_0000, exe, module));
        // Stack overflow on a coroutine stack.
        assert!(is_our_crash(
            0xC000_00FD,
            driver_rip,
            0x8081_2000,
            exe,
            module
        ));
        // Non-error codes and C++ EH never count.
        assert!(!is_our_crash(
            0x4001_0006,
            0xC000_1000,
            host_rsp,
            exe,
            module
        ));
        assert!(!is_our_crash(
            0xE06D_7363,
            0xC000_1000,
            host_rsp,
            exe,
            module
        ));
        assert!(!is_our_crash(
            0x8000_0003,
            0xC000_1000,
            host_rsp,
            exe,
            module
        ));
    }

    #[inline(never)]
    fn recurse(depth: u64) -> u64 {
        let pad = std::hint::black_box([depth as u8; 512]);
        if depth == u64::MAX {
            return 0;
        }
        recurse(depth + 1) + pad[(depth % 512) as usize] as u64
    }

    /// P3: a stack overflow on a coroutine stack is reported (the handler
    /// runs on the headroom below the guard page). Runs itself as a child
    /// process that dies; the parent checks stderr and crash.log.
    #[test]
    fn coroutine_stack_overflow_is_reported() {
        if std::env::var_os("PW64_CRASH_CHILD").is_some() {
            let log = std::env::var_os("PW64_CRASH_CHILD").unwrap();
            super::set_crash_log_path(log.into());
            super::install();
            // The release layout: the stack is committed out of the
            // reserved window (P7).
            assert!(crate::os::stack::reserve_region());
            let mut co = corosensei::Coroutine::<(), (), u64, _>::with_stack(
                crate::os::stack::LowStack::new(),
                |_, ()| recurse(0),
            );
            let _ = co.resume(());
            unreachable!("the recursion should have overflowed");
        }
        let log = std::env::temp_dir().join(format!("pw64_crash_test_{}.log", std::process::id()));
        let _ = std::fs::remove_file(&log);
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "os::crash::tests::coroutine_stack_overflow_is_reported",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("PW64_CRASH_CHILD", &log)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        let logged = std::fs::read_to_string(&log).unwrap_or_default();
        let _ = std::fs::remove_file(&log);
        assert!(!out.status.success(), "child survived: {stderr}");
        assert!(
            stderr.contains("[crash] exception 0xc00000fd"),
            "no report on stderr: {stderr}"
        );
        assert!(
            logged.contains("Birdman64 native crash") && logged.contains("stack overflow"),
            "crash.log: {logged:?}"
        );
    }
}
