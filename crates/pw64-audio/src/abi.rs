//! HLE of the classic RSP audio microcode (`aspMain`, `include/libultra/PR/abi.h`)
//! as driven by the libultra 2.0 synthesizer (`src/libultra/audio`).
//!
//! Sources for the command semantics: the `abi.h` packing macros and the
//! synthesizer C that emits them (how each buffer/count/flag is meant, e.g.
//! `load.c` expects `A_ADPCM` to write the previous 16 samples before the new
//! ones; `env.c` `_getRate` defines the envelope rate as a Q16.16 multiplier
//! per 8 samples; `drvrnew.c` `_init_lpfilter` defines the `A_POLEF`
//! coefficient table), plus the RSP's DMA rules (8-byte aligned addresses,
//! lengths rounded up to 8). The resampler filter table is the microcode's
//! own, read from the ROM ([`crate::lut`]). No emulator code was used.
//! Every handler was later checked against a disassembly of this ROM's
//! `aspMain` (field positions, flag bits, loop granularity, ENVMIXER ramp,
//! POLEF 16-bit gain field): see `docs/notes/audio.md`.
//!
//! Endianness: DMEM is a byte array; 16-bit samples are stored host-endian
//! (LE), and `A_LOADBUFF`/`A_SAVEBUFF` copy raw bytes. So every s16 buffer in
//! RDRAM that this HLE reads or writes (ADPCM books/loop states converted by
//! the native bank loader, the reverb delay line, filter states, the AI
//! output) is host-endian, while ADPCM streams stay in ROM byte order.
//! RAW16 waves (BE PCM in ROM) would need a swap; this game has none.
//!
//! Precision: saturating s16 stores and Q15 products rounded like the RSP's
//! `vmulf` (`(a*b + 0x4000) >> 15`). The exact vector-unit accumulation order
//! of the real microcode is not reproduced, so outputs may differ from
//! hardware by an LSB or two.

use pw64_audio_data::bank::AdpcmBook;
use pw64_audio_data::vadpcm;

/// DMEM bytes addressable by the command list (the microcode maps its
/// buffer region at a base inside the 4 KiB DMEM; libultra's largest
/// offset is `AL_AUX_R_OUT` 2048 + 320).
pub const DMEM_SIZE: usize = 0x1000;

pub const A_SPNOOP: u8 = 0;
pub const A_ADPCM: u8 = 1;
pub const A_CLEARBUFF: u8 = 2;
pub const A_ENVMIXER: u8 = 3;
pub const A_LOADBUFF: u8 = 4;
pub const A_RESAMPLE: u8 = 5;
pub const A_SAVEBUFF: u8 = 6;
pub const A_SEGMENT: u8 = 7;
pub const A_SETBUFF: u8 = 8;
pub const A_SETVOL: u8 = 9;
pub const A_DMEMMOVE: u8 = 10;
pub const A_LOADADPCM: u8 = 11;
pub const A_MIXER: u8 = 12;
pub const A_INTERLEAVE: u8 = 13;
pub const A_POLEF: u8 = 14;
pub const A_SETLOOP: u8 = 15;

/// Command names by opcode (for the histogram).
pub const NAMES: [&str; 16] = [
    "SPNOOP",
    "ADPCM",
    "CLEARBUFF",
    "ENVMIXER",
    "LOADBUFF",
    "RESAMPLE",
    "SAVEBUFF",
    "SEGMENT",
    "SETBUFF",
    "SETVOL",
    "DMEMMOVE",
    "LOADADPCM",
    "MIXER",
    "INTERLEAVE",
    "POLEF",
    "SETLOOP",
];

// Flag bits (abi.h).
pub const A_INIT: u8 = 0x01;
pub const A_LOOP: u8 = 0x02;
pub const A_LEFT: u8 = 0x02;
pub const A_VOL: u8 = 0x04;
pub const A_AUX: u8 = 0x08;

/// RDRAM as the command list addresses it (physical / `K0_TO_PHYS` values;
/// the implementor maps them to host memory).
pub trait Rdram {
    fn read(&mut self, addr: u32, out: &mut [u8]);
    fn write(&mut self, addr: u32, data: &[u8]);
}

