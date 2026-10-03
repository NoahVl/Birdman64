//! Streaming 16-bit stereo WAV writer (`PW64_DUMP_AUDIO`). The header sizes
//! are patched after every append, so the file stays valid even if the
//! process exits without dropping the writer.

use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

pub struct WavWriter {
    file: File,
    data_bytes: u32,
    rate: u32,
}

impl WavWriter {
    pub fn create(path: &Path, rate: u32) -> std::io::Result<Self> {
        let mut w = Self {
            file: File::create(path)?,
            data_bytes: 0,
            rate,
        };
        w.write_header()?;
        Ok(w)
    }

    fn write_header(&mut self) -> std::io::Result<()> {
        let mut h = Vec::with_capacity(44);
        h.extend_from_slice(b"RIFF");
        h.extend_from_slice(&(36 + self.data_bytes).to_le_bytes());
        h.extend_from_slice(b"WAVEfmt ");
        h.extend_from_slice(&16u32.to_le_bytes());
        h.extend_from_slice(&1u16.to_le_bytes()); // PCM
        h.extend_from_slice(&2u16.to_le_bytes()); // stereo
        h.extend_from_slice(&self.rate.to_le_bytes());
        h.extend_from_slice(&(self.rate * 4).to_le_bytes());
        h.extend_from_slice(&4u16.to_le_bytes());
        h.extend_from_slice(&16u16.to_le_bytes());
        h.extend_from_slice(b"data");
        h.extend_from_slice(&self.data_bytes.to_le_bytes());
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&h)
    }

    /// Appends interleaved L/R samples.
    pub fn append(&mut self, samples: &[i16]) -> std::io::Result<()> {
        let b: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        self.file.seek(SeekFrom::End(0))?;
        self.file.write_all(&b)?;
        self.data_bytes += b.len() as u32;
        self.write_header()
    }
}
