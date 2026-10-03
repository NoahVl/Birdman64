//! PI / cartridge ROM (native-build.md §6 task 5): `osPiStartDma`,
//! `osPiReadIo`, `osPiRawReadIo` served from the verified ROM image, plus the
//! asm-only `mio0_decompress`.
//!
//! The C passes cartridge offsets (e.g. filesys 0xDF5B0) as `devAddr`, like
//! libultra's PI API (which adds `osRomBase` itself). DMAs complete
//! synchronously, then the `OSIoMesg` is posted to the caller's queue as the
//! PI manager would. `_uvMediaCopy`/`_uvDMA`/`uvMemRead` stay the C versions:
//! they funnel into these functions, and `uvMemRead` already assembles values
//! big-endian byte by byte (memory.c:185).

use crate::os::mesg::{OSMesg, OSMesgQueue};
use crate::os::{reschedule, with};
use anyhow::Result;
use pw64_rom::Rom;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static ROM: OnceLock<Rom> = OnceLock::new();

/// Env var naming the ROM file (any byte order).
pub const ROM_ENV: &str = "PW64_ROM";

/// ROM debug-flag area read by `bootproc` (system.c:232): "-d" starts the
/// game without the App thread, "-z" clears memory regions. Always zero here.
const DEBUG_FLAGS: std::ops::Range<u32> = 0xFF_B000..0xFF_B040;

/// Installs the ROM served to the game. Call once, before [`crate::os::boot`].
pub fn set_rom(rom: Rom) {
    if ROM.set(rom).is_err() {
        panic!("ROM already loaded");
    }
}

/// Finds the ROM: `explicit`, else `$PW64_ROM`, else the first
/// `.z64/.n64/.v64/.zip` in `rom/` (working directory, then workspace root),
/// else loose files in the working directory and next to the running exe
/// (zips included), else `decomp/baserom.us.z64` (relative to the working
/// directory, then to the workspace root).
pub fn find_rom(explicit: Option<&Path>) -> Result<PathBuf> {
    let candidates = rom_candidates(explicit);
    candidates.first().cloned().ok_or_else(|| {
        anyhow::anyhow!("no ROM found: pass a path, set {ROM_ENV}, or put it in rom/")
    })
}

/// ROM file extensions accepted in directory scans (`.zip` archives count:
/// `Rom::load` extracts a `.z64`/`.n64`/`.v64` member).
const ROM_EXTENSIONS: [&str; 4] = ["z64", "n64", "v64", "zip"];

/// N64 carts top out at 64 MiB: a bigger file can't be a ROM, so directory
/// scans skip it after a stat (never a full read).
const MAX_ROM_BYTES: u64 = 64 << 20;

/// Alphabetically sorted ROM candidates directly in `dir` (not recursive;
/// skips the dir silently when it doesn't exist). Raw files larger than an
/// N64 cart are stat-skipped, not read (F4).
fn dir_rom_candidates(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut roms: Vec<PathBuf> = rd
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file())
        .filter(|p| {
            let Some(ext) = p.extension().and_then(|e| e.to_str()) else {
                return false;
            };
            let ext = ext.to_ascii_lowercase();
            if !ROM_EXTENSIONS.contains(&ext.as_str()) {
                return false;
            }
            // Zips stay: `Rom::load` probes their member sizes without
            // reading the whole container.
            ext == "zip" || std::fs::metadata(p).is_ok_and(|m| m.len() <= MAX_ROM_BYTES)
        })
        .collect();
    roms.sort();
    roms
}

/// Workspace root next to `crates/pw64-platform` (so `cargo run` from any
/// working directory finds the repo's `rom/` and `decomp/`).
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Every ROM path to try, in order. `explicit`/`$PW64_ROM` stay single-path
/// and unvalidated (a bad one errors at load, exactly as before). Otherwise
/// the scan: `rom/` per root (as before, now zips included), then — after the
/// first root's `rom/` check — loose files in the working directory and next
/// to the running exe, then the `decomp/baserom.us.z64` fallback per root.
fn rom_candidates(explicit: Option<&Path>) -> Vec<PathBuf> {
    if let Some(p) = explicit {
        return vec![p.to_path_buf()];
    }
    if let Some(p) = std::env::var_os(ROM_ENV) {
        return vec![p.into()];
    }
    let mut out = Vec::new();
    for (i, root) in [PathBuf::from("."), workspace_root()]
        .into_iter()
        .enumerate()
    {
        out.extend(dir_rom_candidates(&root.join("rom")));
        if i == 0 {
            // Release-download layout: the exe and a ROM (or zip) beside it.
            out.extend(dir_rom_candidates(Path::new(".")));
            if let Some(dir) = std::env::current_exe()
                .ok()
                .and_then(|exe| exe.parent().map(Path::to_path_buf))
            {
                out.extend(dir_rom_candidates(&dir));
            }
            // Linux AppImage: a ROM may also sit next to the AppImage file
            // itself (the "exe dir" is the mount point's lib dir).
            #[cfg(target_os = "linux")]
            if let Some(dir) = std::env::var_os("APPIMAGE")
                .map(PathBuf::from)
                .and_then(|p| p.parent().map(Path::to_path_buf))
            {
                out.extend(dir_rom_candidates(&dir));
            }
        }
        let base = root.join("decomp/baserom.us.z64");
        if base.is_file() {
            out.push(base);
        }
    }
    out
}

