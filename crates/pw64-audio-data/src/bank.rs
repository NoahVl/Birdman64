//! libultra sound bank files (`.ctl`) and their wave tables (`.tbl`).
//!
//! Mirrors `alBnkfNew` (`decomp/src/libultra/audio/bnkf.c`): every pointer in
//! a `.ctl` file is an offset from the start of the file, except
//! `ALWaveTable::base`, which is an offset into the `.tbl` data. We resolve the
//! pointer graph into owned structs instead of patching in place; each struct
//! keeps its `.ctl` offset so shared sounds/wavetables can be recognised.
//!
//! Struct layouts (`decomp/include/libultra/PR/libaudio.h`, big-endian):
//! - `ALBankFile`: revision `'B1'` s16, bankCount s16, bankArray\[\] u32.
//! - `ALBank`: instCount s16, flags u8, pad u8, sampleRate s32, percussion u32,
//!   instArray\[\] u32.
//! - `ALInstrument`: volume, pan, priority, flags, trem type/rate/depth/delay,
//!   vib type/rate/depth/delay (u8 each), bendRange s16, soundCount s16,
//!   soundArray\[\] u32.
//! - `ALSound` (16 B): envelope u32, keyMap u32, wavetable u32, samplePan u8,
//!   sampleVolume u8, flags u8.
//! - `ALEnvelope` (16 B): attack/decay/release time s32 (µs), attack/decay volume u8.
//! - `ALKeyMap` (6 B): velMin, velMax, keyMin, keyMax, keyBase u8, detune s8.
//! - `ALWaveTable` (20 B): base u32, len s32, type u8 (0 ADPCM, 1 RAW16),
//!   flags u8, pad, loop u32, book u32 (ADPCM only).
//! - `ALADPCMBook`: order s32, npredictors s32, book\[order·npredictors·8\] s16.
//! - `ALADPCMloop` (44 B): start, end, count u32, state\[16\] s16.
//! - `ALRawLoop` (12 B): start, end, count u32.
//!
//! Null pointers: `alBnkfNew` adds the file offset *before* its null check, so
//! a stored 0 becomes a pointer to the file header, which it then "patches" as
//! an instrument whose `flags` byte (offset 3 = low byte of bankCount, 1) is
//! already set, so nothing happens. Both banks here rely on that for unused
//! program slots (e.g. the SFX bank's instrument 0). We treat offset 0 as None.

use crate::be;
use anyhow::{Context, Result, ensure};
use serde::Serialize;

/// `AL_BANK_VERSION`.
pub const BANK_VERSION: u16 = 0x4231; // 'B1'

