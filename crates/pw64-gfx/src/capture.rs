//! Display-list captures: record everything one [`Interpreter::run`] reads
//! from a [`Memory`] and replay it later (benchmarks, regression checks on
//! real frames without the game).
//!
//! A capture stores the *results* of the memory calls (`read_u32` words,
//! `read_bytes`/`read_raw` bytes, `map` answers) as three sparse page images
//! plus the map table, so it replays identically whatever the host's
//! byte-order conventions. Captures hold ROM-derived data: keep them in
//! `tmp/`, never commit them (CONTRIBUTING.md).
//!
//! Game hookup (one line in `pw64/src/hle.rs`): replace
//! `self.interp.run(&self.mem, dl)` with
//! `pw64_gfx::capture::run(&mut self.interp, &self.mem, dl)`; then
//! `PW64_CAPTURE_DL=<n>[,<n>..]` writes the n-th gfx task (1-based) to
//! `tmp/dl_capture_<n>.bin`. Benchmark it with
//! `PW64_BENCH_CAPTURE=tmp/dl_capture_<n>.bin cargo run --release -p pw64-gfx --example bench_interp`.

use crate::frame::Frame;
use crate::interp::Interpreter;
use crate::memory::Memory;
use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

const MAGIC: &[u8; 8] = b"PW64DLC\x01";
const PAGE: usize = 4096;

/// Hasher for page numbers / addresses (one multiply; keys are small ints).
#[derive(Default)]
struct AddrHasher(u64);

impl Hasher for AddrHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0.rotate_left(8) ^ b as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
    }
    fn write_u32(&mut self, v: u32) {
        self.0 = (v as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
}

type AddrMap<V> = HashMap<u32, V, BuildHasherDefault<AddrHasher>>;

/// Sparse byte image in 4 KiB pages; unwritten bytes read as zero.
#[derive(Default, Clone)]
struct Pages(AddrMap<Box<[u8; PAGE]>>);

impl Pages {
    fn write(&mut self, addr: u32, data: &[u8]) {
        for (i, &b) in data.iter().enumerate() {
            let a = addr.wrapping_add(i as u32);
            let page = self
                .0
                .entry(a / PAGE as u32)
                .or_insert_with(|| Box::new([0; PAGE]));
            page[a as usize % PAGE] = b;
        }
    }

    fn read(&self, addr: u32, out: &mut [u8]) {
        let mut done = 0;
        while done < out.len() {
            let a = addr.wrapping_add(done as u32);
            let off = a as usize % PAGE;
            let n = (PAGE - off).min(out.len() - done);
            match self.0.get(&(a / PAGE as u32)) {
                Some(p) => out[done..done + n].copy_from_slice(&p[off..off + n]),
                None => out[done..done + n].fill(0),
            }
            done += n;
        }
    }

    fn save(&self, w: &mut impl Write) -> io::Result<()> {
        let mut keys: Vec<_> = self.0.keys().copied().collect();
        keys.sort_unstable();
        w.write_all(&(keys.len() as u32).to_le_bytes())?;
        for k in keys {
            w.write_all(&k.to_le_bytes())?;
            w.write_all(&self.0[&k][..])?;
        }
        Ok(())
    }

    fn load(r: &mut impl Read) -> io::Result<Self> {
        let n = read_u32(r)?;
        let mut p = Pages::default();
        for _ in 0..n {
            let k = read_u32(r)?;
            let mut page = Box::new([0u8; PAGE]);
            r.read_exact(&mut page[..])?;
            p.0.insert(k, page);
        }
        Ok(p)
    }
}

fn read_u32(r: &mut impl Read) -> io::Result<u32> {
    let mut b = [0; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

#[derive(Default, Clone)]
struct Log {
    maps: AddrMap<Option<u32>>,
    words: Pages,
    bytes: Pages,
    raw: Pages,
}

/// A [`Memory`] wrapper that records every read (see module docs).
pub struct Recorder<'a> {
    inner: &'a dyn Memory,
    log: RefCell<Log>,
}

impl<'a> Recorder<'a> {
    pub fn new(inner: &'a dyn Memory) -> Self {
        Self {
            inner,
            log: RefCell::default(),
        }
    }

    /// The recorded reads as a replayable capture of the list at `dl`.
    pub fn finish(self, dl: u32, wide_tags: bool) -> Capture {
        Capture {
            dl,
            wide_tags,
            mem: CaptureMemory(self.log.into_inner()),
        }
    }
}

impl Memory for Recorder<'_> {
    fn read_u32(&self, addr: u32) -> u32 {
        let v = self.inner.read_u32(addr);
        self.log.borrow_mut().words.write(addr, &v.to_be_bytes());
        v
    }
    fn read_bytes(&self, addr: u32, out: &mut [u8]) {
        self.inner.read_bytes(addr, out);
        self.log.borrow_mut().bytes.write(addr, out);
    }
    fn read_raw(&self, addr: u32, out: &mut [u8]) {
        self.inner.read_raw(addr, out);
        self.log.borrow_mut().raw.write(addr, out);
    }
    fn map(&self, raw: u32) -> Option<u32> {
        let m = self.inner.map(raw);
        self.log.borrow_mut().maps.insert(raw, m);
        m
    }
}

/// Replays a capture's reads.
#[derive(Clone)]
pub struct CaptureMemory(Log);

impl Memory for CaptureMemory {
    fn read_u32(&self, addr: u32) -> u32 {
        let mut b = [0; 4];
        self.0.words.read(addr, &mut b);
        u32::from_be_bytes(b)
    }
    fn read_bytes(&self, addr: u32, out: &mut [u8]) {
        self.0.bytes.read(addr, out);
    }
    fn read_raw(&self, addr: u32, out: &mut [u8]) {
        self.0.raw.read(addr, out);
    }
    fn map(&self, raw: u32) -> Option<u32> {
        self.0.maps.get(&raw).copied().flatten()
    }
}

/// One captured display list.
#[derive(Clone)]
pub struct Capture {
    pub dl: u32,
    /// [`Interpreter::wide_tags`] at capture time.
    pub wide_tags: bool,
    pub mem: CaptureMemory,
}

impl Capture {
    /// Runs the captured list on `interp` (sets its `wide_tags`).
    pub fn run(&self, interp: &mut Interpreter) -> Frame {
        interp.wide_tags = self.wide_tags;
        interp.run(&self.mem, self.dl)
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        let mut w = io::BufWriter::new(std::fs::File::create(path)?);
        w.write_all(MAGIC)?;
        w.write_all(&self.dl.to_le_bytes())?;
        w.write_all(&[self.wide_tags as u8])?;
        let log = &self.mem.0;
        let mut maps: Vec<_> = log.maps.iter().map(|(&k, &v)| (k, v)).collect();
        maps.sort_unstable();
        w.write_all(&(maps.len() as u32).to_le_bytes())?;
        for (raw, m) in maps {
            w.write_all(&raw.to_le_bytes())?;
            w.write_all(&[m.is_some() as u8])?;
            w.write_all(&m.unwrap_or(0).to_le_bytes())?;
        }
        log.words.save(&mut w)?;
        log.bytes.save(&mut w)?;
        log.raw.save(&mut w)?;
        w.flush()
    }
}

/// Loads a capture written by [`Capture::save`].
pub fn load(path: &Path) -> io::Result<Capture> {
    let mut r = io::BufReader::new(std::fs::File::open(path)?);
    let mut magic = [0; 8];
    r.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a PW64 DL capture",
        ));
    }
    let dl = read_u32(&mut r)?;
    let mut flag = [0; 1];
    r.read_exact(&mut flag)?;
    let mut log = Log::default();
    for _ in 0..read_u32(&mut r)? {
        let raw = read_u32(&mut r)?;
        let mut some = [0; 1];
        r.read_exact(&mut some)?;
        let m = read_u32(&mut r)?;
        log.maps.insert(raw, (some[0] != 0).then_some(m));
    }
    log.words = Pages::load(&mut r)?;
    log.bytes = Pages::load(&mut r)?;
    log.raw = Pages::load(&mut r)?;
    Ok(Capture {
        dl,
        wide_tags: flag[0] != 0,
        mem: CaptureMemory(log),
    })
}

