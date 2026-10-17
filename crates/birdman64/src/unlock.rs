//! "Unlock everything" (settings screen): raises every test result in both
//! EEPROM save files to a gold medal, which also opens every class, the
//! bonus games and Birdman.
//!
//! Mirrors the save layout of `saveFileWrite` / `saveFileLoad` (decomp
//! save.c): two 0x100-byte files at EEPROM offsets 0 and 0x100, magic
//! `'P''W'` (`'p''w'` = initialised but empty), then, bit-packed LSB first
//! from bit 0x10, one 7-bit points value per test in vehicle-major order
//! (vehicles 0..7, classes 4 for the main vehicles and 3 for the bonus
//! ones, `taskGetTestCount` tests each), then 0x408 photo-flag bits; byte
//! 0xFF is the sum of bytes 0..0xFF.
//!
//! Existing progress is only ever raised (points = max(old, 100)); photos
//! and every other bit stay as they were. A file this code doesn't
//! recognise is left untouched.

/// One save file's size (`sizeof(PilotwingsSaveFile)`).
pub const FILE_SIZE: usize = 0x100;
/// Save files in the EEPROM image.
pub const FILES: usize = 2;
/// A test's points for a gold medal (also the top score).
pub const GOLD: u8 = 100;
/// Bit offset of the first points value (`bits = 0x10`).
const FIRST_BIT: usize = 0x10;
/// Bits per points value.
const POINT_BITS: usize = 7;

/// `taskGetTestCount(class, vehicle)` for the classes the save stores, per
/// vehicle (hang glider, rocket belt, gyrocopter, cannonball, sky diving,
/// jumble hopper, birdman). The bonus vehicles (3..) store 3 classes only
/// (`IS_BONUS_VEHICLE(v) ? 3 : 4`), so Birdman's 4th class is not saved.
/// Counts from the ROM's task files, as in `pw64_platform::start`.
const TESTS: [&[usize]; 7] = [
    &[1, 2, 3, 3],
    &[1, 2, 3, 3],
    &[1, 2, 3, 3],
    &[4, 4, 4],
    &[1, 1, 1],
    &[1, 1, 1],
    &[4, 4, 4],
];

/// Number of 7-bit points values in a save file.
pub fn points_count() -> usize {
    TESTS.iter().map(|c| c.iter().sum::<usize>()).sum()
}

/// Why a save file was left untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// Neither `'P''W'` nor `'p''w'`: not a save file this game wrote.
    UnknownMagic,
    /// A `'P''W'` file whose checksum doesn't match (the game itself would
    /// refuse to load it): damaged, so it is not rewritten.
    BadChecksum,
}

/// What happened to one save file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileResult {
    /// Some results were raised (or an empty file was filled).
    Unlocked,
    /// Every test was already at gold or better: unchanged.
    AlreadyUnlocked,
    /// Left untouched.
    Refused(Refused),
}

/// The checksum `saveFileWrite` stores at byte 0xFF.
pub fn checksum(file: &[u8; FILE_SIZE]) -> u8 {
    file[..FILE_SIZE - 1]
        .iter()
        .fold(0u32, |s, &b| s + b as u32) as u8
}

fn get_bits(file: &[u8; FILE_SIZE], bit: usize, n: usize) -> u8 {
    (0..n).fold(0, |v, i| {
        let b = bit + i;
        v | (((file[b / 8] >> (b % 8)) & 1) << i)
    })
}

fn set_bits(file: &mut [u8; FILE_SIZE], bit: usize, n: usize, val: u8) {
    for i in 0..n {
        let b = bit + i;
        let mask = 1u8 << (b % 8);
        if (val >> i) & 1 != 0 {
            file[b / 8] |= mask;
        } else {
            file[b / 8] &= !mask;
        }
    }
}

/// Every test's points, in save order (mirrors `saveFileLoad`).
pub fn decode_points(file: &[u8; FILE_SIZE]) -> Vec<u8> {
    (0..points_count())
        .map(|i| get_bits(file, FIRST_BIT + i * POINT_BITS, POINT_BITS))
        .collect()
}

/// Writes `points` (save order) into the file, leaving every other bit as
/// it was. Does not touch the magic or the checksum.
pub fn encode_points(file: &mut [u8; FILE_SIZE], points: &[u8]) {
    assert_eq!(points.len(), points_count());
    for (i, &p) in points.iter().enumerate() {
        set_bits(file, FIRST_BIT + i * POINT_BITS, POINT_BITS, p);
    }
}

/// One save file unlocked: `Ok(None)` when it already is. An empty
/// (`'p''w'`) file — the game never reads its body — becomes a fresh
/// all-gold one; a `'P''W'` file keeps everything but has every test raised
/// to at least gold.
pub fn unlock_file(file: &[u8; FILE_SIZE]) -> Result<Option<[u8; FILE_SIZE]>, Refused> {
    let mut out = match &file[..2] {
        b"PW" => {
            if checksum(file) != file[FILE_SIZE - 1] {
                return Err(Refused::BadChecksum);
            }
            *file
        }
        // `saveFileInit`: magic + zeros; nothing in it is ever loaded.
        b"pw" => [0u8; FILE_SIZE],
        _ => return Err(Refused::UnknownMagic),
    };
    out[0] = b'P';
    out[1] = b'W';
    let points: Vec<u8> = decode_points(&out).iter().map(|&p| p.max(GOLD)).collect();
    encode_points(&mut out, &points);
    out[FILE_SIZE - 1] = checksum(&out);
    Ok((out != *file).then_some(out))
}

