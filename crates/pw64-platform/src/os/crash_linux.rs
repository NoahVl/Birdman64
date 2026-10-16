//! Crash tracer, Linux flavour of `crash.rs` (native-build.md §8): a
//! SIGSEGV/SIGBUS/SIGILL/SIGFPE handler that prints the faulting RIP/RSP
//! (aarch64: PC/SP) and
//! access address in the same `[crash]` format, then puts the previous
//! handler back and returns, so the fault re-raises into it (Rust's
//! stack-overflow reporter, or the default core dump). RIPs in the game
//! module print as `pw64game+0x<off>` and the module's `.map` path is
//! logged (T14); the exe's own RIPs print as `exe+0x<off>` (a PIE moves per
//! run): symbolize with `PW64_MAP` (pw64-game build.rs writes an lld `-Map`).
//! No `PW64_WATCH`.

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

/// `(start, end)` of this exe's loaded image (mirror of pw64-game memmap):
/// linker-defined symbols, so async-signal-safe.
#[cfg(not(target_os = "android"))]
pub fn exe_image_range() -> (usize, usize) {
    unsafe extern "C" {
        // Defined by the linker (lld and GNU ld): first byte of the first
        // PT_LOAD and the end of .bss.
        static __executable_start: u8;
        static _end: u8;
    }
    (
        (&raw const __executable_start) as usize,
        (&raw const _end) as usize,
    )
}

/// Android: the "exe" is the app's `.so` (loaded by the Java side), and
/// bionic's linker defines no `__executable_start` for it. The range is the
/// PT_LOAD span of the object holding this function, found once with
/// `dl_iterate_phdr` (not async-signal-safe: [`install`] primes the cache
/// before any handler can run).
#[cfg(target_os = "android")]
pub fn exe_image_range() -> (usize, usize) {
    static IMAGE: (AtomicUsize, AtomicUsize) = (AtomicUsize::new(0), AtomicUsize::new(0));
    let start = IMAGE.0.load(Ordering::Relaxed);
    if start != 0 {
        return (start, IMAGE.1.load(Ordering::Relaxed));
    }
    let r = image_containing(exe_image_range as *const () as usize);
    IMAGE.1.store(r.1, Ordering::Relaxed);
    IMAGE.0.store(r.0, Ordering::Relaxed);
    r
}

/// PT_LOAD span `(start, end)` of the loaded object containing `addr`, or
/// `(0, 0)` (an empty range, matches nothing).
#[cfg(target_os = "android")]
fn image_containing(addr: usize) -> (usize, usize) {
    unsafe extern "C" fn cb(
        info: *mut libc::dl_phdr_info,
        _size: usize,
        data: *mut libc::c_void,
    ) -> libc::c_int {
        // SAFETY: bionic passes a valid info whose phdr array has phnum
        // entries; `data` is the `(addr, start, end)` below.
        unsafe {
            let st = &mut *(data as *mut (usize, usize, usize));
            let info = &*info;
            let phdrs = core::slice::from_raw_parts(info.dlpi_phdr, info.dlpi_phnum as usize);
            let (mut lo, mut hi, mut hit) = (usize::MAX, 0, false);
            for p in phdrs.iter().filter(|p| p.p_type == libc::PT_LOAD) {
                let s = info.dlpi_addr as usize + p.p_vaddr as usize;
                let e = s + p.p_memsz as usize;
                hit |= (s..e).contains(&st.0);
                lo = lo.min(s);
                hi = hi.max(e);
            }
            if hit {
                (st.1, st.2) = (lo, hi);
                return 1; // stop iterating
            }
            0
        }
    }
    let mut st = (addr, 0usize, 0usize);
    // SAFETY: the callback only reads what bionic hands it and writes `st`.
    unsafe { libc::dl_iterate_phdr(Some(cb), (&raw mut st).cast()) };
    (st.1, st.2)
}

