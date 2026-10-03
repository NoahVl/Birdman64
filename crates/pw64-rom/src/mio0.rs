//! MIO0 decompression (Nintendo's LZ variant used by the `GZIP` blocks).

use crate::rom::be_u32;
use anyhow::{Result, bail, ensure};

pub fn decompress(src: &[u8]) -> Result<Vec<u8>> {
    ensure!(
        src.len() >= 16 && &src[..4] == b"MIO0",
        "missing MIO0 header"
    );
    let out_len = be_u32(src, 4)? as usize;
    let mut comp = be_u32(src, 8)? as usize;
    let mut raw = be_u32(src, 12)? as usize;
    let mut layout = 16usize;
    let mut bits = 0u32;
    let mut bits_left = 0;
    let mut out = Vec::with_capacity(out_len);

    while out.len() < out_len {
        if bits_left == 0 {
            bits = be_u32(src, layout)?;
            layout += 4;
            bits_left = 32;
        }
        let literal = bits & 0x8000_0000 != 0;
        bits <<= 1;
        bits_left -= 1;

        if literal {
            let Some(&b) = src.get(raw) else {
                bail!("MIO0 literal read out of range")
            };
            out.push(b);
            raw += 1;
        } else {
            let Some(pair) = src.get(comp..comp + 2) else {
                bail!("MIO0 backref read out of range")
            };
            let v = u16::from_be_bytes([pair[0], pair[1]]) as usize;
            comp += 2;
            let len = (v >> 12) + 3;
            let dist = (v & 0xFFF) + 1;
            ensure!(dist <= out.len(), "MIO0 backref before start of output");
            let start = out.len() - dist;
            // Byte-by-byte on purpose: references may overlap the bytes being written.
            for i in 0..len.min(out_len - out.len()) {
                out.push(out[start + i]);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals_and_overlapping_backref() {
        // Output "abababab": 2 literals then one backref (len 6, dist 2).
        let mut src = b"MIO0".to_vec();
        src.extend(8u32.to_be_bytes()); // out len
        src.extend(20u32.to_be_bytes()); // comp offset
        src.extend(22u32.to_be_bytes()); // raw offset
        src.extend(0b1100_0000_0000_0000_0000_0000_0000_0000u32.to_be_bytes());
        src.extend(((3u16 << 12) | 1).to_be_bytes()); // len 6, dist 2
        src.extend(b"ab");
        assert_eq!(decompress(&src).unwrap(), b"abababab");
    }
}
