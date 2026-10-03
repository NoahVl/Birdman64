//! Big-endian cursor shared by the parsers; the Rust side of the engine's
//! `uvConsumeBytes` (sequential reads, no alignment padding).

use anyhow::{Context, Result, ensure};

pub(crate) struct Reader<'a> {
    pub b: &'a [u8],
    pub pos: usize,
    /// Label for error messages (e.g. "UVCT COMM").
    pub what: &'static str,
}

impl<'a> Reader<'a> {
    pub fn new(b: &'a [u8], what: &'static str) -> Self {
        Self { b, pos: 0, what }
    }
    pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let s = self
            .b
            .get(self.pos..self.pos + n)
            .with_context(|| format!("{} truncated at {:#x}+{n}", self.what, self.pos))?;
        self.pos += n;
        Ok(s)
    }
    pub fn arr<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(self.take(N)?.try_into().unwrap())
    }
    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.arr()?))
    }
    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.arr()?))
    }
    pub fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_be_bytes(self.arr()?))
    }
    /// `Mtx4F`: 16 floats, row-vector convention (`m[3]` = translation).
    pub fn mtx4f(&mut self) -> Result<[[f32; 4]; 4]> {
        let mut m = [[0.0; 4]; 4];
        for row in &mut m {
            for v in row.iter_mut() {
                *v = self.f32()?;
            }
        }
        Ok(m)
    }
    /// libultra fixed-point `Mtx` (s15.16: 16 integer halves, then 16
    /// fraction halves), converted like `uvMat4CopyL2F`.
    pub fn mtx_fixed(&mut self) -> Result<[[f32; 4]; 4]> {
        Ok(mtx_fixed_to_f32(&self.arr()?))
    }
    /// Blocks end with a few bytes of zero padding; anything else means the
    /// parse went off the rails.
    pub fn expect_padding(&self) -> Result<()> {
        let rest = &self.b[self.pos..];
        ensure!(
            rest.len() < 8 && rest.iter().all(|&x| x == 0),
            "{}: {} unexpected trailing bytes",
            self.what,
            rest.len()
        );
        Ok(())
    }
}

/// Converts a libultra `Mtx` (64 bytes, big-endian) to floats.
pub fn mtx_fixed_to_f32(b: &[u8; 64]) -> [[f32; 4]; 4] {
    let mut m = [[0.0; 4]; 4];
    for (i, row) in m.iter_mut().enumerate() {
        for (j, v) in row.iter_mut().enumerate() {
            let k = (i * 4 + j) * 2;
            let int = i16::from_be_bytes([b[k], b[k + 1]]) as i32;
            let frac = u16::from_be_bytes([b[32 + k], b[33 + k]]) as i32;
            *v = ((int << 16) | frac) as f32 / 65536.0;
        }
    }
    m
}
