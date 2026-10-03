//! Minimal RIFF/WAVE writer: mono 16-bit PCM, optional `smpl` loop chunk.

/// Builds a WAV file. `lp` = (start, end) sample indices of a forward loop;
/// `end` is exclusive as in libultra (`ALADPCMloop::end`), written inclusive.
pub fn encode(pcm: &[i16], sample_rate: u32, lp: Option<(u32, u32)>) -> Vec<u8> {
    let data_len = (pcm.len() * 2) as u32;
    let smpl_len = if lp.is_some() { 36 + 24 } else { 0 };
    let mut w = Vec::with_capacity(44 + data_len as usize + smpl_len as usize + 8);
    let u32le = |w: &mut Vec<u8>, v: u32| w.extend_from_slice(&v.to_le_bytes());
    w.extend_from_slice(b"RIFF");
    let riff_len = 4 + (8 + 16) + (8 + data_len) + if lp.is_some() { 8 + smpl_len } else { 0 };
    u32le(&mut w, riff_len);
    w.extend_from_slice(b"WAVEfmt ");
    u32le(&mut w, 16);
    w.extend_from_slice(&1u16.to_le_bytes()); // PCM
    w.extend_from_slice(&1u16.to_le_bytes()); // mono
    u32le(&mut w, sample_rate);
    u32le(&mut w, sample_rate * 2);
    w.extend_from_slice(&2u16.to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    u32le(&mut w, data_len);
    for s in pcm {
        w.extend_from_slice(&s.to_le_bytes());
    }
    if let Some((start, end)) = lp {
        w.extend_from_slice(b"smpl");
        u32le(&mut w, smpl_len);
        // manufacturer, product, sample period (ns), MIDI unity note (60),
        // pitch fraction, SMPTE format, SMPTE offset, loop count, sampler data
        for v in [0, 0, 1_000_000_000 / sample_rate.max(1), 60, 0, 0, 0, 1, 0] {
            u32le(&mut w, v);
        }
        // cue id, type (0 = forward), start, end (inclusive), fraction, play count (0 = infinite)
        for v in [0, 0, start, end.saturating_sub(1), 0, 0] {
            u32le(&mut w, v);
        }
    }
    w
}

#[cfg(test)]
mod tests {
    #[test]
    fn sizes_are_consistent() {
        for lp in [None, Some((2, 4))] {
            let w = super::encode(&[1, -1, 2, -2], 22050, lp);
            let riff = u32::from_le_bytes(w[4..8].try_into().unwrap()) as usize;
            assert_eq!(riff + 8, w.len());
            assert_eq!(&w[36..40], b"data");
        }
    }
}
