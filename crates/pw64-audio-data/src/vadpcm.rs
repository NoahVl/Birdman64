//! VADPCM decoding (the RSP audio microcode's `A_ADPCM` command).
//!
//! A frame is 9 bytes → 16 samples: a header byte (`scale_exp << 4 |
//! predictor`) and 16 signed 4-bit residuals, decoded as two groups of 8.
//! For output `k` (0..8) of a group, with `p` = the previous `order` outputs
//! (oldest first), `c = book[predictor]` and residuals `r[m] = nibble << scale_exp`:
//!
//! ```text
//! acc = Σ_j c[j][k]·p[j]  +  2048·r[k]  +  Σ_{m<k} c[order-1][k-m-1]·r[m]
//! out = clamp_s16(floor(acc / 2048))
//! ```
//!
//! This is the SDK `vadpcm_dec` matrix form (the lower-triangular part folds
//! the in-group prediction into one pass, which is what the RSP vector code
//! does). History carries across groups and frames; outputs are clamped to
//! s16 like the RSP's saturating stores, and the clamped values feed back.

use crate::bank::AdpcmBook;

pub const FRAME_BYTES: usize = 9;
pub const FRAME_SAMPLES: usize = 16;

/// Decoder history: the last 8 output samples (only the last `order` are used).
pub type State = [i16; 8];

/// Decodes one 9-byte frame, updating `state`.
pub fn decode_frame(
    book: &AdpcmBook,
    frame: &[u8; FRAME_BYTES],
    state: &mut State,
) -> [i16; FRAME_SAMPLES] {
    let header = frame[0];
    let scale_exp = header >> 4;
    // Out-of-range predictors cannot occur in valid data; clamp instead of panicking.
    let pred = usize::from(header & 0xF).min(book.npredictors - 1);
    let order = book.order;
    let last_row = book.row(pred, order - 1);

    let mut out = [0i16; FRAME_SAMPLES];
    for g in 0..2 {
        let mut r = [0i32; 8];
        for (m, v) in r.iter_mut().enumerate() {
            let byte = frame[1 + g * 4 + m / 2];
            let nib = if m % 2 == 0 { byte >> 4 } else { byte & 0xF };
            // Sign-extend the nibble, then scale.
            *v = (i32::from(nib as i8) << 28 >> 28) << scale_exp;
        }
        for k in 0..8 {
            let mut acc: i64 = 0;
            for j in 0..order {
                let prev = state[8 - order + j];
                acc += i64::from(book.row(pred, j)[k]) * i64::from(prev);
            }
            acc += 2048 * i64::from(r[k]);
            for m in 0..k {
                acc += i64::from(last_row[k - m - 1]) * i64::from(r[m]);
            }
            out[g * 8 + k] = (acc >> 11).clamp(i16::MIN.into(), i16::MAX.into()) as i16;
        }
        state.copy_from_slice(&out[g * 8..g * 8 + 8]);
    }
    out
}

/// Decodes a whole VADPCM stream from silence. A trailing partial frame is ignored.
pub fn decode(book: &AdpcmBook, data: &[u8]) -> Vec<i16> {
    let mut state = State::default();
    let mut pcm = Vec::with_capacity(data.len() / FRAME_BYTES * FRAME_SAMPLES);
    for frame in data.as_chunks::<FRAME_BYTES>().0 {
        pcm.extend_from_slice(&decode_frame(book, frame, &mut state));
    }
    pcm
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Order-2, one-predictor book with row 0 = `r0`, row 1 = `r1`.
    fn book(r0: [i16; 8], r1: [i16; 8]) -> AdpcmBook {
        AdpcmBook {
            order: 2,
            npredictors: 1,
            book: r0.iter().chain(r1.iter()).copied().collect(),
        }
    }

    /// Packs a header and 16 nibbles (-8..=7) into a frame.
    fn frame(header: u8, nibs: [i8; 16]) -> [u8; 9] {
        let mut f = [0u8; 9];
        f[0] = header;
        for i in 0..8 {
            f[1 + i] = ((nibs[2 * i] as u8 & 0xF) << 4) | (nibs[2 * i + 1] as u8 & 0xF);
        }
        f
    }

    #[test]
    fn silence() {
        let b = book([100; 8], [-200; 8]);
        assert_eq!(decode(&b, &[0u8; 27]), vec![0i16; 48]);
    }

    #[test]
    fn zero_book_is_scaled_residual() {
        let b = book([0; 8], [0; 8]);
        let n = [1, -1, 7, -8, 0, 3, -3, 2, 1, 1, 1, 1, -2, -2, -2, -2];
        let out = decode(&b, &frame(0x00, n));
        assert_eq!(out, n.map(i16::from).to_vec());
        let out = decode(&b, &frame(0x30, n)); // scale 2^3
        assert_eq!(out, n.map(|v| i16::from(v) * 8).to_vec());
    }

    #[test]
    fn in_group_and_cross_group_prediction() {
        // Last row c[1][0] = 0.5 (1024/2048): each residual leaks half into the
        // next sample inside a group (lower-triangular term) ...
        let b = book([0; 8], [1024, 0, 0, 0, 0, 0, 0, 0]);
        let mut n = [0i8; 16];
        n[0] = 4;
        n[7] = 4;
        let out = decode(&b, &frame(0, n));
        assert_eq!(&out[..3], &[4, 2, 0]);
        // ... and across the group boundary via the history term c[1][0]·p[-1].
        assert_eq!(out[7], 4);
        assert_eq!(out[8], 2);
        assert_eq!(out[9], 0);
    }

    #[test]
    fn history_crosses_frames_and_floors() {
        // c[0][0] = -0.5 weights p[-2], the second-to-last sample of the
        // previous group; frame 0 ends with out[14] = 3, out[15] = 0.
        let b = book([-1024, 0, 0, 0, 0, 0, 0, 0], [0; 8]);
        let mut n = [0i8; 16];
        n[14] = 3; // frame 0, out[14] = 3, out[15] = 0
        let mut data = frame(0, n).to_vec();
        data.extend_from_slice(&frame(0, [0; 16]));
        let out = decode(&b, &data);
        assert_eq!(out[14], 3);
        // Frame 1, k = 0: -1024·p[-2] = -1024·3 → floor(-1.5) = -2.
        assert_eq!(out[16], -2);
    }

    #[test]
    fn saturates() {
        let b = book([0; 8], [0; 8]);
        let out = decode(&b, &frame(0xF0, [7; 16])); // 7 << 15 overflows s16
        assert!(out.iter().all(|&s| s == i16::MAX));
    }
}