/// Unlocks both save files of an EEPROM image in place (bytes past the two
/// files are left alone). A refused file is left exactly as it was.
pub fn unlock_image(image: &mut [u8]) -> [FileResult; FILES] {
    std::array::from_fn(|i| {
        let Some(slot) = image.get_mut(i * FILE_SIZE..(i + 1) * FILE_SIZE) else {
            return FileResult::Refused(Refused::UnknownMagic);
        };
        let file: &mut [u8; FILE_SIZE] = slot.try_into().unwrap();
        match unlock_file(file) {
            Ok(Some(new)) => {
                *file = new;
                FileResult::Unlocked
            }
            Ok(None) => FileResult::AlreadyUnlocked,
            Err(r) => FileResult::Refused(r),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 'P''W' file as `saveFileWrite` makes it: points, photo bits, sum.
    fn save(points: &[u8], photo_bits: &[usize]) -> [u8; FILE_SIZE] {
        let mut f = [0u8; FILE_SIZE];
        f[0] = b'P';
        f[1] = b'W';
        encode_points(&mut f, points);
        let photo0 = FIRST_BIT + points_count() * POINT_BITS;
        for &b in photo_bits {
            set_bits(&mut f, photo0 + b, 1, 1);
        }
        f[FILE_SIZE - 1] = checksum(&f);
        f
    }

    #[test]
    fn layout_fits_the_file() {
        // 27 main-vehicle tests + 12 cannonball + 3 + 3 + 12 Birdman.
        assert_eq!(points_count(), 57);
        let end = FIRST_BIT + points_count() * POINT_BITS + 0x408;
        assert!(end <= (FILE_SIZE - 1) * 8);
    }

    #[test]
    fn decode_encode_round_trip() {
        let points: Vec<u8> = (0..points_count()).map(|i| (i * 37 % 128) as u8).collect();
        let f = save(&points, &[0, 5, 0x407]);
        assert_eq!(decode_points(&f), points);
        let mut g = f;
        encode_points(&mut g, &decode_points(&f));
        assert_eq!(g, f);
    }

    #[test]
    fn progress_is_only_raised_and_photos_kept() {
        let mut points = vec![0u8; points_count()];
        points[0] = 85; // silver
        points[1] = 100; // gold
        points[2] = 127; // above gold: kept
        points[3] = 40; // partial
        let photos = [0, 1, 77, 0x407];
        let f = save(&points, &photos);
        let new = unlock_file(&f).unwrap().unwrap();
        let got = decode_points(&new);
        assert_eq!(got[0], GOLD);
        assert_eq!(got[1], GOLD);
        assert_eq!(got[2], 127);
        assert_eq!(got[3], GOLD);
        assert!(got.iter().all(|&p| p >= GOLD));
        // Photo flags (everything after the points) are untouched.
        let photo0 = FIRST_BIT + points_count() * POINT_BITS;
        for b in 0..0x408 {
            assert_eq!(
                get_bits(&new, photo0 + b, 1),
                get_bits(&f, photo0 + b, 1),
                "photo bit {b}"
            );
        }
        assert_eq!(&new[..2], b"PW");
        assert_eq!(checksum(&new), new[FILE_SIZE - 1]);
    }

    #[test]
    fn idempotent() {
        let f = save(&vec![50; points_count()], &[3]);
        let once = unlock_file(&f).unwrap().unwrap();
        assert_eq!(unlock_file(&once), Ok(None));
    }

    #[test]
    fn empty_file_becomes_all_gold() {
        let mut f = [0u8; FILE_SIZE];
        f[0] = b'p';
        f[1] = b'w';
        let new = unlock_file(&f).unwrap().unwrap();
        assert_eq!(&new[..2], b"PW");
        assert!(decode_points(&new).iter().all(|&p| p == GOLD));
        assert_eq!(checksum(&new), new[FILE_SIZE - 1]);
    }

    #[test]
    fn damaged_or_unknown_files_are_refused() {
        let mut bad = save(&vec![10; points_count()], &[]);
        bad[0x40] ^= 1;
        assert_eq!(unlock_file(&bad), Err(Refused::BadChecksum));
        assert_eq!(unlock_file(&[0u8; FILE_SIZE]), Err(Refused::UnknownMagic));
        // In an image a refused file stays byte-identical; the other one is
        // still unlocked.
        let good = save(&vec![10; points_count()], &[]);
        let mut image = vec![0u8; 0x800];
        image[..FILE_SIZE].copy_from_slice(&bad);
        image[FILE_SIZE..2 * FILE_SIZE].copy_from_slice(&good);
        image[0x300] = 0xAB; // past the two files: never touched
        let res = unlock_image(&mut image);
        assert_eq!(
            res,
            [
                FileResult::Refused(Refused::BadChecksum),
                FileResult::Unlocked
            ]
        );
        assert_eq!(&image[..FILE_SIZE], &bad[..]);
        assert_eq!(image[0x300], 0xAB);
        let second: &[u8; FILE_SIZE] = image[FILE_SIZE..2 * FILE_SIZE].try_into().unwrap();
        assert!(decode_points(second).iter().all(|&p| p == GOLD));
    }
}