/// A plain byte vector at physical address 0 (tests, offline tools).
impl Rdram for Vec<u8> {
    fn read(&mut self, addr: u32, out: &mut [u8]) {
        let a = addr as usize;
        out.copy_from_slice(&self[a..a + out.len()]);
    }
    fn write(&mut self, addr: u32, data: &[u8]) {
        let a = addr as usize;
        self[a..a + data.len()].copy_from_slice(data);
    }
}

fn sat(v: i32) -> i16 {
    v.clamp(i16::MIN as i32, i16::MAX as i32) as i16
}

/// Q15 product rounded like the RSP's `vmulf`.
fn mulq15(a: i16, b: i16) -> i32 {
    (a as i32 * b as i32 + 0x4000) >> 15
}

fn align(v: u32, a: u32) -> u32 {
    (v + a - 1) & !(a - 1)
}

/// The RSP audio task state: DMEM plus the microcode's registers.
pub struct AudioHle {
    dmem: Box<[u8; DMEM_SIZE]>,
    /// `A_LOADADPCM` table (ADPCM codebook, or `A_POLEF` coefficients).
    table: [i16; 128],
    lut: [i16; 256],
    // A_SETBUFF (main)
    in_: u16,
    out: u16,
    count: u16,
    // A_SETBUFF (A_AUX)
    dry_right: u16,
    wet_left: u16,
    wet_right: u16,
    // A_SETVOL
    vol: [i16; 2],
    target: [i16; 2],
    rate: [i32; 2],
    dry: i16,
    wet: i16,
    loop_addr: u32,
    /// Commands executed, by opcode (low 4 bits; unknown opcodes in 16+).
    pub histogram: [u64; 17],
    pub tasks: u64,
    /// The command being executed (diagnostics).
    pub last: [u32; 2],
    /// Optional level meter (`PW64_AUDIO_METER`; dev diagnostics).
    pub meter: Option<Box<Meter>>,
}

/// Level meter (dev diagnostics): per-voice-source and per-bus energies.
/// Voices are keyed by the last ADPCM codebook address (`A_LOADADPCM`), which
/// lies in the music or the SFX bank's `.ctl` copy, so the two players can be
/// told apart by address range.
#[derive(Default, Debug)]
pub struct Meter {
    last_book: u32,
    /// Book address → (dry energy L+R, wet energy L+R, ENVMIXER samples).
    pub voices: std::collections::BTreeMap<u32, [f64; 3]>,
    /// Main-bus mix of the aux return (`alMainBusPull`): energy of the dry
    /// main L+R before, of the aux return added, samples.
    pub bus: [f64; 3],
    /// Every book address seen (kept across [`Meter::reset`]).
    pub books: std::collections::BTreeSet<u32>,
}

impl Meter {
    /// Starts a new measurement window.
    pub fn reset(&mut self) {
        self.voices.clear();
        self.bus = [0.0; 3];
    }

    /// First address of the second bank's books: the split at the largest
    /// gap between all books seen (the music `.ctl` is loaded first).
    pub fn split(&self) -> u32 {
        let k: Vec<u32> = self.books.iter().copied().collect();
        k.windows(2)
            .max_by_key(|w| w[1] - w[0])
            .map_or(u32::MAX, |w| w[1])
    }
}

impl AudioHle {
    /// `lut`: the microcode's 64×4-tap resampler table ([`crate::lut`]).
    pub fn new(lut: [i16; 256]) -> Self {
        Self {
            dmem: Box::new([0; DMEM_SIZE]),
            table: [0; 128],
            lut,
            in_: 0,
            out: 0,
            count: 0,
            dry_right: 0,
            wet_left: 0,
            wet_right: 0,
            vol: [0; 2],
            target: [0; 2],
            rate: [0; 2],
            dry: 0,
            wet: 0,
            loop_addr: 0,
            histogram: [0; 17],
            tasks: 0,
            last: [0; 2],
            meter: None,
        }
    }

    /// Runs one task's command list (pairs of host u32 words `w0`, `w1`).
    pub fn run(&mut self, mem: &mut impl Rdram, list: &[[u32; 2]]) {
        self.tasks += 1;
        for &[w0, w1] in list {
            self.exec(mem, w0, w1);
        }
    }

