//! Sequence bank (`ALSeqFile`) and compact MIDI sequences (`ALCSeq`).
//!
//! The game plays music with `ALCSPlayer` (`alCSPNew` in `uvaManagerInit`), so
//! every sequence is in the *compact* format produced by the SDK's `midicomp`:
//!
//! - Header: `trackOffset[16]` u32 (from sequence start, 0 = unused), `division` u32.
//! - Per track: varlen delta, event, varlen delta, event, …
//! - Events are MIDI with running status, except: note-on carries a varlen
//!   *duration* after the velocity (no note-offs are stored); meta events have
//!   no length byte: `FF 51 tt tt tt` tempo, `FF 2F` end of track,
//!   `FF 2E xx xx` loop start, `FF 2D count cur off32` loop end
//!   (jump back `off32` bytes from the end of the event while `cur` != 0;
//!   `cur` 0xFF = forever). Meta events clear running status.
//! - Compression: byte `FE` starts a back-reference `FE hi lo len` = replay
//!   `len` bytes starting `(hi<<8|lo) + 4` bytes before the byte after `len`;
//!   `FE FE` is a literal `FE`. Back-referenced bytes are never re-expanded.
//!
//! Mirrors `alSeqFileNew` (bnkf.c) and `__alCSeqGetTrackEvent`/`__getTrackByte`
//! (`decomp/src/libultra/audio/cseq.c`).

use crate::be;
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;

pub const SEQFILE_VERSION: u16 = 0x5331; // 'S1'

#[derive(Debug, Clone, Copy, Serialize)]
pub struct SeqEntry {
    /// Offset from the start of the sequence file.
    pub offset: u32,
    pub len: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct SeqFile {
    pub entries: Vec<SeqEntry>,
}

impl SeqFile {
    /// Parses the header of a sequence file image (`bytes` starts at the file).
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let rev = be::u16(bytes, 0)?;
        ensure!(
            rev == SEQFILE_VERSION,
            "sequence file revision 0x{rev:04X} is not 'S1'"
        );
        let n = be::i16(bytes, 2)?;
        ensure!(n >= 0, "negative sequence count");
        let entries = (0..n as usize)
            .map(|i| {
                Ok(SeqEntry {
                    offset: be::u32(bytes, 4 + 8 * i)?,
                    len: be::u32(bytes, 8 + 8 * i)?,
                })
            })
            .collect::<Result<_>>()?;
        Ok(Self { entries })
    }

    /// The bytes of sequence `i`.
    pub fn data<'a>(&self, bytes: &'a [u8], i: usize) -> Result<&'a [u8]> {
        let e = self.entries.get(i).context("sequence index out of range")?;
        be::slice(bytes, e.offset as usize, e.len as usize)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Event {
    /// Channel message; `duration` (ticks) is set for note-ons only.
    Midi {
        status: u8,
        b1: u8,
        b2: u8,
        duration: Option<u32>,
    },
    /// Microseconds per quarter note.
    Tempo(u32),
    LoopStart,
    LoopEnd {
        count: u8,
        current: u8,
        /// Bytes to jump back from the end of the event.
        offset: u32,
    },
    EndOfTrack,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct TrackEvent {
    /// Absolute tick.
    pub tick: u64,
    pub event: Event,
}

/// A compact sequence (`ALCMidiHdr` + track data).
pub struct CompactSeq<'a> {
    data: &'a [u8],
    pub division: u32,
    /// Track start offsets (from the sequence start).
    pub tracks: [Option<u32>; 16],
}

impl<'a> CompactSeq<'a> {
    /// Mirrors `alCSeqNew`.
    pub fn parse(data: &'a [u8]) -> Result<Self> {
        let mut tracks = [None; 16];
        for (i, t) in tracks.iter_mut().enumerate() {
            let off = be::u32(data, 4 * i)?;
            if off != 0 {
                ensure!(
                    (off as usize) < data.len(),
                    "track {i} offset 0x{off:X} out of range"
                );
                *t = Some(off);
            }
        }
        let division = be::u32(data, 0x40)?;
        ensure!(division > 0, "zero division");
        Ok(Self {
            data,
            division,
            tracks,
        })
    }

    /// Decodes one track in a single pass (loops are reported, not taken).
    pub fn track_events(&self, track: usize) -> Result<Vec<TrackEvent>> {
        let start = self.tracks[track].context("track not present")?;
        let mut r = TrackReader {
            data: self.data,
            pos: start as usize,
            bu_pos: 0,
            bu_len: 0,
            last_status: 0,
        };
        let mut tick = u64::from(r.var_len()?);
        let mut out = Vec::new();
        loop {
            let event = r
                .event()
                .with_context(|| format!("track {track} tick {tick}"))?;
            out.push(TrackEvent { tick, event });
            if event == Event::EndOfTrack {
                return Ok(out);
            }
            ensure!(out.len() < 1_000_000, "track {track} does not terminate");
            tick += u64::from(r.var_len()?);
        }
    }