/// Return addresses of the current thread, innermost first, into `ips`
/// (glibc `backtrace(3)`). Returns the count.
#[cfg(not(target_os = "android"))]
fn return_addresses(ips: &mut [usize; 48]) -> usize {
    let mut raw = [core::ptr::null_mut::<libc::c_void>(); 48];
    // SAFETY: glibc backtrace(3) into a 48-slot buffer.
    let n = unsafe { libc::backtrace(raw.as_mut_ptr(), raw.len() as libc::c_int) }.max(0) as usize;
    for (o, i) in ips.iter_mut().zip(&raw[..n]) {
        *o = *i as usize;
    }
    n
}

/// Bionic has no `backtrace(3)`: walk with libunwind's `_Unwind_Backtrace`
/// (Rust's std links it on Android), the same frames glibc's would give.
#[cfg(target_os = "android")]
fn return_addresses(ips: &mut [usize; 48]) -> usize {
    unsafe extern "C" {
        fn _Unwind_Backtrace(
            trace: unsafe extern "C" fn(*mut libc::c_void, *mut libc::c_void) -> libc::c_int,
            arg: *mut libc::c_void,
        ) -> libc::c_int;
        fn _Unwind_GetIP(ctx: *mut libc::c_void) -> usize;
    }
    unsafe extern "C" fn step(ctx: *mut libc::c_void, arg: *mut libc::c_void) -> libc::c_int {
        // SAFETY: `arg` is the `(buffer, count)` below; `ctx` is the
        // unwinder's live frame context.
        unsafe {
            let st = &mut *(arg as *mut (&mut [usize; 48], usize));
            if st.1 == st.0.len() {
                return 5; // _URC_END_OF_STACK: buffer full
            }
            st.0[st.1] = _Unwind_GetIP(ctx);
            st.1 += 1;
        }
        0 // _URC_NO_REASON: keep going
    }
    let mut st = (ips, 0usize);
    // SAFETY: the callback writes only into `st`.
    unsafe { _Unwind_Backtrace(step, (&raw mut st).cast()) };
    st.1
}

/// Pure code-address formatting for the crash lines (mirror: crash.rs):
/// inside the game module image `pw64game+0x<off>`, inside the exe image
/// `exe+0x<off>` (offsets from the image base: a PIE moves per run), each
/// followed by the absolute address; otherwise the raw address. The handler
/// passes its fixed buffer; the test passes a String.
fn write_rip(
    out: &mut impl core::fmt::Write,
    rip: u64,
    exe: (usize, usize),
    module: (usize, usize),
) -> core::fmt::Result {
    let inside = |r: (usize, usize)| (r.0 as u64..r.1 as u64).contains(&rip);
    if inside(module) {
        write!(out, "pw64game+0x{:x} (0x{rip:x})", rip - module.0 as u64)
    } else if inside(exe) {
        write!(out, "exe+0x{:x} (0x{rip:x})", rip - exe.0 as u64)
    } else {
        write!(out, "0x{rip:x}")
    }
}

/// The image bases line of a report (pure; mirror: crash.rs).
fn write_images(
    out: &mut impl core::fmt::Write,
    exe: (usize, usize),
    module: (usize, usize),
) -> core::fmt::Result {
    write!(out, "images: exe 0x{:x}-0x{:x}", exe.0, exe.1)?;
    if module.1 > module.0 {
        write!(out, ", pw64game 0x{:x}-0x{:x}", module.0, module.1)?;
    }
    Ok(())
}

/// For the panic report (mirror: crash.rs): the image bases and the current
/// thread's return addresses in the [`write_rip`] form, skipping `skip`
/// frames (std's backtrace prints names only, no addresses).
pub fn panic_frames(skip: u32) -> String {
    let (exe, module) = (exe_image_range(), module_range());
    let mut ips = [0usize; 48];
    let n = return_addresses(&mut ips);
    let mut s = String::new();
    let _ = write_images(&mut s, exe, module);
    s.push_str("\nframes:");
    for ip in ips[..n].iter().skip(skip as usize + 1) {
        s.push_str("\n  ");
        let _ = write_rip(&mut s, *ip as u64, exe, module);
    }
    s
}

