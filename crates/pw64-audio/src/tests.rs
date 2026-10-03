//! Command tests. Reference vectors are derived from the definitions the
//! libultra C relies on (see `abi.rs` doc): the SDK VADPCM decoder
//! (`pw64_audio_data::vadpcm`, bit-exact vs the ROM loop states), plain
//! recursions for the filters, and closed forms for the mixers.

use crate::abi::*;
use crate::lut;

fn w_setbuff(flags: u8, i: u16, o: u16, c: u16) -> [u32; 2] {
    [
        (A_SETBUFF as u32) << 24 | (flags as u32) << 16 | i as u32,
        (o as u32) << 16 | c as u32,
    ]
}

fn w(op: u8, flags: u8, lo: u16, w1: u32) -> [u32; 2] {
    [(op as u32) << 24 | (flags as u32) << 16 | lo as u32, w1]
}

fn put(h: &mut AudioHle, off: u16, s: &[i16]) {
    for (i, v) in s.iter().enumerate() {
        h.set16(off + 2 * i as u16, *v);
    }
}

fn get(h: &AudioHle, off: u16, n: usize) -> Vec<i16> {
    (0..n).map(|i| h.s16(off + 2 * i as u16)).collect()
}

fn le(s: &[i16]) -> Vec<u8> {
    s.iter().flat_map(|v| v.to_le_bytes()).collect()
}

fn hle() -> AudioHle {
    AudioHle::new(lut::linear())
}

/// Deterministic pseudo-random bytes.
fn noise(n: usize, seed: u32) -> Vec<u8> {
    let mut x = seed;
    (0..n)
        .map(|_| {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
            (x >> 16) as u8
        })
        .collect()
}

#[test]
fn clearbuff_dmemmove_and_dma_alignment() {
    let mut h = hle();
    let mut mem = vec![0u8; 0x1000];
    mem[0x100..0x120].copy_from_slice(&(0u8..32).collect::<Vec<_>>());
    // Load 13 bytes from 0x103: DMA fetches from 0x100, 16 bytes, to DMEM 0x40.
    h.run(
        &mut mem,
        &[w_setbuff(0, 0x44, 0, 13), w(A_LOADBUFF, 0, 0, 0x103)],
    );
    assert_eq!(&h.dmem()[0x40..0x50], &(0u8..16).collect::<Vec<_>>()[..]);
    assert_eq!(h.dmem()[0x50], 0);
    // DMEMMOVE 16 bytes 0x40 → 0x80, then CLEARBUFF 0x40 (count rounds to 16).
    h.run(
        &mut mem,
        &[
            w(A_DMEMMOVE, 0, 0x40, 0x80 << 16 | 16),
            w(A_CLEARBUFF, 0, 0x40, 3),
        ],
    );
    assert_eq!(&h.dmem()[0x80..0x90], &(0u8..16).collect::<Vec<_>>()[..]);
    assert!(h.dmem()[0x40..0x50].iter().all(|&b| b == 0));
    // SAVEBUFF 16 bytes back to 0x200.
    h.run(
        &mut mem,
        &[w_setbuff(0, 0, 0x80, 16), w(A_SAVEBUFF, 0, 0, 0x200)],
    );
    assert_eq!(&mem[0x200..0x210], &(0u8..16).collect::<Vec<_>>()[..]);
}