    /// Executes one command.
    pub fn exec(&mut self, mem: &mut impl Rdram, w0: u32, w1: u32) {
        let op = (w0 >> 24) as u8;
        let flags = (w0 >> 16) as u8;
        self.histogram[(op as usize).min(16)] += 1;
        self.last = [w0, w1];
        match op {
            A_SPNOOP | A_SEGMENT => {} // segments: libultra only sets segment 0 = 0
            A_ADPCM => self.adpcm(mem, flags, w1),
            A_CLEARBUFF => {
                let n = align(w1 & 0xFFFF, 16);
                self.fill(w0 as u16, n, 0);
            }
            A_ENVMIXER => self.envmixer(mem, flags, w1),
            A_LOADBUFF => self.dma_load(mem, self.in_, w1, self.count as u32),
            A_RESAMPLE => self.resample(mem, flags, w0 as u16, w1),
            A_SAVEBUFF => self.dma_save(mem, self.out, w1, self.count as u32),
            A_SETBUFF => {
                if flags & A_AUX != 0 {
                    self.dry_right = w0 as u16;
                    self.wet_left = (w1 >> 16) as u16;
                    self.wet_right = w1 as u16;
                } else {
                    self.in_ = w0 as u16;
                    self.out = (w1 >> 16) as u16;
                    self.count = w1 as u16;
                }
            }
            A_SETVOL => {
                // aSetVolume(f, v, t, r): w0 = f<<16 | v, w1 = t<<16 | r.
                let v = w0 as i16;
                if flags & A_AUX != 0 {
                    self.dry = v;
                    self.wet = w1 as i16;
                } else {
                    let ch = if flags & A_LEFT != 0 { 0 } else { 1 };
                    if flags & A_VOL != 0 {
                        self.vol[ch] = v;
                    } else {
                        self.target[ch] = v;
                        self.rate[ch] = w1 as i32;
                    }
                }
            }
            A_DMEMMOVE => {
                let n = align(w1 & 0xFFFF, 16);
                self.copy_within(w0 as u16, (w1 >> 16) as u16, n);
            }
            A_LOADADPCM => {
                let n = ((w0 & 0xFF_FFFF) as usize).min(256) & !1;
                let mut b = vec![0u8; n];
                mem.read(w1, &mut b);
                // 32 bytes = the reverb's POLEF coefficients, not a voice book.
                if let Some(m) = &mut self.meter
                    && n != 32
                {
                    m.last_book = w1;
                    m.books.insert(w1);
                }
                for (i, c) in b.as_chunks::<2>().0.iter().enumerate() {
                    self.table[i] = i16::from_le_bytes(*c);
                }
            }
            A_MIXER => {
                let gain = w0 as i16;
                let (src, dst) = ((w1 >> 16) as u16, w1 as u16);
                // aspMain mixes 32 bytes per loop iteration.
                let n = align(self.count as u32, 32) / 2;
                // alMainBusPull: aux return (AL_AUX_*_OUT) into AL_MAIN_*_OUT.
                if self.meter.is_some()
                    && gain == 0x7FFF
                    && matches!((src, dst), (1728, 1088) | (2048, 1408))
                {
                    let e = |o: u16| -> f64 {
                        (0..n as u16)
                            .map(|i| (self.s16(o + 2 * i) as f64).powi(2))
                            .sum()
                    };
                    let (a, b) = (e(dst), e(src));
                    if let Some(m) = &mut self.meter {
                        m.bus[0] += a;
                        m.bus[1] += b;
                        m.bus[2] += n as f64;
                    }
                }
                for i in 0..n as u16 {
                    let s = self.s16(src + 2 * i);
                    let d = self.s16(dst + 2 * i);
                    self.set16(dst + 2 * i, sat(d as i32 + mulq15(s, gain)));
                }
            }
            A_INTERLEAVE => {
                let (l, r) = ((w1 >> 16) as u16, w1 as u16);
                let n = self.count / 2;
                // Read first: the output (usually DMEM 0) may overlap the inputs.
                let lr: Vec<(i16, i16)> = (0..n)
                    .map(|i| (self.s16(l + 2 * i), self.s16(r + 2 * i)))
                    .collect();
                for (i, (a, b)) in lr.into_iter().enumerate() {
                    let o = self.out + 4 * i as u16;
                    self.set16(o, a);
                    self.set16(o + 2, b);
                }
            }
            A_POLEF => self.polef(mem, flags, w0 as i16, w1),
            A_SETLOOP => self.loop_addr = w1,
            _ => {}
        }
    }