/// Records one run of `dl` and returns the frame plus its capture.
pub fn record(interp: &mut Interpreter, mem: &dyn Memory, dl: u32) -> (Frame, Capture) {
    let rec = Recorder::new(mem);
    let frame = interp.run(&rec, dl);
    let cap = rec.finish(dl, interp.wide_tags);
    (frame, cap)
}

/// `PW64_CAPTURE_DL=<n>[,<n>..]`: 1-based task numbers to capture.
fn requested() -> &'static [usize] {
    static REQ: OnceLock<Vec<usize>> = OnceLock::new();
    REQ.get_or_init(|| {
        std::env::var("PW64_CAPTURE_DL")
            .map(|s| s.split(',').filter_map(|n| n.trim().parse().ok()).collect())
            .unwrap_or_default()
    })
}

/// Drop-in for `interp.run(mem, dl)` that writes `tmp/dl_capture_<n>.bin`
/// for the gfx tasks listed in `PW64_CAPTURE_DL` (see module docs). A
/// capture that does not replay to the same frame is reported (log warn).
pub fn run(interp: &mut Interpreter, mem: &dyn Memory, dl: u32) -> Frame {
    static TASK: AtomicUsize = AtomicUsize::new(0);
    let req = requested();
    if req.is_empty() {
        return interp.run(mem, dl);
    }
    let n = TASK.fetch_add(1, Ordering::Relaxed) + 1;
    if !req.contains(&n) {
        return interp.run(mem, dl);
    }
    let (frame, cap) = record(interp, mem, dl);
    let path = PathBuf::from(format!("tmp/dl_capture_{n}.bin"));
    let _ = std::fs::create_dir_all("tmp");
    match cap.save(&path) {
        Ok(()) => log::info!("DL capture: task {n} → {}", path.display()),
        Err(e) => log::warn!("DL capture {}: {e}", path.display()),
    }
    let replay = cap.run(&mut Interpreter::new());
    if replay.vertices != frame.vertices || replay.draws != frame.draws {
        log::warn!(
            "DL capture {}: replay differs from the live frame",
            path.display()
        );
    }
    frame
}
