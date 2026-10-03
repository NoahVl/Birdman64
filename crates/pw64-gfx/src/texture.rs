//! Tile → GPU texture: content keys, decoding through the TMEM model and
//! sampler state.
//!
//! A texture is identified by a 64-bit FNV-1a hash of what the tile
//! samples: format, decoded size, the TMEM bytes it covers and (for CI) the
//! TLUT. The same key is stable across runs and machines, so it doubles as
//! the texture-replacement ("texture pack") hook: see
//! [`TextureCache::replacer`].

use pw64_formats::Image;
use pw64_formats::gbi::{ImFmt, ImSiz};
use pw64_formats::tmem::{TMEM_SIZE, TlutMode, Tmem};
use std::collections::HashMap;
use std::sync::Arc;

/// GPU address mode per axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Wrap {
    Clamp,
    Repeat,
    Mirror,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SamplerKey {
    pub wrap: [Wrap; 2],
    pub linear: bool,
}

/// A decoded tile bound for sampling, plus what the shader needs to turn
/// (scaled) vertex S/T into normalized UVs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TileBinding {
    /// Content key (see module docs).
    pub key: u64,
    pub sampler: SamplerKey,
    pub width: u32,
    pub height: u32,
    /// Tile shift factors S/T (`G_TX_SHIFT`).
    pub shift: [f32; 2],
    /// Tile origin in texels (`uls`/`ult`), minus half a texel when
    /// filtering bilinearly (the RDP samples texel centers at integers).
    pub origin: [f32; 2],
}

/// Decoded textures by content key.
#[derive(Default)]
pub struct TextureCache {
    pub map: HashMap<u64, Arc<Image>>,
    /// Optional replacement hook: called once per new key with the decoded
    /// image; a returned image is used instead (any size; UVs are
    /// normalized, so higher resolutions just work).
    #[allow(clippy::type_complexity)]
    pub replacer: Option<Box<dyn Fn(u64, &Image) -> Option<Image> + Send>>,
    memo: KeyMemo,
}

struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
    fn bytes(&mut self, b: &[u8]) {
        for &x in b {
            self.0 = (self.0 ^ x as u64).wrapping_mul(0x0100_0000_01b3);
        }
    }
}

/// Pass-through hasher for keys that already are well-mixed hashes.
#[derive(Default)]
pub(crate) struct PreHashed(u64);

impl std::hash::Hasher for PreHashed {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 << 8) | b as u64;
        }
    }
    fn write_u64(&mut self, v: u64) {
        self.0 = v;
    }
}

type PreHashedMap<V> = HashMap<u64, V, std::hash::BuildHasherDefault<PreHashed>>;

/// Content key memo. The key is FNV-1a over the key input bytes (format
/// header ‖ TMEM region ‖ TLUT), which costs ~4 cycles/byte — too slow to
/// redo for every tile bind. The memo maps a fast word-wise hash of the
/// same bytes to (stored input bytes, key); a hit is *verified* by
/// comparing the stored bytes, so the result always equals the FNV key of
/// the current input (identical keys for texture packs / `PW64_DUMP_TEX`).
#[derive(Default)]
struct KeyMemo {
    /// Key input bytes of the current bind (reused buffer).
    scratch: Vec<u8>,
    entries: PreHashedMap<Vec<(Box<[u8]>, u64)>>,
    /// Stored input bytes (bounded: the memo is dropped past `MEMO_BYTES`).
    bytes: usize,
}

const MEMO_BYTES: usize = 64 << 20;

/// Fast, non-cryptographic hash of `b` (4 independent 64-bit lanes).
fn fast_hash(b: &[u8]) -> u64 {
    const K: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut lanes = [K, K ^ 1, K ^ 2, K ^ 3];
    let (chunks, rest) = b.as_chunks::<32>();
    for c in chunks {
        for (i, lane) in lanes.iter_mut().enumerate() {
            let w = u64::from_le_bytes(c[i * 8..i * 8 + 8].try_into().unwrap());
            *lane = (*lane ^ w).wrapping_mul(K).rotate_left(29);
        }
    }
    let mut h = (b.len() as u64).wrapping_mul(K);
    for (i, &x) in rest.iter().enumerate() {
        h ^= (x as u64) << ((i % 8) * 8);
        if i % 8 == 7 {
            h = h.wrapping_mul(K).rotate_left(31);
        }
    }
    for l in lanes {
        h = (h ^ l).wrapping_mul(K).rotate_left(27);
    }
    h ^ (h >> 32)
}

impl KeyMemo {
    /// FNV key of `self.scratch`.
    fn key(&mut self) -> u64 {
        let fast = fast_hash(&self.scratch);
        if let Some(list) = self.entries.get(&fast)
            && let Some((_, key)) = list.iter().find(|(b, _)| **b == *self.scratch)
        {
            return *key;
        }
        let mut hash = Fnv::new();
        hash.bytes(&self.scratch);
        if self.bytes > MEMO_BYTES {
            self.entries.clear();
            self.bytes = 0;
        }
        self.bytes += self.scratch.len();
        self.entries
            .entry(fast)
            .or_default()
            .push((self.scratch.as_slice().into(), hash.0));
        hash.0
    }
}

/// Appends `len` bytes of the ring `bank`, starting at `start` (wrapping).
fn push_wrapped(out: &mut Vec<u8>, bank: &[u8], mut start: usize, mut len: usize) {
    while len > 0 {
        let n = len.min(bank.len() - start);
        out.extend_from_slice(&bank[start..start + n]);
        len -= n;
        start = 0;
    }
}

