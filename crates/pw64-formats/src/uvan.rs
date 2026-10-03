//! `UVAN` joint animations (rotation keys per model part). Mirrors
//! `uvJanimLoad` in `decomp/src/kernel/anim.c` (`UnkCommStruct`,
//! `UnkPartStruct` in `uv_filesystem.h`; `ParsedUVAN` in `uv_graphics.h`).
//!
//! Blocks (MIO0), big-endian:
//! - `COMM` (0x18): s32 first frame, s32 last frame, s32 `unk8`, s32 frame
//!   step (`unkC`, ≤0 → 1), s32 UVMD id, s16 `unk14`.
//! - `PART` (one per animated part): s32 key count, s32 part index, then
//!   keys of 0x14 bytes: f32 quaternion (x, y, z, w), s16 frame, u16 flags
//!   (bits 8–10 = format, must be 1 = quaternion; bit 11 = `unk12_4`,
//!   copied to the runtime key).
//!
//! `uvJanim_80200638` writes the rotation into the part matrix with
//! `uvMat4SetQuaternionRotation`, which in the engine's row-vector
//! convention rotates by the *conjugate*: as a glTF (column-vector)
//! rotation use `[-x, -y, -z, w]`.

use anyhow::{Context, Result, bail, ensure};
use pw64_rom::Form;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Key {
    pub quat: [f32; 4],
    /// Absolute frame (the engine subtracts [`Uvan::first_frame`]).
    pub frame: i16,
    pub flags: u16,
}

impl Key {
    /// `unk12_5`: key format (1 = quaternion, the only one supported).
    pub fn format(&self) -> u16 {
        (self.flags >> 8) & 7
    }
    /// `unk12_4`.
    pub fn flag(&self) -> bool {
        self.flags & 0x800 != 0
    }
}

#[derive(Debug, Clone)]
pub struct Track {
    /// Model part index (`ParsedUVAN_Unk0.unk4`).
    pub part: u16,
    pub keys: Vec<Key>,
}

#[derive(Debug, Clone)]
pub struct Uvan {
    pub first_frame: i32,
    pub last_frame: i32,
    pub unk8: i32,
    /// Frame step (`ParsedUVAN.unk8`).
    pub step: i32,
    pub model: i32,
    pub unk14: i16,
    pub tracks: Vec<Track>,
}

impl Uvan {
    /// Mirrors `uvJanimLoad`.
    pub fn parse(form: &Form) -> Result<Self> {
        ensure!(form.tag.0 == *b"UVAN", "not a UVAN file ({})", form.tag);
        let mut head = None;
        let mut tracks = Vec::new();
        for b in &form.blocks {
            let d = &b.data;
            let s32 = |o: usize| -> Result<i32> {
                Ok(i32::from_be_bytes(
                    d.get(o..o + 4).context("block truncated")?.try_into()?,
                ))
            };
            match &b.tag.0 {
                b"COMM" => {
                    ensure!(d.len() >= 0x16, "COMM size {}", d.len());
                    head = Some((
                        s32(0)?,
                        s32(4)?,
                        s32(8)?,
                        s32(12)?,
                        s32(16)?,
                        i16::from_be_bytes([d[20], d[21]]),
                    ));
                }
                b"PART" => {
                    let n = s32(0)?;
                    let part = s32(4)?;
                    ensure!(
                        n >= 0 && (0..=u16::MAX as i32).contains(&part),
                        "PART {n} {part}"
                    );
                    let n = n as usize;
                    ensure!(d.len() >= 8 + n * 0x14, "PART with {n} keys too short");
                    let keys = d[8..8 + n * 0x14]
                        .as_chunks::<0x14>()
                        .0
                        .iter()
                        .map(|k| {
                            let f = |o: usize| f32::from_be_bytes(k[o..o + 4].try_into().unwrap());
                            Key {
                                quat: [f(0), f(4), f(8), f(12)],
                                frame: i16::from_be_bytes([k[16], k[17]]),
                                flags: u16::from_be_bytes([k[18], k[19]]),
                            }
                        })
                        .collect();
                    tracks.push(Track {
                        part: part as u16,
                        keys,
                    });
                }
                b"PAD " => {}
                t => bail!("unexpected UVAN block {}", String::from_utf8_lossy(t)),
            }
        }
        let (first_frame, last_frame, unk8, step, model, unk14) =
            head.context("UVAN without COMM")?;
        Ok(Self {
            first_frame,
            last_frame,
            unk8,
            step,
            model,
            unk14,
            tracks,
        })
    }

    /// Frame count as `uvJanimLoad` computes it (`unk6`: last key + 1).
    pub fn frame_count(&self) -> i32 {
        self.tracks
            .iter()
            .flat_map(|t| &t.keys)
            .map(|k| k.frame as i32 - self.first_frame + 1)
            .max()
            .unwrap_or(0)
    }
}