/// The handlers we replaced, restored on the first fault.
static PREVIOUS: Mutex<Vec<(libc::c_int, libc::sigaction)>> = Mutex::new(Vec::new());

/// Formats into a fixed buffer: no allocation inside the signal handler.
struct Buf {
    b: [u8; 512],
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

/// The interrupted PC and SP from the signal's `ucontext_t`, plus their
/// names for the report (x86_64 `gregs[REG_RIP/REG_RSP]`, aarch64
/// `mcontext.pc/.sp`).
///
/// # Safety
/// `uc` is the ucontext the kernel passed to an `SA_SIGINFO` handler.
#[cfg(target_arch = "x86_64")]
unsafe fn pc_sp(uc: &libc::ucontext_t) -> (u64, u64, &'static str, &'static str) {
    let g = &uc.uc_mcontext.gregs;
    (
        g[libc::REG_RIP as usize] as u64,
        g[libc::REG_RSP as usize] as u64,
        "RIP",
        "RSP",
    )
}

/// See the x86_64 twin.
///
/// # Safety
/// `uc` is the ucontext the kernel passed to an `SA_SIGINFO` handler.
#[cfg(target_arch = "aarch64")]
unsafe fn pc_sp(uc: &libc::ucontext_t) -> (u64, u64, &'static str, &'static str) {
    (uc.uc_mcontext.pc, uc.uc_mcontext.sp, "PC", "SP")
}

extern "C" fn handler(sig: libc::c_int, info: *mut libc::siginfo_t, ctx: *mut libc::c_void) {
    // SAFETY: the kernel passes a valid siginfo/ucontext for SA_SIGINFO.
    let (addr, (rip, rsp, pc_name, sp_name)) = unsafe {
        let uc = &*(ctx as *const libc::ucontext_t);
        ((*info).si_addr() as usize, pc_sp(uc))
    };
    let mut buf = Buf { b: [0; 512], n: 0 };
    let (exe, module) = (exe_image_range(), module_range());
    let _ = write!(buf, "[crash] signal {sig} at {pc_name} ");
    let _ = write_rip(&mut buf, rip, exe, module);
    let _ = write!(buf, " {sp_name} 0x{rsp:x}, fault address 0x{addr:x}\n[crash] ");
    let _ = write_images(&mut buf, exe, module);
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
    // Android: computes and caches the image range outside any handler.
    let _ = exe_image_range();
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
    /// the module image becomes `pw64game+0x<off>`, in the exe `exe+0x<off>`,
    /// the `.map` lookups, with the absolute RIP after them).
    #[test]
    fn rip_in_module_range_formats_as_offset() {
        let module = (0x7F00_0000_0000, 0x7F00_0010_0000);
        let exe = (0x5555_0000_0000, 0x5555_0300_0000);
        let fmt = |rip: u64| {
            let mut s = String::new();
            let _ = write_rip(&mut s, rip, exe, module);
            s
        };
        assert_eq!(fmt(0x7F00_0000_1234), "pw64game+0x1234 (0x7f0000001234)");
        assert_eq!(fmt(0x5555_0000_5678), "exe+0x5678 (0x555500005678)");
        assert_eq!(fmt(0xC000_1234), "0xc0001234");
        // Empty ranges (module not loaded): always raw.
        let mut s = String::new();
        let _ = write_rip(&mut s, 0xB000_1234, (0, 0), (0, 0));
        assert_eq!(s, "0xb0001234");
    }

    #[test]
    fn panic_frames_are_image_relative() {
        let s = super::panic_frames(0);
        assert!(s.starts_with("images: exe 0x"), "{s}");
        assert!(s.contains("\n  exe+0x"), "{s}");
    }
}