/// `G_TX_SHIFT` factor: 1..=10 divide, 11..=15 multiply.
pub fn shift_factor(n: u8) -> f32 {
    match n {
        0 => 1.0,
        1..=10 => 1.0 / (1u32 << n) as f32,
        _ => (1u32 << (16 - n.min(16))) as f32,
    }
}

/// Size and wrap of one axis: masked non-clamped tiles repeat with period
/// `2^mask`; otherwise the tile rectangle is clamped.
fn axis(mask: u8, clamp: bool, mirror: bool, size: u32) -> (u32, Wrap) {
    if mask == 0 || clamp {
        (size, Wrap::Clamp)
    } else {
        (
            1 << mask.min(10),
            if mirror { Wrap::Mirror } else { Wrap::Repeat },
        )
    }
}

/// Decodes `tile` from TMEM (cached) and returns its binding. `None` if
/// the tile is not fully set up.
pub fn bind_tile(
    tmem: &mut Tmem,
    tile: u8,
    linear: bool,
    cache: &mut TextureCache,
) -> Option<TileBinding> {
    let t = tmem.tile(tile)?;
    let d = t.desc;
    let (w, ws) = axis(d.masks, d.cms.clamp, d.cms.mirror, t.width());
    let (h, wt) = axis(d.maskt, d.cmt.clamp, d.cmt.mirror, t.height());
    let (w, h) = (w.clamp(1, 1024), h.clamp(1, 1024));

    // Key input (see `KeyMemo`): header, TMEM region, TLUT.
    let scratch = &mut cache.memo.scratch;
    scratch.clear();
    let fmt_bits = [
        match d.format.0 {
            ImFmt::Rgba => 0,
            ImFmt::Yuv => 1,
            ImFmt::Ci => 2,
            ImFmt::Ia => 3,
            ImFmt::I => 4,
            ImFmt::Invalid(n) => n,
        },
        d.format.1 as u8,
        d.palette,
        tmem.tlut_mode() as u8,
    ];
    scratch.extend_from_slice(&fmt_bits);
    scratch.extend_from_slice(&w.to_le_bytes());
    scratch.extend_from_slice(&h.to_le_bytes());
    let bits = d.format.1.bits();
    let row = if d.line == 0 {
        (w * bits).div_ceil(64) as usize * 8
    } else {
        d.line as usize * 8
    };
    let base = d.tmem as usize * 8;
    let len = (row * h as usize).min(TMEM_SIZE);
    push_wrapped(scratch, &tmem.mem[..], base & (TMEM_SIZE - 1), len);
    if d.format.1 == ImSiz::B32 {
        // The high (B, A) bank at the same offset, wrapping within it.
        push_wrapped(scratch, &tmem.mem[0x800..], base & 0x7FF, len);
    }
    if d.format.0 == ImFmt::Ci && tmem.tlut_mode() != TlutMode::None {
        scratch.extend_from_slice(&tmem.mem[0x800..]);
    }
    let key = cache.memo.key();

    if !cache.map.contains_key(&key) {
        // Decode with the wrap-period size, then restore the tile rectangle.
        tmem.set_tile_size(tile, 0, 0, ((w - 1) << 2) as u16, ((h - 1) << 2) as u16);
        let img = tmem.decode_tile(tile);
        tmem.set_tile_size(tile, t.uls, t.ult, t.lrs, t.lrt);
        let img = img.unwrap_or_else(|e| {
            log::warn!("texture decode failed ({e}); using magenta");
            Image {
                width: w,
                height: h,
                rgba: [255, 0, 255, 255].repeat((w * h) as usize),
            }
        });
        let img = cache
            .replacer
            .as_ref()
            .and_then(|r| r(key, &img))
            .unwrap_or(img);
        cache.map.insert(key, Arc::new(img));
    }

    let half = if linear { 0.5 } else { 0.0 };
    Some(TileBinding {
        key,
        sampler: SamplerKey {
            wrap: [ws, wt],
            linear,
        },
        width: w,
        height: h,
        shift: [shift_factor(d.shifts), shift_factor(d.shiftt)],
        origin: [t.uls as f32 / 4.0 - half, t.ult as f32 / 4.0 - half],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn axis_rules() {
        assert_eq!(axis(5, false, false, 20), (32, Wrap::Repeat));
        assert_eq!(axis(5, false, true, 32), (32, Wrap::Mirror));
        assert_eq!(axis(5, true, false, 20), (20, Wrap::Clamp));
        assert_eq!(axis(0, false, false, 47), (47, Wrap::Clamp));
        assert_eq!(shift_factor(1), 0.5);
        assert_eq!(shift_factor(15), 2.0);
    }

    /// Wrapped reads match the old per-byte `(base + i) & mask` indexing.
    #[test]
    fn push_wrapped_matches_masked_indexing() {
        let bank: Vec<u8> = (0..64).map(|i| i as u8).collect();
        for (start, len) in [(0, 64), (60, 10), (10, 200), (63, 1), (5, 0)] {
            let mut out = Vec::new();
            push_wrapped(&mut out, &bank, start, len);
            let want: Vec<u8> = (0..len).map(|i| bank[(start + i) & 63]).collect();
            assert_eq!(out, want);
        }
    }

    /// Memoised keys equal plain FNV over the same bytes, also after a
    /// fast-hash bucket holds other inputs.
    #[test]
    fn memo_key_is_fnv() {
        let mut m = KeyMemo::default();
        for n in 0..300usize {
            let input: Vec<u8> = (0..n).map(|i| (i * 7 + n) as u8).collect();
            let mut f = Fnv::new();
            f.bytes(&input);
            for _ in 0..2 {
                m.scratch = input.clone();
                assert_eq!(m.key(), f.0);
            }
        }
    }
}