/// Every candidate from [`rom_candidates`], verified in order: the first that
/// loads wins, later candidates only matter when earlier ones fail (so a
/// truncated file next to a good ROM doesn't block boot). Returns the path
/// used; when all fail, one error whose context chain names every path tried.
pub fn load_rom(explicit: Option<&Path>) -> Result<PathBuf> {
    let candidates = rom_candidates(explicit);
    let mut last: Option<anyhow::Error> = None;
    for path in &candidates {
        match Rom::load(path) {
            Ok(rom) => {
                set_rom(rom);
                return Ok(path.clone());
            }
            Err(e) => last = Some(e.context(format!("loading {}", path.display()))),
        }
    }
    match candidates.len() {
        0 => Err(anyhow::anyhow!(
            "no ROM found: pass a path, set {ROM_ENV}, or put it in rom/"
        )),
        // Single candidate (explicit arg / env): report its error directly.
        1 => Err(last.unwrap()),
        _ => {
            let mut e = last.unwrap();
            for path in candidates[..candidates.len() - 1].iter().rev() {
                e = e.context(format!("tried {}", path.display()));
            }
            Err(e)
        }
    }
}

/// The loaded ROM (z64 byte order), if any. Host HLE code reads microcode
/// data tables from it (e.g. the audio resampler LUT).
pub fn rom_bytes() -> Option<&'static [u8]> {
    ROM.get().map(Rom::bytes)
}

fn rom() -> &'static [u8] {
    ROM.get()
        .expect("ROM not loaded (pw64_platform::pi::load_rom)")
        .bytes()
}

/// Copies `dst.len()` ROM bytes from `dev_addr`; past the end reads as 0
/// (open bus is not emulated).
fn read_rom(dev_addr: u32, dst: &mut [u8]) {
    let rom = rom();
    let start = (dev_addr as usize).min(rom.len());
    let n = dst.len().min(rom.len() - start);
    dst[..n].copy_from_slice(&rom[start..start + n]);
    if n < dst.len() {
        eprintln!("[pi] read past ROM end: {dev_addr:#x}+{:#x}", dst.len());
        dst[n..].fill(0);
    }
}

/// Reads one PI word as raw bytes. Both callers treat the result as bytes
/// (memory.c:174 copies `buf[i]`, system.c:232 tests `sp3C[0..2]`), so the
/// word is stored in ROM (big-endian) byte order, not as a host u32 value.
/// # Safety
/// `data` must be writable.
unsafe fn read_word(dev_addr: u32, data: *mut u32) -> i32 {
    let mut w = [0u8; 4];
    if !DEBUG_FLAGS.contains(&dev_addr) {
        read_rom(dev_addr, &mut w);
    }
    // SAFETY: C passes a writable u32.
    unsafe { data.cast::<[u8; 4]>().write_unaligned(w) };
    0
}

/// Mirrors `osPiRawReadIo`.
///
/// # Safety
/// `data` must be a writable u32.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn osPiRawReadIo(dev_addr: u32, data: *mut u32) -> i32 {
    // SAFETY: C passes a writable u32.
    unsafe { read_word(dev_addr, data) }
}

/// Mirrors `osPiReadIo`.
///
/// # Safety
/// `data` must be a writable u32.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn osPiReadIo(dev_addr: u32, data: *mut u32) -> i32 {
    // SAFETY: C passes a writable u32.
    unsafe { read_word(dev_addr, data) }
}

/// libultra `OSIoMesg` (BUILD_VERSION D: no `piHandle`), native layout.
#[repr(C)]
pub struct OSIoMesg {
    pub ty: u16,
    pub pri: u8,
    pub status: u8,
    pub ret_queue: *mut OSMesgQueue,
    pub dram_addr: *mut c_void,
    pub dev_addr: u32,
    pub size: u32,
}

const OS_READ: i32 = 0;
/// `OS_MESG_TYPE_DMAREAD`.
const MESG_TYPE_DMAREAD: u16 = 1;