#[derive(Debug, Clone, Serialize)]
pub struct BankFile {
    pub banks: Vec<Option<Bank>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Bank {
    pub offset: u32,
    pub sample_rate: i32,
    pub percussion: Option<Instrument>,
    /// Indexed by MIDI program (music) or sound id (SFX).
    pub instruments: Vec<Option<Instrument>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Instrument {
    pub offset: u32,
    pub volume: u8,
    pub pan: u8,
    pub priority: u8,
    pub trem_type: u8,
    pub trem_rate: u8,
    pub trem_depth: u8,
    pub trem_delay: u8,
    pub vib_type: u8,
    pub vib_rate: u8,
    pub vib_depth: u8,
    pub vib_delay: u8,
    /// Pitch-bend range in cents.
    pub bend_range: i16,
    pub sounds: Vec<Sound>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Sound {
    pub offset: u32,
    pub envelope: Envelope,
    pub key_map: KeyMap,
    pub wavetable: WaveTable,
    pub sample_pan: u8,
    pub sample_volume: u8,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Envelope {
    /// Microseconds.
    pub attack_time: i32,
    pub decay_time: i32,
    pub release_time: i32,
    pub attack_volume: u8,
    pub decay_volume: u8,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct KeyMap {
    pub velocity_min: u8,
    pub velocity_max: u8,
    pub key_min: u8,
    pub key_max: u8,
    /// MIDI key at which the sample plays at its recorded rate.
    pub key_base: u8,
    /// Cents.
    pub detune: i8,
}

#[derive(Debug, Clone, Serialize)]
pub struct WaveTable {
    pub offset: u32,
    /// Offset of the sample data in the `.tbl`.
    pub base: u32,
    /// Length in bytes.
    pub len: u32,
    pub kind: WaveKind,
}

#[derive(Debug, Clone, Serialize)]
pub enum WaveKind {
    Adpcm {
        book: AdpcmBook,
        #[serde(rename = "loop")]
        loop_: Option<AdpcmLoop>,
    },
    Raw16 {
        #[serde(rename = "loop")]
        loop_: Option<RawLoop>,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct AdpcmBook {
    pub order: usize,
    pub npredictors: usize,
    /// `[predictor][order row][8]`, flattened.
    pub book: Vec<i16>,
}

impl AdpcmBook {
    /// The 8 coefficients of `row` (0..order) for `predictor`.
    pub fn row(&self, predictor: usize, row: usize) -> &[i16] {
        let i = (predictor * self.order + row) * 8;
        &self.book[i..i + 8]
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct AdpcmLoop {
    /// Sample indices.
    pub start: u32,
    pub end: u32,
    /// 0xFFFFFFFF = forever.
    pub count: u32,
    /// Decoder history at `start` (the RSP reloads it when looping).
    pub state: [i16; 16],
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct RawLoop {
    pub start: u32,
    pub end: u32,
    pub count: u32,
}

impl BankFile {
    /// Parses a `.ctl` image. Mirrors `alBnkfNew`.
    pub fn parse(ctl: &[u8]) -> Result<Self> {
        let rev = be::u16(ctl, 0)?;
        ensure!(
            rev == BANK_VERSION,
            "bank file revision 0x{rev:04X} is not 'B1'"
        );
        let count = be::i16(ctl, 2)?;
        ensure!(count >= 0, "negative bank count");
        let banks = (0..count as usize)
            .map(|i| {
                let off = be::u32(ctl, 4 + 4 * i)?;
                nonnull(off)
                    .map(|o| Bank::parse(ctl, o).with_context(|| format!("bank {i} @0x{o:X}")))
                    .transpose()
            })
            .collect::<Result<_>>()?;
        Ok(Self { banks })
    }

    /// Every distinct wavetable (by `.ctl` offset), in first-use order.
    pub fn wavetables(&self) -> Vec<&WaveTable> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for bank in self.banks.iter().flatten() {
            for inst in bank
                .percussion
                .iter()
                .chain(bank.instruments.iter().flatten())
            {
                for s in &inst.sounds {
                    if seen.insert(s.wavetable.offset) {
                        out.push(&s.wavetable);
                    }
                }
            }
        }
        out
    }
}

fn nonnull(off: u32) -> Option<usize> {
    (off != 0).then_some(off as usize)
}

impl Bank {
    /// Mirrors `_bnkfPatchBank`.
    fn parse(ctl: &[u8], off: usize) -> Result<Self> {
        let count = be::i16(ctl, off)?;
        ensure!(count >= 0, "negative instrument count");
        let sample_rate = be::i32(ctl, off + 4)?;
        let percussion = nonnull(be::u32(ctl, off + 8)?)
            .map(|o| Instrument::parse(ctl, o).context("percussion"))
            .transpose()?;
        let instruments = (0..count as usize)
            .map(|i| {
                nonnull(be::u32(ctl, off + 12 + 4 * i)?)
                    .map(|o| {
                        Instrument::parse(ctl, o)
                            .with_context(|| format!("instrument {i} @0x{o:X}"))
                    })
                    .transpose()
            })
            .collect::<Result<_>>()?;
        Ok(Self {
            offset: off as u32,
            sample_rate,
            percussion,
            instruments,
        })
    }
}

impl Instrument {
    /// Mirrors `_bnkfPatchInst`.
    fn parse(ctl: &[u8], off: usize) -> Result<Self> {
        let h = be::slice(ctl, off, 16)?;
        let count = be::i16(ctl, off + 14)?;
        ensure!(count >= 0, "negative sound count");
        let sounds = (0..count as usize)
            .map(|i| {
                // Sound pointers are patched unconditionally (no null check).
                let o = be::u32(ctl, off + 16 + 4 * i)? as usize;
                Sound::parse(ctl, o).with_context(|| format!("sound {i} @0x{o:X}"))
            })
            .collect::<Result<_>>()?;
        Ok(Self {
            offset: off as u32,
            volume: h[0],
            pan: h[1],
            priority: h[2],
            trem_type: h[4],
            trem_rate: h[5],
            trem_depth: h[6],
            trem_delay: h[7],
            vib_type: h[8],
            vib_rate: h[9],
            vib_depth: h[10],
            vib_delay: h[11],
            bend_range: be::i16(ctl, off + 12)?,
            sounds,
        })
    }
}

impl Sound {
    /// Mirrors `_bnkfPatchSound`.
    fn parse(ctl: &[u8], off: usize) -> Result<Self> {
        let env = be::u32(ctl, off)? as usize;
        let km = be::u32(ctl, off + 4)? as usize;
        let wt = be::u32(ctl, off + 8)? as usize;
        let k = be::slice(ctl, km, 6).context("keymap")?;
        Ok(Self {
            offset: off as u32,
            envelope: Envelope {
                attack_time: be::i32(ctl, env)?,
                decay_time: be::i32(ctl, env + 4)?,
                release_time: be::i32(ctl, env + 8)?,
                attack_volume: be::u8(ctl, env + 12)?,
                decay_volume: be::u8(ctl, env + 13)?,
            },
            key_map: KeyMap {
                velocity_min: k[0],
                velocity_max: k[1],
                key_min: k[2],
                key_max: k[3],
                key_base: k[4],
                detune: k[5] as i8,
            },
            wavetable: WaveTable::parse(ctl, wt).with_context(|| format!("wavetable @0x{wt:X}"))?,
            sample_pan: be::u8(ctl, off + 12)?,
            sample_volume: be::u8(ctl, off + 13)?,
        })
    }
}

impl WaveTable {
    /// Mirrors `_bnkfPatchWaveTable`.
    fn parse(ctl: &[u8], off: usize) -> Result<Self> {
        let base = be::u32(ctl, off)?;
        let len = be::u32(ctl, off + 4)?;
        let ty = be::u8(ctl, off + 8)?;
        let loop_off = nonnull(be::u32(ctl, off + 12)?);
        let kind = match ty {
            0 => {
                // The book pointer is patched unconditionally.
                let b = be::u32(ctl, off + 16)? as usize;
                let order = be::i32(ctl, b)?;
                let npred = be::i32(ctl, b + 4)?;
                ensure!(
                    (1..=8).contains(&order) && (1..=16).contains(&npred),
                    "bad ADPCM book: order {order}, {npred} predictors"
                );
                let n = (order * npred * 8) as usize;
                let book = (0..n)
                    .map(|i| be::i16(ctl, b + 8 + 2 * i))
                    .collect::<Result<_>>()?;
                let loop_ = loop_off
                    .map(|l| -> Result<_> {
                        let mut state = [0i16; 16];
                        for (i, s) in state.iter_mut().enumerate() {
                            *s = be::i16(ctl, l + 12 + 2 * i)?;
                        }
                        Ok(AdpcmLoop {
                            start: be::u32(ctl, l)?,
                            end: be::u32(ctl, l + 4)?,
                            count: be::u32(ctl, l + 8)?,
                            state,
                        })
                    })
                    .transpose()?;
                WaveKind::Adpcm {
                    book: AdpcmBook {
                        order: order as usize,
                        npredictors: npred as usize,
                        book,
                    },
                    loop_,
                }
            }
            1 => WaveKind::Raw16 {
                loop_: loop_off
                    .map(|l| -> Result<_> {
                        Ok(RawLoop {
                            start: be::u32(ctl, l)?,
                            end: be::u32(ctl, l + 4)?,
                            count: be::u32(ctl, l + 8)?,
                        })
                    })
                    .transpose()?,
            },
            t => anyhow::bail!("unknown wavetable type {t}"),
        };
        Ok(Self {
            offset: off as u32,
            base,
            len,
            kind,
        })
    }

    /// The sample bytes inside the `.tbl` image.
    pub fn data<'a>(&self, tbl: &'a [u8]) -> Result<&'a [u8]> {
        be::slice(tbl, self.base as usize, self.len as usize).context("wavetable data outside .tbl")
    }

    /// Decodes to 16-bit PCM (VADPCM or big-endian RAW16).
    pub fn decode(&self, tbl: &[u8]) -> Result<Vec<i16>> {
        let data = self.data(tbl)?;
        Ok(match &self.kind {
            WaveKind::Adpcm { book, .. } => crate::vadpcm::decode(book, data),
            WaveKind::Raw16 { .. } => data
                .as_chunks::<2>()
                .0
                .iter()
                .map(|&c| i16::from_be_bytes(c))
                .collect(),
        })
    }

    /// Loop (start, end, count) in samples, if any.
    pub fn loop_points(&self) -> Option<(u32, u32, u32)> {
        match &self.kind {
            WaveKind::Adpcm { loop_, .. } => loop_.map(|l| (l.start, l.end, l.count)),
            WaveKind::Raw16 { loop_ } => loop_.map(|l| (l.start, l.end, l.count)),
        }
    }
}