/// A_ADPCM over two commands (state carried in RDRAM) equals the SDK
/// decoder over the whole stream, with the 16 history samples first.
#[test]
fn adpcm_matches_sdk_decoder() {
    let book: Vec<i16> = noise(256, 7)
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| i16::from_le_bytes(*b) / 4)
        .collect();
    let mut stream = noise(9 * 6, 99);
    for f in stream.chunks_mut(9) {
        f[0] = (f[0] & 0x07) | 0x90; // predictors 0..7, scale 9
    }
    let expect = pw64_audio_data::vadpcm::decode(
        &pw64_audio_data::bank::AdpcmBook {
            order: 2,
            npredictors: 8,
            book: book.clone(),
        },
        &stream,
    );

    let mut mem = vec![0u8; 0x1000];
    mem[0x100..0x200].copy_from_slice(&le(&book));
    mem[0x400..0x400 + stream.len()].copy_from_slice(&stream);
    let state = 0x800;
    let mut h = hle();
    let mut out = Vec::new();
    for (k, flags) in [(0u32, A_INIT), (1, 0)] {
        // Like `_decodeChunk`: DMA from the aligned address, decode from +align.
        let src = 0x400 + 27 * k;
        let skew = (src & 7) as u16;
        h.run(
            &mut mem,
            &[
                w(A_LOADADPCM, 0, 256, 0x100),
                w_setbuff(0, 0, 0, 32 + skew),
                w(A_LOADBUFF, 0, 0, src - skew as u32),
                w_setbuff(0, skew, 0x200, 3 * 32),
                w(A_ADPCM, flags, 0, state),
            ],
        );
        let got = get(&h, 0x200, 16 * 4);
        if k == 0 {
            assert_eq!(&got[..16], &[0; 16]);
        } else {
            assert_eq!(&got[..16], &out[out.len() - 16..]); // previous frame
        }
        out.extend_from_slice(&got[16..]);
    }
    assert_eq!(out, expect);
    // The state buffer holds the last frame.
    let st: Vec<i16> = mem[state as usize..state as usize + 32]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| i16::from_le_bytes(*b))
        .collect();
    assert_eq!(st, expect[expect.len() - 16..]);

    // A_LOOP takes its history from the A_SETLOOP address instead.
    let loop_state: Vec<i16> = (0..16).map(|i| i * 50).collect();
    mem[0xA00..0xA20].copy_from_slice(&le(&loop_state));
    h.run(
        &mut mem,
        &[
            w(A_SETLOOP, 0, 0, 0xA00),
            w_setbuff(0, 0, 0x200, 0),
            w(A_ADPCM, A_LOOP, 0, state),
        ],
    );
    assert_eq!(get(&h, 0x200, 16), loop_state);
}

#[test]
fn resample_unity_is_delay_and_state_carries() {
    let input: Vec<i16> = (0..64).map(|i| (i * 37 - 900) as i16).collect();
    let run = |chunks: &[usize]| {
        let mut mem = vec![0u8; 0x100];
        let mut h = hle();
        let mut out = Vec::new();
        let mut pos = 0;
        for (n, &c) in chunks.iter().enumerate() {
            put(&mut h, 0x100, &input[pos..pos + c]);
            let flags = if n == 0 { A_INIT } else { 0 };
            h.run(
                &mut mem,
                &[
                    w_setbuff(0, 0x100, 0x300, (c * 2) as u16),
                    w(A_RESAMPLE, flags, 0x8000, 0x40),
                ],
            );
            out.extend(get(&h, 0x300, c));
            pos += c;
        }
        out
    };
    let whole = run(&[64]);
    // Phase 0 of the table selects tap 1 = the sample 3 before the output.
    assert_eq!(&whole[..3], &[0, 0, 0]);
    assert_eq!(&whole[3..], &input[..61]);
    assert_eq!(run(&[16, 32, 16]), whole);
}

#[test]
fn resample_half_pitch_interpolates() {
    let mut mem = vec![0u8; 0x100];
    let mut h = hle();
    let input: Vec<i16> = (0..16).map(|i| i * 1000).collect();
    put(&mut h, 0x100, &input);
    h.run(
        &mut mem,
        &[
            w_setbuff(0, 0x100, 0x300, 32 * 2),
            w(A_RESAMPLE, A_INIT, 0x4000, 0x40),
        ],
    );
    let out = get(&h, 0x300, 32);
    // out[2k] = s[k-3], out[2k+1] = midpoint of s[k-3], s[k-2].
    for k in 3..15 {
        assert_eq!(out[2 * k], input[k - 3]);
        assert!((out[2 * k + 1] as i32 - (input[k - 3] as i32 + 500)).abs() <= 1);
    }
    // 32 outputs at half pitch consumed 16 inputs: state = s[12..16], frac 0.
    let st: Vec<i16> = mem[0x40..0x4A]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| i16::from_le_bytes(*b))
        .collect();
    assert_eq!(st, [12000, 13000, 14000, 15000, 0]);
}

/// Real table (if a ROM is around): sanity only; never committed.
#[test]
fn rom_lut_is_valid_when_present() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../decomp/baserom.us.z64");
    if let Ok(rom) = std::fs::read(path) {
        let l = lut::from_rom(&rom).expect("resampler table at the documented offset");
        assert!(lut::valid(&l));
    }
    assert!(!lut::valid(&[0; 256]));
}

