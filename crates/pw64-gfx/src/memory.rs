//! The memory the display-list interpreter reads from.
//!
//! Addresses passed to [`Memory`] are *physical* (after segment
//! resolution), like the addresses the RSP DMA engine sees. For the native
//! game this is RDRAM (host `0x8000_0000 + phys`); for standalone assets it
//! is any byte arena laid out by the caller (see [`VecMemory`]).

/// Big-endian, physically addressed memory.
pub trait Memory {
    /// Reads the big-endian word at `addr` (4-byte aligned in practice).
    fn read_u32(&self, addr: u32) -> u32;

    /// Copies `out.len()` bytes starting at `addr`. The default goes
    /// through [`Memory::read_u32`]; implement it directly when you can.
    ///
    /// Structural data (display-list words, `Mtx`, `Vtx`, lights) is read
    /// as the RSP sees it: big-endian.
    fn read_bytes(&self, addr: u32, out: &mut [u8]) {
        for (i, b) in out.iter_mut().enumerate() {
            let a = addr.wrapping_add(i as u32);
            *b = self.read_u32(a & !3).to_be_bytes()[(a & 3) as usize];
        }
    }

    /// Byte-exact copy used for RDP texture/palette loads: texels reach RAM
    /// raw (as the DMA copied them), so hosts whose structural data is
    /// byteswapped on the way in must NOT swap here. The default is
    /// [`Memory::read_bytes`].
    fn read_raw(&self, addr: u32, out: &mut [u8]) {
        self.read_bytes(addr, out);
    }

    /// Optional address-map hook: maps a *raw* display-list word to the value
    /// `read_u32`/`read_bytes` should be called with. `None` (the default)
    /// keeps the classic RSP segmented resolution, `seg[(a >> 24) & 0xF] +
    /// (a & 0x00FFFFFF)` with a 29-bit mask — which collapses K0 pointers,
    /// segment addresses and full host pointers into one physical address.
    ///
    /// The native game's lists hold a mix of full host pointers (RDRAM window
    /// `0x80xxxxxx`, its thread stacks, exe image `0xC0xxxxxx`) and
    /// `OS_(PHYSICAL_TO_)K0` values, which must resolve *distinguishably*
    /// (the image and the window would otherwise alias). Such hosts return
    /// `Some(host)` for every word and read at host addresses directly.
    fn map(&self, raw: u32) -> Option<u32> {
        let _ = raw;
        None
    }
}

/// A plain byte arena starting at physical address 0. Reads past the end
/// return zero (the RDP happily reads garbage; zero is deterministic).
#[derive(Debug, Default, Clone)]
pub struct VecMemory {
    pub bytes: Vec<u8>,
}

impl VecMemory {
    /// Appends `data` aligned to `align` bytes; returns its physical address.
    pub fn push(&mut self, data: &[u8], align: usize) -> u32 {
        let a = self.bytes.len().next_multiple_of(align.max(1));
        self.bytes.resize(a, 0);
        self.bytes.extend_from_slice(data);
        a as u32
    }

    /// Appends big-endian words (e.g. a display list).
    pub fn push_words(&mut self, words: &[u32]) -> u32 {
        let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_be_bytes()).collect();
        self.push(&bytes, 8)
    }
}

impl Memory for VecMemory {
    fn read_u32(&self, addr: u32) -> u32 {
        let a = addr as usize;
        if let Some(b) = self.bytes.get(a..a.wrapping_add(4))
            && let Ok(b) = b.try_into()
        {
            return u32::from_be_bytes(b);
        }
        let mut b = [0; 4];
        self.read_bytes(addr, &mut b);
        u32::from_be_bytes(b)
    }

    fn read_bytes(&self, addr: u32, out: &mut [u8]) {
        let start = (addr as usize).min(self.bytes.len());
        let end = (start + out.len()).min(self.bytes.len());
        let n = end - start;
        out[..n].copy_from_slice(&self.bytes[start..end]);
        out[n..].fill(0);
    }
}