    fn idx(off: u16) -> usize {
        off as usize & (DMEM_SIZE - 1)
    }

    /// DMEM s16 at a byte offset (even; wraps like the 12-bit DMEM address).
    pub fn s16(&self, off: u16) -> i16 {
        let i = Self::idx(off) & !1;
        i16::from_le_bytes([self.dmem[i], self.dmem[i + 1]])
    }

    pub fn set16(&mut self, off: u16, v: i16) {
        let i = Self::idx(off) & !1;
        self.dmem[i..i + 2].copy_from_slice(&v.to_le_bytes());
    }

    fn u8(&self, off: u16) -> u8 {
        self.dmem[Self::idx(off)]
    }

    fn fill(&mut self, off: u16, n: u32, v: u8) {
        for i in 0..n as u16 {
            self.dmem[Self::idx(off.wrapping_add(i))] = v;
        }
    }

    fn copy_within(&mut self, src: u16, dst: u16, n: u32) {
        let tmp: Vec<u8> = (0..n as u16)
            .map(|i| self.u8(src.wrapping_add(i)))
            .collect();
        for (i, b) in tmp.into_iter().enumerate() {
            self.dmem[Self::idx(dst.wrapping_add(i as u16))] = b;
        }
    }

    /// Raw DMEM bytes (tests).
    pub fn dmem(&self) -> &[u8; DMEM_SIZE] {
        &self.dmem
    }

    pub fn dmem_mut(&mut self) -> &mut [u8; DMEM_SIZE] {
        &mut self.dmem
    }

    /// SP DMA RDRAM → DMEM: both addresses 8-byte aligned (low bits
    /// ignored), length rounded up to 8.
    fn dma_load(&mut self, mem: &mut impl Rdram, dmem: u16, addr: u32, len: u32) {
        if len == 0 {
            return;
        }
        let mut b = vec![0u8; align(len, 8) as usize];
        mem.read(addr & !7, &mut b);
        let d = dmem & !7;
        for (i, v) in b.into_iter().enumerate() {
            self.dmem[Self::idx(d.wrapping_add(i as u16))] = v;
        }
    }

    /// SP DMA DMEM → RDRAM (same alignment rules).
    fn dma_save(&mut self, mem: &mut impl Rdram, dmem: u16, addr: u32, len: u32) {
        if len == 0 {
            return;
        }
        let d = dmem & !7;
        let b: Vec<u8> = (0..align(len, 8) as u16)
            .map(|i| self.u8(d.wrapping_add(i)))
            .collect();
        mem.write(addr & !7, &b);
    }

    fn load_s16s<const N: usize>(mem: &mut impl Rdram, addr: u32) -> [i16; N] {
        let mut b = vec![0u8; N * 2];
        mem.read(addr, &mut b);
        std::array::from_fn(|i| i16::from_le_bytes([b[2 * i], b[2 * i + 1]]))
    }

    fn store_s16s(mem: &mut impl Rdram, addr: u32, v: &[i16]) {
        let b: Vec<u8> = v.iter().flat_map(|s| s.to_le_bytes()).collect();
        mem.write(addr, &b);
    }

    /// `A_ADPCM`: decodes `ceil(count / 32)` 9-byte frames from `in` to
    /// `out + 32`, after first writing the previous frame's 16 samples at
    /// `out` (`alAdpcmPull` skips them by `lastsam`). History comes from
    /// zeros (`A_INIT`), the loop state (`A_LOOP`, `A_SETLOOP` address) or
    /// the state buffer at `w1`, which receives the last frame afterwards.
    fn adpcm(&mut self, mem: &mut impl Rdram, flags: u8, state: u32) {
        let mut last: [i16; 16] = if flags & A_INIT != 0 {
            [0; 16]
        } else if flags & A_LOOP != 0 {
            Self::load_s16s(mem, self.loop_addr)
        } else {
            Self::load_s16s(mem, state)
        };
        let mut o = self.out;
        for s in last {
            self.set16(o, s);
            o = o.wrapping_add(2);
        }
        let book = AdpcmBook {
            order: 2,
            npredictors: 8,
            book: self.table.to_vec(),
        };
        let mut i = self.in_;
        for _ in 0..self.count.div_ceil(32) {
            let frame: [u8; 9] = std::array::from_fn(|k| self.u8(i.wrapping_add(k as u16)));
            i = i.wrapping_add(9);
            let mut hist: vadpcm::State = last[8..].try_into().unwrap();
            last = vadpcm::decode_frame(&book, &frame, &mut hist);
            for s in last {
                self.set16(o, s);
                o = o.wrapping_add(2);
            }
        }
        Self::store_s16s(mem, state, &last);
    }

