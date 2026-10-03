//! Crash tracer, Linux flavour of `crash.rs` (native-build.md §8): a
//! SIGSEGV/SIGBUS/SIGILL/SIGFPE handler that prints the faulting RIP/RSP and
//! access address in the same `[crash]` format, then puts the previous
//! handler back and returns, so the fault re-raises into it (Rust's
//! stack-overflow reporter, or the default core dump). RIPs in the game
//! module print as `pw64game+0x<off>` and the module's `.map` path is
//! logged (T14); the exe's own RIPs: symbolize with `PW64_MAP` (pw64-game
//! build.rs writes an lld `-Map`). No `PW64_WATCH`.

use core::fmt::Write as _;
use std::os::unix::ffi::OsStrExt as _;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

const SIGNALS: [libc::c_int; 4] = [libc::SIGSEGV, libc::SIGBUS, libc::SIGILL, libc::SIGFPE];

/// On Windows this points the exception handler at the data dir's
/// crash.log. Here a signal handler may not safely use std file APIs
/// (async-signal-safety), so the path is accepted and ignored: the native
/// `[crash]` report stays on stderr only.
pub fn set_crash_log_path(_path: std::path::PathBuf) {}

/// The loaded game module's image range and its `.map` file, set once the
/// dylib loader has the module up (`pw64-game` dylib.rs; empty range until
/// then, which matches nothing). The handler reads the range with plain
/// atomics and the path without allocating (T14, first-run-build.md).
static MODULE_RANGE: (AtomicUsize, AtomicUsize) = (AtomicUsize::new(0), AtomicUsize::new(0));
static MODULE_MAP: OnceLock<PathBuf> = OnceLock::new();

/// Registers the loaded game module for crash symbolization: RIPs inside
/// `range` print as `pw64game+0x<off>` (the offset is the address to look
/// up in the `.map` the builder writes next to the module). Called once,
/// right after `pw64_game::dylib::load` succeeds.
pub fn set_game_module(range: (usize, usize), map: PathBuf) {
    MODULE_RANGE.0.store(range.0, Ordering::Relaxed);
    MODULE_RANGE.1.store(range.1, Ordering::Relaxed);
    let _ = MODULE_MAP.set(map);
}

/// The module range as set: (0, 0) when unset (an empty range never
/// contains a RIP).
fn module_range() -> (usize, usize) {
    (
        MODULE_RANGE.0.load(Ordering::Relaxed),
        MODULE_RANGE.1.load(Ordering::Relaxed),
    )
}

/// Pure RIP formatting for the crash lines (mirror: crash.rs): inside the
/// module image `pw64game+0x<off>`, otherwise the raw address. The handler
/// passes its fixed buffer; the test passes a String.
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

/// The handlers we replaced, restored on the first fault.
static PREVIOUS: Mutex<Vec<(libc::c_int, libc::sigaction)>> = Mutex::new(Vec::new());

/// Formats into a fixed buffer: no allocation inside the signal handler.
struct Buf {
    b: [u8; 320],
    n: usize,
}

impl Buf {
    /// Raw bytes (paths may not be UTF-8): truncates at the buffer end.
    fn push_bytes(&mut self, s: &[u8]) {
        let k = s.len().min(self.b.len() - self.n);
        self.b[self.n..self.n + k].copy_from_slice(&s[..k]);
        self.n += k;
    }
}

impl core::fmt::Write for Buf {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.push_bytes(s.as_bytes());
        Ok(())
    }
}

extern "C" fn handler(sig: libc::c_int, info: *mut libc::siginfo_t, ctx: *mut libc::c_void) {
    // SAFETY: the kernel passes a valid siginfo/ucontext for SA_SIGINFO.
    let (addr, rip, rsp) = unsafe {
        let uc = &*(ctx as *const libc::ucontext_t);
        (
            (*info).si_addr() as usize,
            uc.uc_mcontext.gregs[libc::REG_RIP as usize] as u64,
            uc.uc_mcontext.gregs[libc::REG_RSP as usize] as u64,
        )
    };
    let mut buf = Buf { b: [0; 320], n: 0 };
    let _ = write!(buf, "[crash] signal {sig} at RIP ");
    let _ = write_rip(&mut buf, rip, module_range());
    let _ = write!(buf, " RSP 0x{rsp:x}, fault address 0x{addr:x}");
    // Where the module's `.map` is (cache layout: next to the module): with
    // the `pw64game+0x<off>` form above, the faulting function is a lookup.
    // Raw path bytes: no allocation (async-signal-safety).
    if let Some(map) = MODULE_MAP.get() {
        let _ = write!(buf, "\nmodule map: ");
        buf.push_bytes(map.as_os_str().as_bytes());
    }
    let _ = writeln!(buf);
    // A long map path fills the buffer: keep the line terminated.
    if buf.n == buf.b.len() {
        buf.b[buf.n - 1] = b'\n';
    }
    // SAFETY: write(2) is async-signal-safe.
    unsafe { libc::write(2, buf.b.as_ptr().cast(), buf.n) };
    // Restore the previous handlers; returning re-executes the faulting
    // instruction, which now reaches them. try_lock: never block in here.
    if let Ok(prev) = PREVIOUS.try_lock() {
        for (s, act) in prev.iter() {
            // SAFETY: re-installing an action sigaction handed us.
            unsafe { libc::sigaction(*s, act, core::ptr::null_mut()) };
        }
    }
}

/// Registers the handler (idempotent per call site: `os::boot`).
pub fn install() {
    let mut prev = PREVIOUS.lock().unwrap();
    if !prev.is_empty() {
        return;
    }
    for sig in SIGNALS {
        // SAFETY: plain sigaction setup; SA_ONSTACK uses the alternate stack
        // Rust gives its threads, so a guard-page hit can still report.
        unsafe {
            let mut act: libc::sigaction = core::mem::zeroed();
            act.sa_sigaction = handler as *const () as usize;
            act.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK;
            libc::sigemptyset(&mut act.sa_mask);
            let mut old: libc::sigaction = core::mem::zeroed();
            if libc::sigaction(sig, &act, &mut old) == 0 {
                prev.push((sig, old));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::write_rip;

    /// T14 mirror of crash.rs `rip_in_module_range_formats_as_offset`: the
    /// pure formatting the handler writes into its fixed buffer (a RIP in
    /// the module image becomes `pw64game+0x<off>`, the `.map` lookup).
    #[test]
    fn rip_in_module_range_formats_as_offset() {
        let range = (0xB000_0000, 0xB010_0000);
        let fmt = |rip: u64| {
            let mut s = String::new();
            let _ = write_rip(&mut s, rip, range);
            s
        };
        assert_eq!(fmt(0xB000_0000), "pw64game+0x0");
        assert_eq!(fmt(0xB000_1234), "pw64game+0x1234");
        assert_eq!(fmt(0xB00F_FFFF), "pw64game+0xfffff");
        assert_eq!(fmt(0xC000_1234), "0xc0001234");
        // Empty range (module not loaded): always raw.
        let mut s = String::new();
        let _ = write_rip(&mut s, 0xB000_1234, (0, 0));
        assert_eq!(s, "0xb0001234");
    }
}
