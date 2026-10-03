//! `--audio`: export sound banks to WAV and sequences to `.seq`/`.mid`,
//! and print verification statistics.

use anyhow::{Context, Result, ensure};
use pw64_audio_data::{BankFile, CompactSeq, SeqFile, WaveKind, rom, vadpcm, wav};
use pw64_rom::{Filesystem, Rom};
use std::collections::HashMap;
use std::path::Path;

pub fn export(rom_img: &Rom, fs: &Filesystem, dir: &Path) -> Result<()> {
    let bytes = rom_img.bytes();
    ensure!(
        bytes.len() >= rom::END_US,
        "ROM too small for audio segments"
    );

    // Sound banks: music (raw ROM segments) and SFX (the UVSX file's blocks).
    let uvsx = fs
        .entries
        .iter()
        .find(|e| e.tag.0 == *b"UVSX")
        .context("no UVSX file")?;
    let uvsx = fs.read(uvsx)?;
    let sfx_ctl = &uvsx.block(b".CTL").context("UVSX has no .CTL")?.data;
    let sfx_tbl = &uvsx.block(b".TBL").context("UVSX has no .TBL")?.data;
    let banks: [(&str, &[u8], &[u8]); 2] = [
        (
            "music",
            &bytes[rom::CTL_US..rom::TBL_US],
            &bytes[rom::TBL_US..rom::END_US],
        ),
        ("sfx", sfx_ctl, sfx_tbl),
    ];

    let sample_dir = dir.join("samples");
    std::fs::create_dir_all(&sample_dir)?;
    for (name, ctl, tbl) in banks {
        export_bank(name, ctl, tbl, dir, &sample_dir).with_context(|| format!("{name} bank"))?;
    }

    // Sequences.
    let seq_dir = dir.join("seq");
    std::fs::create_dir_all(&seq_dir)?;
    let seq_bytes = &bytes[rom::SEQ_US..rom::CTL_US];
    let seqs = SeqFile::parse(seq_bytes)?;
    let mut total_events = 0;
    for i in 0..seqs.entries.len() {
        let data = seqs.data(seq_bytes, i)?;
        std::fs::write(seq_dir.join(format!("{i:02}.seq")), data)?;
        let cs = CompactSeq::parse(data).with_context(|| format!("sequence {i}"))?;
        let mut ntracks = 0;
        for t in 0..16 {
            if cs.tracks[t].is_some() {
                ntracks += 1;
                total_events += cs
                    .track_events(t)
                    .with_context(|| format!("sequence {i}"))?
                    .len();
            }
        }
        std::fs::write(seq_dir.join(format!("{i:02}.mid")), cs.to_midi()?)?;
        println!(
            "  seq {i:2}: {:5} bytes, {ntracks:2} tracks, division {}",
            data.len(),
            cs.division
        );
    }
    println!(
        "sequences: {} (all tracks decoded to end-of-track, {total_events} events)",
        seqs.entries.len()
    );
    println!("wrote {}", dir.display());
    Ok(())
}

/// Decoded PCM + loop points, keyed by wavetable `.ctl` offset.
type WaveCache = HashMap<u32, (Vec<i16>, Option<(u32, u32)>)>;

#[derive(Default)]
struct Stats {
    waves: usize,
    adpcm: usize,
    raw: usize,
    looped: usize,
    samples: usize,
    silent: usize,
    saturated_samples: usize,
    clipped_waves: usize,
    bad_predictor_frames: usize,
    bad_lengths: usize,
    bad_loops: usize,
    /// ADPCM loops whose stored decoder state equals our decode of the frame
    /// containing the loop start (the SDK encoder's own decode): a bit-exact
    /// check of the VADPCM decoder.
    loop_state_matches: usize,
    tbl_end: usize,
    peaks: Vec<i32>,
    rms: Vec<f64>,
}

