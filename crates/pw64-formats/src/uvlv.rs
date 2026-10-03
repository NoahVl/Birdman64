//! `UVLV` "levels": load lists (which terras, lights, environments, models,
//! contours, textures, sequences, animations, fonts and blits to load).
//! Mirrors `_uvExpandTextureCpy` (the UVLV parser, misnamed in the decomp)
//! and `uvLevelAppend` in `decomp/src/kernel/texture.c` (`ParsedUVLV` in
//! `uv_graphics.h`).
//!
//! The single UVLV file holds one `COMM` block per level (level id = COMM
//! index). Each block is ten lists in this order, each a u16 count followed
//! by that many u16 global ids: terras, lights (UVLT), environments (UVEN),
//! models, contours, textures, sequences (UVSQ), animations, fonts, blits.
//!
//! A game "level" is several of these appended on top of each other (see
//! [`EnvSetup`]): the map's level, shared ones (0x1A, 0xC, 0xD, 0x2E, vehicle
//! and pilot levels) and the environment's own level (`env_802E1A80`).

use crate::reader::Reader;
use anyhow::{Context, Result, ensure};
use pw64_rom::Form;

/// `ParsedUVLV`: global ids per asset type.
#[derive(Debug, Clone, Default)]
pub struct Uvlv {
    pub terras: Vec<u16>,
    pub lights: Vec<u16>,
    pub environments: Vec<u16>,
    pub models: Vec<u16>,
    pub contours: Vec<u16>,
    pub textures: Vec<u16>,
    pub sequences: Vec<u16>,
    pub animations: Vec<u16>,
    pub fonts: Vec<u16>,
    pub blits: Vec<u16>,
}

impl Uvlv {
    /// Mirrors `_uvExpandTextureCpy`.
    pub fn parse_comm(b: &[u8]) -> Result<Self> {
        let mut r = Reader::new(b, "UVLV COMM");
        let mut list = || -> Result<Vec<u16>> {
            let n = r.u16()?;
            (0..n).map(|_| r.u16()).collect()
        };
        let lv = Self {
            terras: list()?,
            lights: list()?,
            environments: list()?,
            models: list()?,
            contours: list()?,
            textures: list()?,
            sequences: list()?,
            animations: list()?,
            fonts: list()?,
            blits: list()?,
        };
        r.expect_padding()?;
        Ok(lv)
    }

    /// The lists with their names, in file order.
    pub fn lists(&self) -> [(&'static str, &[u16]); 10] {
        [
            ("terras", &self.terras),
            ("lights", &self.lights),
            ("environments", &self.environments),
            ("models", &self.models),
            ("contours", &self.contours),
            ("textures", &self.textures),
            ("sequences", &self.sequences),
            ("animations", &self.animations),
            ("fonts", &self.fonts),
            ("blits", &self.blits),
        ]
    }
}

/// Parses every level in the `UVLV` file (index = level id).
pub fn parse(form: &Form) -> Result<Vec<Uvlv>> {
    ensure!(form.tag.0 == *b"UVLV", "not a UVLV file ({})", form.tag);
    form.blocks
        .iter()
        .filter(|b| b.tag.0 == *b"COMM")
        .enumerate()
        .map(|(i, b)| Uvlv::parse_comm(&b.data).with_context(|| format!("level {i}")))
        .collect()
}

/// The four islands (`enum MapId` in `decomp/src/app/task.h`; the value is
/// also the map's UVLV level id).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Map {
    HolidayIsland = 1,
    CrescentIsland = 3,
    LittleStates = 5,
    EverFrostIsland = 10,
}

impl Map {
    /// Terra used in normal tests (`taskInitTest`).
    pub fn terra(self) -> u16 {
        match self {
            Map::HolidayIsland => 0,
            Map::CrescentIsland => 1,
            Map::LittleStates => 3,
            Map::EverFrostIsland => 7,
        }
    }
}

/// How the game sets up a flight for environment id `env`
/// (`envGetCurrentId`, `envLoadTerrainPal`, `env_802E1A80`, `levelLoad`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnvSetup {
    pub env: u16,
    pub map: Map,
    /// Condition index (time of day / weather) within the map, the value
    /// `envGetCurrentId` switches on.
    pub condition: u8,
    /// UVTP palette (`uvMemLoadPal`), if any.
    pub palette: Option<u16>,
    /// The environment's own UVLV level (`env_802E1A80`: 0x70 + env).
    pub env_level: u16,
}

impl EnvSetup {
    /// Levels `levelLoad` appends for every map (besides pilot/vehicle ones).
    pub const SHARED_LEVELS: [u16; 4] = [0x1A, 0xC, 0xD, 0x2E];

    /// All flyable environments, 2..=21.
    pub fn all() -> impl Iterator<Item = EnvSetup> {
        (2..=21).filter_map(Self::for_env)
    }

    pub fn for_env(env: u16) -> Option<Self> {
        // envGetCurrentId, inverted.
        let (map, condition) = match env {
            2..=5 => (Map::HolidayIsland, [0, 1, 2, 4][env as usize - 2]),
            6 => (Map::HolidayIsland, 5),
            7..=10 => (Map::CrescentIsland, env as u8 - 7),
            11 => (Map::CrescentIsland, 5),
            12..=17 => (Map::LittleStates, env as u8 - 12),
            18..=20 => (Map::EverFrostIsland, env as u8 - 18),
            21 => (Map::EverFrostIsland, 5),
            _ => return None,
        };
        // envLoadTerrainPal.
        let palette = match env {
            6 => Some(0),
            11 => Some(1),
            17 => Some(2),
            18 | 19 => Some(5),
            20 => Some(4),
            21 => Some(3),
            _ => None,
        };
        Some(Self {
            env,
            map,
            condition,
            palette,
            env_level: 0x70 + env,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_table() {
        let all: Vec<_> = EnvSetup::all().collect();
        assert_eq!(all.len(), 20);
        let e = EnvSetup::for_env(6).unwrap();
        assert_eq!(
            (e.map, e.condition, e.palette, e.env_level),
            (Map::HolidayIsland, 5, Some(0), 0x76)
        );
        assert_eq!(EnvSetup::for_env(21).unwrap().palette, Some(3));
        assert!(EnvSetup::for_env(22).is_none());
    }

    #[test]
    fn parse_level() {
        let mut b = Vec::new();
        for list in [
            &[0u16][..],
            &[],
            &[3],
            &[7, 8],
            &[],
            &[],
            &[],
            &[],
            &[],
            &[],
        ] {
            b.extend((list.len() as u16).to_be_bytes());
            list.iter().for_each(|v| b.extend(v.to_be_bytes()));
        }
        b.extend([0, 0]);
        let lv = Uvlv::parse_comm(&b).unwrap();
        assert_eq!(lv.terras, [0]);
        assert_eq!(lv.environments, [3]);
        assert_eq!(lv.models, [7, 8]);
    }
}