    /// Converts to a format-1 Standard MIDI File (one MTrk per used track).
    /// Durations become note-offs; loop points become `loopStart`/`loopEnd`
    /// marker meta events. The music is rendered once (loops not unrolled).
    pub fn to_midi(&self) -> Result<Vec<u8>> {
        ensure!(
            self.division < 0x8000,
            "division {} too large for SMF",
            self.division
        );
        let mut mtrks = Vec::new();
        for t in 0..16 {
            if self.tracks[t].is_none() {
                continue;
            }
            // (tick, order, bytes): order 0 = note-off, sorts before same-tick events.
            let mut evs: Vec<(u64, u8, Vec<u8>)> = Vec::new();
            let mut end = 0;
            for e in self.track_events(t)? {
                match e.event {
                    Event::Midi {
                        status,
                        b1,
                        b2,
                        duration,
                    } => {
                        let ty = status & 0xF0;
                        if ty == 0xC0 || ty == 0xD0 {
                            evs.push((e.tick, 1, vec![status, b1]));
                        } else {
                            evs.push((e.tick, 1, vec![status, b1, b2]));
                        }
                        if let Some(d) = duration {
                            let off = e.tick + u64::from(d);
                            evs.push((off, 0, vec![0x80 | (status & 0xF), b1, 0]));
                            end = end.max(off);
                        }
                    }
                    Event::Tempo(us) => {
                        let b = us.to_be_bytes();
                        evs.push((e.tick, 1, vec![0xFF, 0x51, 3, b[1], b[2], b[3]]));
                    }
                    Event::LoopStart => evs.push((e.tick, 1, meta_text(b"loopStart"))),
                    Event::LoopEnd { .. } => evs.push((e.tick, 1, meta_text(b"loopEnd"))),
                    Event::EndOfTrack => end = end.max(e.tick),
                }
            }
            evs.sort_by_key(|e| (e.0, e.1));
            let mut body = Vec::new();
            let mut last = 0;
            for (tick, _, bytes) in evs {
                write_var_len(&mut body, (tick - last) as u32);
                body.extend_from_slice(&bytes);
                last = tick;
            }
            write_var_len(&mut body, (end - last) as u32);
            body.extend_from_slice(&[0xFF, 0x2F, 0]);
            mtrks.push(body);
        }
        let mut smf = Vec::new();
        smf.extend_from_slice(b"MThd");
        smf.extend_from_slice(&6u32.to_be_bytes());
        smf.extend_from_slice(&1u16.to_be_bytes());
        smf.extend_from_slice(&(mtrks.len() as u16).to_be_bytes());
        smf.extend_from_slice(&(self.division as u16).to_be_bytes());
        for body in mtrks {
            smf.extend_from_slice(b"MTrk");
            smf.extend_from_slice(&(body.len() as u32).to_be_bytes());
            smf.extend_from_slice(&body);
        }
        Ok(smf)
    }
}

fn meta_text(s: &[u8]) -> Vec<u8> {
    let mut v = vec![0xFF, 0x06, s.len() as u8];
    v.extend_from_slice(s);
    v
}

fn write_var_len(out: &mut Vec<u8>, mut v: u32) {
    let mut buf = [0u8; 5];
    let mut i = 4;
    buf[i] = (v & 0x7F) as u8;
    v >>= 7;
    while v != 0 {
        i -= 1;
        buf[i] = 0x80 | (v & 0x7F) as u8;
        v >>= 7;
    }
    out.extend_from_slice(&buf[i..]);
}

struct TrackReader<'a> {
    data: &'a [u8],
    pos: usize,
    bu_pos: usize,
    bu_len: usize,
    last_status: u8,
}