fn envmix_cmds(
    flags: u8,
    vol: [i16; 2],
    tgt: [i16; 2],
    rate: [i32; 2],
    dry: i16,
    wet: i16,
    n: u16,
) -> Vec<[u32; 2]> {
    let setvol = |f: u8, v: i16, w1: u32| w(A_SETVOL, f, v as u16, w1);
    let mut c = vec![
        w_setbuff(0, 0x000, 0x100, n * 2),
        w_setbuff(A_AUX, 0x200, 0x300, 0x400),
    ];
    if flags & A_INIT != 0 {
        c.extend([
            setvol(A_LEFT | A_VOL, vol[0], 0),
            setvol(A_VOL, vol[1], 0),
            setvol(A_LEFT, tgt[0], rate[0] as u32),
            setvol(0, tgt[1], rate[1] as u32),
            setvol(A_AUX, dry, wet as u16 as u32),
        ]);
    }
    c.push(w(A_ENVMIXER, flags | A_AUX, 0, 0x800));
    c
}

#[test]
fn envmixer_constant_volume_pans_and_sends() {
    let mut mem = vec![0u8; 0x1000];
    let mut h = hle();
    let x: Vec<i16> = (0..16).map(|i| (i - 8) * 1000).collect();
    put(&mut h, 0, &x);
    put(&mut h, 0x100, &[100; 16]); // dry L already holds another voice
    let (vl, vr, dry, wet) = (0x4000i16, 0x2000i16, 0x7000i16, 0x1000i16);
    h.run(
        &mut mem,
        &envmix_cmds(A_INIT, [vl, vr], [vl, vr], [0x10000; 2], dry, wet, 16),
    );
    let g = |v: i16, a: i16| ((v as i32 * a as i32 + 0x4000) >> 15) as i16;
    let mix = |s: i16, gain: i16| ((s as i32 * gain as i32 + 0x4000) >> 15) as i16;
    for (i, &s) in x.iter().enumerate() {
        let o = 2 * i as u16;
        assert_eq!(h.s16(0x100 + o), 100 + mix(s, g(vl, dry)));
        assert_eq!(h.s16(0x200 + o), mix(s, g(vr, dry)));
        assert_eq!(h.s16(0x300 + o), mix(s, g(vl, wet)));
        assert_eq!(h.s16(0x400 + o), mix(s, g(vr, wet)));
    }
}

/// The dev meter attributes ENVMIXER output to the last voice codebook
/// (not the reverb's 32-byte POLEF table) and splits the two banks.
#[test]
fn meter_attributes_voices_to_books() {
    let mut mem = vec![0u8; 0x8000];
    let mut h = hle();
    h.meter = Some(Box::default());
    put(&mut h, 0, &[1000; 16]);
    let v = 0x7FFFi16;
    let mut cmds = vec![w(A_LOADADPCM, 0, 128, 0x1000)];
    cmds.extend(envmix_cmds(A_INIT, [v, v], [v, v], [0x10000; 2], v, 0, 16));
    cmds.push(w(A_LOADADPCM, 0, 32, 0x7000)); // POLEF coefficients
    cmds.push(w(A_LOADADPCM, 0, 128, 0x6000));
    cmds.extend(envmix_cmds(A_INIT, [v, v], [v, v], [0x10000; 2], v, 0, 16));
    h.run(&mut mem, &cmds);
    let m = h.meter.as_ref().unwrap();
    assert_eq!(
        m.voices.keys().copied().collect::<Vec<_>>(),
        [0x1000, 0x6000]
    );
    assert_eq!(m.split(), 0x6000);
    let dry = m.voices[&0x1000][0];
    assert_eq!(dry, 2.0 * 16.0 * 1000.0f64.powi(2)); // L+R, 16 samples, ≈ unity gain
}