fn export_bank(name: &str, ctl: &[u8], tbl: &[u8], dir: &Path, out: &Path) -> Result<()> {
    let file = BankFile::parse(ctl)?;
    std::fs::write(
        dir.join(format!("{name}_bank.json")),
        serde_json::to_string_pretty(&file)?,
    )?;

    let mut st = Stats::default();
    let mut decoded = WaveCache::new();
    let (mut ninst, mut nsounds) = (0, 0);
    for (bi, bank) in file.banks.iter().enumerate() {
        let Some(bank) = bank else { continue };
        let prefix = if file.banks.len() > 1 {
            format!("{name}{bi}")
        } else {
            name.to_string()
        };
        let insts = bank
            .percussion
            .iter()
            .map(|i| ("perc".to_string(), i))
            .chain(
                bank.instruments
                    .iter()
                    .enumerate()
                    .filter_map(|(n, i)| i.as_ref().map(|i| (format!("{n:03}"), i))),
            );
        for (iname, inst) in insts {
            ninst += 1;
            for (si, s) in inst.sounds.iter().enumerate() {
                nsounds += 1;
                let w = &s.wavetable;
                let (pcm, lp) = match decoded.entry(w.offset) {
                    std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                    std::collections::hash_map::Entry::Vacant(e) => {
                        let pcm = w.decode(tbl)?;
                        verify_wave(w, tbl, &pcm, &mut st)?;
                        e.insert((pcm, w.loop_points().map(|(a, b, _)| (a, b))))
                    }
                };
                let file = out.join(format!("{prefix}_{iname}_{si:02}.wav"));
                std::fs::write(file, wav::encode(pcm, bank.sample_rate as u32, *lp))?;
            }
        }
        println!(
            "{name}: bank {bi}: {} program slots, rate {} Hz, percussion {}",
            bank.instruments.len(),
            bank.sample_rate,
            bank.percussion.is_some()
        );
    }

    st.peaks.sort_unstable();
    st.rms.sort_by(f64::total_cmp);
    let pct = |v: &[f64], p: f64| {
        v.get(((v.len() - 1) as f64 * p) as usize)
            .copied()
            .unwrap_or(0.0)
    };
    let peaks: Vec<f64> = st.peaks.iter().map(|&p| f64::from(p)).collect();
    println!(
        "{name}: {ninst} instruments, {nsounds} sounds, {} unique waves ({} ADPCM, {} RAW16, {} looped), \
         {} samples ({:.1} s @22050), waves end at .tbl 0x{:X} of 0x{:X}",
        st.waves,
        st.adpcm,
        st.raw,
        st.looped,
        st.samples,
        st.samples as f64 / 22050.0,
        st.tbl_end,
        tbl.len()
    );
    println!(
        "{name}: peak min/median/max {:.0}/{:.0}/{:.0}, RMS min/median/max {:.0}/{:.0}/{:.0}; \
         silent {}, saturated samples {}, waves with >=1% saturated {}",
        pct(&peaks, 0.0),
        pct(&peaks, 0.5),
        pct(&peaks, 1.0),
        pct(&st.rms, 0.0),
        pct(&st.rms, 0.5),
        pct(&st.rms, 1.0),
        st.silent,
        st.saturated_samples,
        st.clipped_waves
    );
    println!(
        "{name}: errors: bad predictor frames {}, bad lengths {}, bad loops {}; \
         ADPCM loop states matching our decode {}/{}",
        st.bad_predictor_frames, st.bad_lengths, st.bad_loops, st.loop_state_matches, st.looped
    );
    Ok(())
}

fn verify_wave(
    w: &pw64_audio_data::WaveTable,
    tbl: &[u8],
    pcm: &[i16],
    st: &mut Stats,
) -> Result<()> {
    st.waves += 1;
    st.samples += pcm.len();
    st.tbl_end = st.tbl_end.max((w.base + w.len) as usize);
    match &w.kind {
        WaveKind::Adpcm { book, .. } => {
            st.adpcm += 1;
            let data = w.data(tbl)?;
            // The SDK pads odd byte lengths (odd frame counts) to even.
            let frames = data.len() / vadpcm::FRAME_BYTES;
            if data.len() != (frames * vadpcm::FRAME_BYTES).next_multiple_of(2) {
                st.bad_lengths += 1;
            }
            st.bad_predictor_frames += data
                .as_chunks::<{ vadpcm::FRAME_BYTES }>()
                .0
                .iter()
                .filter(|f| usize::from(f[0] & 0xF) >= book.npredictors)
                .count();
        }
        WaveKind::Raw16 { .. } => {
            st.raw += 1;
            if !w.len.is_multiple_of(2) {
                st.bad_lengths += 1;
            }
        }
    }
    if let Some((start, end, _)) = w.loop_points() {
        st.looped += 1;
        if start >= end || end as usize > pcm.len() {
            st.bad_loops += 1;
        }
        if let WaveKind::Adpcm { loop_: Some(l), .. } = &w.kind {
            let f = start as usize / vadpcm::FRAME_SAMPLES * vadpcm::FRAME_SAMPLES;
            if pcm.get(f..f + vadpcm::FRAME_SAMPLES) == Some(&l.state[..]) {
                st.loop_state_matches += 1;
            }
        }
    }
    let peak = pcm.iter().map(|&s| i32::from(s).abs()).max().unwrap_or(0);
    let rms = if pcm.is_empty() {
        0.0
    } else {
        (pcm.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>() / pcm.len() as f64).sqrt()
    };
    let clipped = pcm
        .iter()
        .filter(|&&s| s == i16::MAX || s == i16::MIN)
        .count();
    st.saturated_samples += clipped;
    if peak == 0 {
        st.silent += 1;
    }
    if clipped * 100 >= pcm.len().max(1) {
        st.clipped_waves += 1;
    }
    st.peaks.push(peak);
    st.rms.push(rms);
    Ok(())
}