    /// `A_RESAMPLE`: 4-tap polyphase resampler. `pitch` is Q1.15 (0x8000 =
    /// unity, `UNITY_PITCH`); the position accumulator is Q16.16 and the
    /// top 6 fraction bits pick the filter phase. Input is read from 4
    /// samples before `in` (they hold the previous call's tail, from the
    /// state buffer, or zeros on `A_INIT`); output is `count` bytes rounded
    /// up to 8 samples. State: those 4 tail samples + the fraction.
    fn resample(&mut self, mem: &mut impl Rdram, flags: u8, pitch: u16, state: u32) {
        let base = self.in_.wrapping_sub(8);
        let (tail, mut frac) = if flags & A_INIT != 0 {
            ([0i16; 4], 0u32)
        } else {
            let s: [i16; 5] = Self::load_s16s(mem, state);
            ([s[0], s[1], s[2], s[3]], s[4] as u16 as u32)
        };
        for (k, s) in tail.into_iter().enumerate() {
            self.set16(base.wrapping_add(2 * k as u16), s);
        }
        let step = (pitch as u32) << 1;
        let mut pos = base; // byte offset of tap 0
        let mut o = self.out;
        for _ in 0..align(self.count as u32, 16) / 2 {
            let phase = ((frac >> 10) & 0x3F) as usize * 4;
            let mut acc = 0i32;
            for k in 0..4 {
                acc += self.s16(pos.wrapping_add(2 * k as u16)) as i32 * self.lut[phase + k] as i32;
            }
            self.set16(o, sat((acc + 0x4000) >> 15));
            o = o.wrapping_add(2);
            frac += step;
            pos = pos.wrapping_add(2 * (frac >> 16) as u16);
            frac &= 0xFFFF;
        }
        let mut s = [0i16; 5];
        for (k, v) in s.iter_mut().take(4).enumerate() {
            *v = self.s16(pos.wrapping_add(2 * k as u16));
        }
        s[4] = frac as u16 as i16;
        Self::store_s16s(mem, state, &s);
    }