impl TrackReader<'_> {
    fn raw(&mut self) -> Result<u8> {
        let b = be::u8(self.data, self.pos)?;
        self.pos += 1;
        Ok(b)
    }

    /// Mirrors `__getTrackByte`.
    fn byte(&mut self) -> Result<u8> {
        if self.bu_len > 0 {
            let b = be::u8(self.data, self.bu_pos)?;
            self.bu_pos += 1;
            self.bu_len -= 1;
            return Ok(b);
        }
        let b = self.raw()?;
        if b != 0xFE {
            return Ok(b);
        }
        let hi = self.raw()?;
        if hi == 0xFE {
            return Ok(0xFE);
        }
        let lo = self.raw()?;
        let len = self.raw()?;
        let back = ((usize::from(hi) << 8) | usize::from(lo)) + 4;
        self.bu_pos = self
            .pos
            .checked_sub(back)
            .context("back-reference before data start")?;
        ensure!(len > 0, "zero-length back-reference");
        self.bu_len = usize::from(len);
        self.byte()
    }

    /// Mirrors `__readVarLen`.
    fn var_len(&mut self) -> Result<u32> {
        let mut v = u32::from(self.byte()?);
        if v & 0x80 != 0 {
            v &= 0x7F;
            loop {
                let c = self.byte()?;
                v = (v << 7) + u32::from(c & 0x7F);
                if c & 0x80 == 0 {
                    break;
                }
            }
        }
        Ok(v)
    }

    /// Mirrors `__alCSeqGetTrackEvent` (without taking loops).
    fn event(&mut self) -> Result<Event> {
        let status = self.byte()?;
        if status == 0xFF {
            let ty = self.byte()?;
            self.last_status = 0;
            return Ok(match ty {
                0x51 => {
                    let b = [self.byte()?, self.byte()?, self.byte()?];
                    Event::Tempo(u32::from_be_bytes([0, b[0], b[1], b[2]]))
                }
                0x2F => Event::EndOfTrack,
                0x2E => {
                    self.byte()?;
                    self.byte()?;
                    Event::LoopStart
                }
                0x2D => {
                    // Read straight from curLoc, not through the back-reference logic.
                    let count = self.raw()?;
                    let current = self.raw()?;
                    let o = be::u32(self.data, self.pos)?;
                    self.pos += 4;
                    Event::LoopEnd {
                        count,
                        current,
                        offset: o,
                    }
                }
                t => bail!("unknown meta event 0x{t:02X}"),
            });
        }
        let (status, b1) = if status & 0x80 != 0 {
            self.last_status = status;
            (status, self.byte()?)
        } else {
            ensure!(
                self.last_status != 0,
                "running status without a status byte"
            );
            (self.last_status, status)
        };
        let ty = status & 0xF0;
        let (b2, duration) = if ty == 0xC0 || ty == 0xD0 {
            (0, None)
        } else {
            let b2 = self.byte()?;
            let d = if ty == 0x90 {
                Some(self.var_len()?)
            } else {
                None
            };
            (b2, d)
        };
        Ok(Event::Midi {
            status,
            b1,
            b2,
            duration,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seq(track: &[u8]) -> Vec<u8> {
        let mut d = vec![0u8; 0x44];
        d[3] = 0x44; // track 0 at 0x44
        d[0x43] = 96; // division
        d.extend_from_slice(track);
        d
    }

    #[test]
    fn events_running_status_and_backref() {
        // tempo; note-on (key 60, dur 48); after 128 ticks a running-status
        // note (key 62); then the same note again via a back-reference; EOT.
        let t = [
            0x00, 0xFF, 0x51, 0x07, 0xA1, 0x20, // tempo 500000
            0x00, 0x90, 60, 100, 48, // note
            0x81, 0x00, 62, 100, 48, // running status
            0x00, 0xFE, 0x00, 0x04,
            0x03, // replay "62 100 48": 4+4 = 8 bytes before the byte after `len`
            0x00, 0xFF, 0x2F,
        ];
        let d = seq(&t);
        let s = CompactSeq::parse(&d).unwrap();
        assert_eq!(s.division, 96);
        let ev = s.track_events(0).unwrap();
        let note = |key| Event::Midi {
            status: 0x90,
            b1: key,
            b2: 100,
            duration: Some(48),
        };
        assert_eq!(ev[0].event, Event::Tempo(500_000));
        assert_eq!(ev[1].event, note(60));
        assert_eq!((ev[2].tick, ev[2].event), (128, note(62)));
        assert_eq!((ev[3].tick, ev[3].event), (128, note(62)));
        assert_eq!(ev[4].event, Event::EndOfTrack);
        let midi = s.to_midi().unwrap();
        assert_eq!(&midi[..4], b"MThd");
    }

    #[test]
    fn var_len_roundtrip() {
        for v in [0, 0x7F, 0x80, 0x3FFF, 0x4000, 0x0FFF_FFFF] {
            let mut b = Vec::new();
            write_var_len(&mut b, v);
            let mut r = TrackReader {
                data: &b,
                pos: 0,
                bu_pos: 0,
                bu_len: 0,
                last_status: 0,
            };
            assert_eq!(r.var_len().unwrap(), v);
        }
    }
}
