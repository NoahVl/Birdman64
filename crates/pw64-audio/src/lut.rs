//! The `A_RESAMPLE` filter table: 64 phases × 4 taps (s16, Q15) inside the
//! `aspMain` microcode data segment. It is read from the user's ROM at run
//! time, never stored in this repo.

/// ROM offset (US, z64 order) of the table: `aspMain` data (`0x51B70`,
/// docs/notes/audio.md) + 0xC0.
pub const ROM_OFFSET_US: usize = 0x51C30;

/// Reads the table and sanity-checks it (phase `63 - p` is phase `p`
/// reversed, and every phase's taps sum to ≈ 1.0 in Q15).
pub fn from_rom(rom: &[u8]) -> Option<[i16; 256]> {
    let b = rom.get(ROM_OFFSET_US..ROM_OFFSET_US + 512)?;
    let lut: [i16; 256] = std::array::from_fn(|i| i16::from_be_bytes([b[2 * i], b[2 * i + 1]]));
    valid(&lut).then_some(lut)
}

/// Structural check of a resampler table (see [`from_rom`]).
pub fn valid(lut: &[i16; 256]) -> bool {
    (0..64).all(|p| {
        let a = &lut[p * 4..p * 4 + 4];
        let b = &lut[(63 - p) * 4..(63 - p) * 4 + 4];
        let sum: i32 = a.iter().map(|&v| v as i32).sum();
        a.iter().eq(b.iter().rev()) && (0x7000..0x9000).contains(&sum)
    })
}

/// Fallback when no ROM table is available (tests, other ROMs): a
/// windowed-sinc-free linear interpolator between taps 1 and 2 (the same
/// 3-sample delay as the real table). Not bit-exact with hardware.
pub fn linear() -> [i16; 256] {
    let mut lut = [0i16; 256];
    for p in 0..64 {
        let f = (p as i32 * 0x8000 + 32) / 64; // phase fraction, Q15
        lut[p * 4 + 1] = (0x7FFF - f).min(0x7FFF) as i16;
        lut[p * 4 + 2] = f as i16;
    }
    lut
}

#[cfg(test)]
mod tests {
    #[test]
    fn linear_is_monotonic_interpolator() {
        let l = super::linear();
        assert_eq!(&l[0..4], &[0, 0x7FFF, 0, 0]);
        assert_eq!(&l[128..132], &[0, 0x3FFF, 0x4000, 0]);
    }
}
