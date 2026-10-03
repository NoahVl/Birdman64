//! `UVEN` environments: clear color, fog and the sky/sea models drawn before
//! the terrain. Mirrors `_uvParseUVEN` in `decomp/src/kernel/texture.c`
//! (`ParsedUVEN`, `uvEnvModel` in `uv_graphics.h`); drawn by `_uvEnvDraw`
//! (`decomp/src/kernel/env.c`).
//!
//! The single UVEN file holds one `COMM` block per environment (env id =
//! COMM index, picked with `uvFile_80224170`). Each block, big-endian:
//!
//! | type | meaning |
//! |---|---|
//! | u8 | model count *n* |
//! | (u16, u8) × *n* | model table: UVMD id, [`EnvModel`] flags |
//! | 0x3C bytes | `ParsedUVEN`, copied raw (`uvConsumeBytes` default case) |
//!
//! `ParsedUVEN` layout: +0 screen (clear) RGBA, +4 fog RGBA, +8 an unused
//! RGBA, +0xC 8 bytes, +0x14 f32 fog min, +0x18 f32 fog max, +0x1C u8 fog
//! enabled, +0x1D 17 bytes, +0x2E u8 clear enabled, +0x2F pad, then the
//! runtime-only model table pointer, model count and callback (+0x30..0x3C;
//! the file stores the count at +0x34 too).

use crate::reader::Reader;
use anyhow::{Context, Result, ensure};
use pw64_rom::Form;

/// `uvEnvModel`: one sky/sea model and how `_uvEnvDraw` draws it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnvModel {
    /// Global UVMD id.
    pub model: u16,
    pub flags: u8,
}

impl EnvModel {
    /// Bit 0: keep the states' `GFX_STATE_ZBUFFER` (otherwise cleared, so
    /// the model is drawn without depth test/update, as a backdrop).
    pub const KEEP_ZBUFFER: u8 = 1;
    /// Bit 1: draw with the far (27000) projection of `uvChan_80204C94`.
    pub const FAR_PROJECTION: u8 = 2;
    /// Bit 2: fogged (the env's fog factor); otherwise fog off.
    pub const FOG: u8 = 4;
    /// Bit 3: follow the camera in X/Y (translation = camera x, y, 0).
    pub const FOLLOW_CAMERA: u8 = 8;
}

/// `ParsedUVEN`.
#[derive(Debug, Clone)]
pub struct Uven {
    pub models: Vec<EnvModel>,
    /// `screenR..A`: clear color (`uvGfxClearScreen`), used if [`Self::clear`].
    pub screen: [u8; 4],
    /// `fogR..A` (`gDPSetFogColor`, alpha forced to 255 by `_uvEnvDraw`).
    pub fog_color: [u8; 4],
    /// `unusedR..A` (only reachable via `uvEnvProps`).
    pub unused_color: [u8; 4],
    pub fog_min: f32,
    pub fog_max: f32,
    pub fog_enabled: bool,
    pub clear: bool,
    /// Bytes +0xC..0x14 and +0x1D..0x2E (unknown; kept for inspection).
    pub unk: [u8; 25],
}

impl Uven {
    /// Mirrors `_uvParseUVEN`.
    pub fn parse_comm(b: &[u8]) -> Result<Self> {
        let mut r = Reader::new(b, "UVEN COMM");
        let n = r.u8()?;
        let models = (0..n)
            .map(|_| {
                Ok(EnvModel {
                    model: r.u16()?,
                    flags: r.u8()?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let s: [u8; 0x3C] = r.arr()?;
        r.expect_padding()?;
        let f32_at = |o: usize| f32::from_be_bytes(s[o..o + 4].try_into().unwrap());
        let mut unk = [0; 25];
        unk[..8].copy_from_slice(&s[0xC..0x14]);
        unk[8..].copy_from_slice(&s[0x1D..0x2E]);
        ensure!(s[0x34] == n, "UVEN: stored model count {} != {n}", s[0x34]);
        Ok(Self {
            models,
            screen: s[0..4].try_into().unwrap(),
            fog_color: s[4..8].try_into().unwrap(),
            unused_color: s[8..12].try_into().unwrap(),
            fog_min: f32_at(0x14),
            fog_max: f32_at(0x18),
            fog_enabled: s[0x1C] != 0,
            clear: s[0x2E] != 0,
            unk,
        })
    }

    /// The fog factor `_uvEnvDraw` passes to `uvGfxSetFogFactor`
    /// (`fogMin / fogMax` if enabled, else 0).
    pub fn fog_factor(&self) -> f32 {
        if self.fog_enabled && self.fog_max != 0.0 {
            self.fog_min / self.fog_max
        } else {
            0.0
        }
    }
}

/// Parses every environment in the `UVEN` file (index = env id).
pub fn parse(form: &Form) -> Result<Vec<Uven>> {
    ensure!(form.tag.0 == *b"UVEN", "not a UVEN file ({})", form.tag);
    form.blocks
        .iter()
        .filter(|b| b.tag.0 == *b"COMM")
        .enumerate()
        .map(|(i, b)| Uven::parse_comm(&b.data).with_context(|| format!("env {i}")))
        .collect()
}
