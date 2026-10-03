//! Pilotwings 64 audio data in the classic libultra `libaudio` formats.
//!
//! The game keeps its audio in two places:
//! - the music bank (`.ctl` + `.tbl`) and the sequence bank sit in raw ROM
//!   segments *outside* the UV filesystem ([`rom`] has the offsets);
//! - the sound-effect bank is the single `UVSX` filesystem file, whose `.CTL`
//!   and `.TBL` blocks hold another bank file + wave table.
//!
//! Modules:
//! - [`bank`]: `ALBankFile` → banks → instruments → sounds → wavetables
//!   (mirrors `alBnkfNew`), ADPCM books and loops.
//! - [`vadpcm`]: VADPCM decoder (the RSP `A_ADPCM` algorithm).
//! - [`seq`]: `ALSeqFile` and compact MIDI (`ALCSeq`) sequences, plus a
//!   Standard MIDI File converter.
//! - [`wav`]: minimal 16-bit PCM WAV writer (with `smpl` loop chunk).

pub mod bank;
pub mod seq;
pub mod vadpcm;
pub mod wav;

pub use bank::{Bank, BankFile, Instrument, Sound, WaveKind, WaveTable};
pub use seq::{CompactSeq, SeqFile};

/// ROM locations of the audio segments (US release). From the splat config
/// (`decomp/config/us/pilotwings64.us.yaml`: `audio_seq`, `audio_ctl`,
/// `audio_tbl`); the `.tbl` segment runs to the end of the 8 MiB ROM.
pub mod rom {
    /// `ALSeqFile` ('S1'); offsets inside it are relative to this address.
    pub const SEQ_US: usize = 0x618B70;
    /// Music `ALBankFile` ('B1').
    pub const CTL_US: usize = 0x62D460;
    /// Music wave table; `ALWaveTable::base` is relative to this.
    pub const TBL_US: usize = 0x6314D0;
    /// End of ROM (= end of the `.tbl` segment).
    pub const END_US: usize = 0x800000;
}

/// Bounds-checked big-endian reads.
pub(crate) mod be {
    use anyhow::{Result, bail};

    pub fn slice(b: &[u8], off: usize, len: usize) -> Result<&[u8]> {
        match off.checked_add(len) {
            Some(end) if end <= b.len() => Ok(&b[off..end]),
            _ => bail!(
                "read of {len} bytes at 0x{off:X} past end (0x{:X})",
                b.len()
            ),
        }
    }
    pub fn u8(b: &[u8], off: usize) -> Result<u8> {
        Ok(slice(b, off, 1)?[0])
    }
    pub fn u16(b: &[u8], off: usize) -> Result<u16> {
        let s = slice(b, off, 2)?;
        Ok(u16::from_be_bytes([s[0], s[1]]))
    }
    pub fn i16(b: &[u8], off: usize) -> Result<i16> {
        Ok(u16(b, off)? as i16)
    }
    pub fn u32(b: &[u8], off: usize) -> Result<u32> {
        let s = slice(b, off, 4)?;
        Ok(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    }
    pub fn i32(b: &[u8], off: usize) -> Result<i32> {
        Ok(u32(b, off)? as i32)
    }
}