    /// `A_ENVMIXER`: per-voice volume envelope + pan + dry/wet send.
    /// Volumes (Q15, from `A_SETVOL`) move toward their targets by the
    /// Q16.16 rate once per 8 samples (`env.c` `_getRate` computes
    /// `(tgt/vol)^(8/count)`), linearly interpolated inside the 8 and
    /// clamped at the target. Each sample is added (saturating) to dry L/R
    /// (`out`, aux `dry_right`) scaled by vol·dry, and with `A_AUX` to wet
    /// L/R scaled by vol·wet. State (private layout, 36 of 80 bytes):
    /// value[2], target[2], rate[2], seq[2] as i32, then dry, wet.
    fn envmixer(&mut self, mem: &mut impl Rdram, flags: u8, state: u32) {
        let (mut value, target, rate, mut seq, dry, wet);
        if flags & A_INIT != 0 {
            value = self.vol.map(|v| (v as i32) << 16);
            target = self.target.map(|v| (v as i32) << 16);
            rate = self.rate;
            seq = value;
            dry = self.dry;
            wet = self.wet;
        } else {
            let s: [i16; 18] = Self::load_s16s(mem, state);
            let w = |i: usize| (s[2 * i] as u16 as i32) | ((s[2 * i + 1] as i32) << 16);
            value = [w(0), w(1)];
            target = [w(2), w(3)];
            rate = [w(4), w(5)];
            seq = [w(6), w(7)];
            dry = s[16];
            wet = s[17];
        }
        let aux = flags & A_AUX != 0;
        let outs = [self.out, self.dry_right, self.wet_left, self.wet_right];
        let n = align(self.count as u32, 16) / 2;
        let mut step = [0i32; 2];
        let mut energy = [0f64; 2]; // meter: dry, wet contributions
        for i in 0..n as u16 {
            if i % 8 == 0 {
                for c in 0..2 {
                    step[c] = if value[c] == target[c] {
                        0
                    } else {
                        let next = (seq[c] as i64 * rate[c] as i64) >> 16;
                        seq[c] = next.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
                        ((seq[c] as i64 - value[c] as i64) / 8) as i32
                    };
                }
            }
            let mut vol = [0i16; 2];
            for c in 0..2 {
                if step[c] != 0 {
                    let v = value[c] as i64 + step[c] as i64;
                    let reached = if step[c] > 0 {
                        v >= target[c] as i64
                    } else {
                        v <= target[c] as i64
                    };
                    value[c] = if reached { target[c] } else { v as i32 };
                    if reached {
                        step[c] = 0;
                    }
                }
                vol[c] = (value[c] >> 16) as i16;
            }
            let x = self.s16(self.in_.wrapping_add(2 * i));
            let gains = [
                sat(mulq15(vol[0], dry)),
                sat(mulq15(vol[1], dry)),
                sat(mulq15(vol[0], wet)),
                sat(mulq15(vol[1], wet)),
            ];
            for k in 0..if aux { 4 } else { 2 } {
                let o = outs[k].wrapping_add(2 * i);
                let d = self.s16(o);
                let c = mulq15(x, gains[k]);
                energy[k / 2] += (c as f64).powi(2);
                self.set16(o, sat(d as i32 + c));
            }
        }
        if let Some(m) = &mut self.meter {
            let v = m.voices.entry(m.last_book).or_default();
            v[0] += energy[0];
            v[1] += energy[1];
            v[2] += n as f64;
        }
        let mut s = [0i16; 18];
        for (i, v) in value
            .iter()
            .chain(&target)
            .chain(&rate)
            .chain(&seq)
            .enumerate()
        {
            s[2 * i] = *v as u16 as i16;
            s[2 * i + 1] = (*v >> 16) as i16;
        }
        s[16] = dry;
        s[17] = wet;
        Self::store_s16s(mem, state, &s);
    }

    /// `A_POLEF`: order-2 IIR in the ADPCM matrix form, Q14. The table
    /// (`A_LOADADPCM`, 16 × s16) holds h1[8] (weights of y[-2]) and h2[8]
    /// (weights of y[-1] = the impulse response: `_init_lpfilter` stores
    /// fc^(i+1)). Per group of 8, with h2 pre-scaled by the gain:
    /// `y[i] = (g·x[i] + h1[i]·y[-2] + h2[i]·y[-1] + Σ_{j<i} (h2[i-1-j]·g>>14)·x[j]) >> 14`.
    /// Runs in place on `in` → `out`; state = the last two outputs.
    fn polef(&mut self, mem: &mut impl Rdram, flags: u8, gain: i16, state: u32) {
        let (mut y2, mut y1) = if flags & A_INIT != 0 {
            (0i16, 0i16)
        } else {
            let s: [i16; 4] = Self::load_s16s(mem, state);
            (s[2], s[3])
        };
        let h1: [i16; 8] = self.table[..8].try_into().unwrap();
        let h2: [i16; 8] = self.table[8..16].try_into().unwrap();
        let h2g = h2.map(|h| ((h as i32 * gain as i32) >> 14) as i16);
        let n = align(self.count as u32, 16) / 16;
        let (mut i, mut o) = (self.in_, self.out);
        for _ in 0..n {
            let x: [i16; 8] = std::array::from_fn(|k| self.s16(i.wrapping_add(2 * k as u16)));
            let mut y = [0i16; 8];
            for k in 0..8 {
                let mut acc =
                    x[k] as i32 * gain as i32 + h1[k] as i32 * y2 as i32 + h2[k] as i32 * y1 as i32;
                for j in 0..k {
                    acc += h2g[k - 1 - j] as i32 * x[j] as i32;
                }
                y[k] = sat(acc >> 14);
            }
            for (k, v) in y.iter().enumerate() {
                self.set16(o.wrapping_add(2 * k as u16), *v);
            }
            (y2, y1) = (y[6], y[7]);
            i = i.wrapping_add(16);
            o = o.wrapping_add(16);
        }
        Self::store_s16s(mem, state, &[0, 0, y2, y1]);
    }
}