#[test]
fn envmixer_ramps_by_rate_per_8_samples_and_clamps() {
    let mut mem = vec![0u8; 0x1000];
    let mut h = hle();
    put(&mut h, 0, &[0x7FFF; 64]);
    // Left: 0x1000 → 0x4000, ×2 per 8 samples; right: 0x4000 → 0x1000, ×0.5.
    let cmds = envmix_cmds(
        A_INIT,
        [0x1000, 0x4000],
        [0x4000, 0x1000],
        [0x20000, 0x8000],
        0x7FFF,
        0,
        32,
    );
    h.run(&mut mem, &cmds);
    let l = get(&h, 0x100, 32);
    let r = get(&h, 0x200, 32);
    // Linear inside each 8: sample 7 reaches 2× (left) / 0.5× (right).
    assert!((l[7] as i32 - 0x2000).abs() <= 2, "{:#x}", l[7]);
    assert!((l[15] as i32 - 0x3FFF).abs() <= 2);
    assert!(l[16..].iter().all(|&v| (v as i32 - 0x3FFF).abs() <= 1));
    assert!(l.windows(2).all(|p| p[1] >= p[0]));
    assert!((r[7] as i32 - 0x2000).abs() <= 2);
    assert!(r[16..].iter().all(|&v| (v as i32 - 0x1000).abs() <= 1));
    // Continuing (state from RDRAM) keeps the reached targets.
    h.dmem_mut()[0x100..0x500].fill(0);
    h.run(&mut mem, &envmix_cmds(0, [0; 2], [0; 2], [0; 2], 0, 0, 16));
    assert!(
        get(&h, 0x100, 16)
            .iter()
            .all(|&v| (v as i32 - 0x3FFF).abs() <= 1)
    );
    assert!(
        get(&h, 0x200, 16)
            .iter()
            .all(|&v| (v as i32 - 0x1000).abs() <= 1)
    );
}

#[test]
fn mixer_saturates_and_interleave() {
    let mut mem = vec![0u8; 0x100];
    let mut h = hle();
    put(&mut h, 0x100, &[30000, -30000, 1000, -1000]);
    put(&mut h, 0x200, &[10000, -10000, 1000, 1000]);
    h.run(
        &mut mem,
        &[
            w_setbuff(0, 0, 0, 8),
            w(A_MIXER, 0, 0x7FFF, 0x100 << 16 | 0x200),
        ],
    );
    assert_eq!(get(&h, 0x200, 4), [32767, -32768, 2000, 0]);
    // Negative gain (reverb.c uses 0xda83 = -0.293).
    put(&mut h, 0x300, &[0; 8]);
    h.run(&mut mem, &[w(A_MIXER, 0, 0xC000, 0x100 << 16 | 0x300)]);
    assert_eq!(get(&h, 0x300, 2), [-15000, 15000]);

    put(&mut h, 0x440, &[1, 2, 3, 4]);
    put(&mut h, 0x580, &[-1, -2, -3, -4]);
    h.run(
        &mut mem,
        &[
            w_setbuff(0, 0, 0, 8),
            w(A_INTERLEAVE, 0, 0, 0x440 << 16 | 0x580),
        ],
    );
    assert_eq!(get(&h, 0, 8), [1, -1, 2, -2, 3, -3, 4, -4]);
}

/// A_POLEF with `_init_lpfilter`'s table equals the one-pole recursion
/// y[n] = (g·x[n] + fc·y[n-1]) / 2^14 up to rounding.
#[test]
fn polef_is_one_pole_lowpass() {
    const SCALE: i32 = 16384;
    let fc = (0x5000 * SCALE) >> 15;
    let gain = (SCALE - fc) as i16;
    let mut coef = [0i16; 16];
    let mut f = 1.0f64;
    for c in coef.iter_mut().skip(8) {
        f *= fc as f64 / SCALE as f64;
        *c = (f * SCALE as f64) as i16;
    }
    let mut mem = vec![0u8; 0x100];
    mem[0..32].copy_from_slice(&le(&coef));
    let x: Vec<i16> = (0..48)
        .map(|i| if i % 12 < 6 { 12000 } else { -8000 })
        .collect();
    let mut h = hle();
    put(&mut h, 0x100, &x[..24]);
    h.run(
        &mut mem,
        &[
            w(A_LOADADPCM, 0, 32, 0),
            w_setbuff(0, 0x100, 0x100, 48),
            w(A_POLEF, A_INIT, gain as u16, 0x40),
        ],
    );
    let mut got = get(&h, 0x100, 24);
    put(&mut h, 0x100, &x[24..]);
    h.run(&mut mem, &[w(A_POLEF, 0, gain as u16, 0x40)]);
    got.extend(get(&h, 0x100, 24));
    let mut y = 0f64;
    for (n, &s) in x.iter().enumerate() {
        y = (gain as f64 * s as f64 + fc as f64 * y) / SCALE as f64;
        assert!((got[n] as f64 - y).abs() <= 8.0, "n {n}: {} vs {y}", got[n]);
    }
}