/// Mirrors `osPiStartDma` + the PI manager: copies ROM → RAM now, then posts
/// `mb` to `mq`.
///
/// # Safety
/// `vaddr` must be writable for `nbytes` bytes; `mb` and `mq` valid per
/// libultra's `osPiStartDma`.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn osPiStartDma(
    mb: *mut OSIoMesg,
    pri: i32,
    direction: i32,
    dev_addr: u32,
    vaddr: *mut c_void,
    nbytes: u32,
    mq: *mut OSMesgQueue,
) -> i32 {
    assert_eq!(
        direction, OS_READ,
        "osPiStartDma: only ROM reads are supported"
    );
    // N64 virtual addresses are 32-bit. C code that round-trips an address
    // through `s32` (e.g. `_uvDMA`'s `s32 dest`, system.c:419) sign-extends
    // 0x80xxxxxx; take the low 32 bits like the hardware would.
    let vaddr = (vaddr as usize as u32 as usize) as *mut c_void;
    assert!(
        !vaddr.is_null(),
        "osPiStartDma: null RAM address (ROM {dev_addr:#x}+{nbytes:#x})"
    );
    // SAFETY: the C passes a writable RAM buffer of `nbytes`.
    let dst = unsafe { std::slice::from_raw_parts_mut(vaddr.cast::<u8>(), nbytes as usize) };
    read_rom(dev_addr, dst);
    if !mb.is_null() {
        // SAFETY: C passes a writable OSIoMesg.
        unsafe {
            mb.write(OSIoMesg {
                ty: MESG_TYPE_DMAREAD,
                pri: pri as u8,
                status: 0,
                ret_queue: mq,
                dram_addr: vaddr,
                dev_addr,
                size: nbytes,
            })
        };
    }
    if !mq.is_null() {
        with(|k| k.post(mq as usize, mb as usize));
        reschedule();
    }
    0
}

/// No PI manager thread natively: DMAs are synchronous.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn osCreatePiManager(
    _pri: i32,
    _cmd_q: *mut OSMesgQueue,
    _buf: *mut OSMesg,
    _n: i32,
) {
}

/// `void mio0_decompress(void* src, u8* dst)` (asm in the ROM build,
/// filesystem.c:81), backed by `pw64_rom::mio0`. `src` holds a complete MIO0
/// stream in RAM; `dst` receives the header's decompressed size.
///
/// # Safety
/// `src` must point at a complete MIO0 stream; `dst` at room for the
/// stream's decompressed size.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn mio0_decompress(src: *const u8, dst: *mut u8) {
    // SAFETY: the header is 16 readable bytes.
    let hdr = unsafe { std::slice::from_raw_parts(src, 16) };
    let be = |o: usize| u32::from_be_bytes(hdr[o..o + 4].try_into().unwrap()) as usize;
    assert_eq!(&hdr[..4], b"MIO0", "mio0_decompress: bad header at {src:?}");
    let (out_len, comp, raw) = (be(4), be(8), be(12));
    // Upper bound of the stream: every output byte comes from at most one
    // raw byte or one 2-byte backref (which yields >= 3 bytes).
    let mut len = raw.max(comp) + out_len;
    // Don't let the bound run past the end of the RDRAM window.
    const WINDOW: std::ops::Range<usize> = 0x8000_0000..0x8080_0000;
    if WINDOW.contains(&(src as usize)) {
        len = len.min(WINDOW.end - src as usize);
    }
    // SAFETY: bytes are only read within the stream, which the bound covers.
    let stream = unsafe { std::slice::from_raw_parts(src, len) };
    let out = pw64_rom::mio0::decompress(stream).expect("mio0_decompress: corrupt stream");
    // SAFETY: C sized `dst` for the decompressed size.
    unsafe { std::ptr::copy_nonoverlapping(out.as_ptr(), dst, out.len()) };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Per-test temp directory, removed on drop.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!("pw64-pi-{tag}-{}", std::process::id()));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn dir_scan_is_sorted_and_filters() {
        let d = TempDir::new("scan");
        // Non-ROM extensions and extension-less names are skipped; the rest
        // come back alphabetically, `.zip` included.
        for name in ["b.n64", "a.ZIP", "c.txt", "romless", "d.z64"] {
            std::fs::write(d.0.join(name), b"x").unwrap();
        }
        std::fs::create_dir(d.0.join("sub.z64")).unwrap(); // dirs never count
        let got: Vec<String> = dir_rom_candidates(&d.0)
            .into_iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(got, ["a.ZIP", "b.n64", "d.z64"]);
        // Missing directory: empty, never an error.
        assert!(dir_rom_candidates(&d.0.join("nope")).is_empty());
    }
}
