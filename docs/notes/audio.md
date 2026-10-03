# Notes: audio

Classic libultra `libaudio` (not n_audio). Code: `crates/pw64-audio-data`, export: `pw64-extract --audio`.

## ROM locations (US)
| Data | Where | Format |
|---|---|---|
| Sequence bank | ROM `0x618B70`–`0x62D460` | `ALSeqFile` 'S1', 31 compact sequences, offsets relative to file |
| Music bank | `.ctl` ROM `0x62D460`–`0x6314D0`, `.tbl` `0x6314D0`–end | `ALBankFile` 'B1', 1 bank; `.tbl` data ends at +`0xCB706`, rest of ROM is `0xFF` |
| SFX bank | the single `UVSX` filesystem file: `.CTL` + `.TBL` blocks (stored uncompressed) | 'B1', 1 bank |

Segment names/offsets come from `decomp/config/us/pilotwings64.us.yaml` (`audio_seq/ctl/tbl`).

## Contents
- Music bank: 100 program slots (48 used + percussion), 143 sounds, 57 waves. SFX bank: 120 slots
  (instrument *n* = SFX id *n*, slot 0 null, always `soundArray[0]`), 120 sounds, 107 waves.
- All banks 22050 Hz. All waves VADPCM, book order 2 × 4 predictors. No RAW16.
- Wave `len` = frames·9 padded to even (odd frame counts get 1 pad byte).
- Loops: `count` is always `0xFFFFFFFF`. `ALADPCMloop.state` = the 16 decoded samples of the frame
  containing `start` (verified bit-exact for all 71 loops → our decoder matches the SDK's).
- Null pointers: `alBnkfNew` adds the base before its null check, so offset 0 → the file header,
  whose byte 3 (`flags` if read as an instrument) is 1 → skipped. Parsers treat 0 as None.
  Seq 01 sets program 2, which is such a null slot (would read the header as an instrument).
- Sequences: compact MIDI (`ALCSeq`), division 768, 4–11 tracks. Format details in `seq.rs` doc.
  `FF 2E`/`FF 2D` loop markers are used (loop end `cur` 0xFF = forever). `.mid` export plays once.

## How the game drives the synth (`src/kernel/audio_manager.c`, `audio_seq.c`, `app/snd.c`)
- `uvaManagerInit`: heap `0x413DC` bytes at **`0x80000400`** (low RAM!). `ALSynConfig`: 32 PVoices,
  256 updates, `AL_FX_CUSTOM` reverb (8 sections, table in `uvaManagerInit`), output 22050 Hz.
- Players: one `ALCSPlayer` (16 voices/channels, 256 events) for music, one `ALSndPlayer`
  (16 sounds, 256 events) for SFX. SFX go through the emitter system (`audio_emitter.c`):
  `alSndpAllocate(instArray[sound]->soundArray[0])`.
- `uvaManager_80204518(0)` (from `snd.c`): loads both banks (ctl copied to heap, `alBnkfNew` with
  the `.tbl` **ROM address**; wave data is streamed by DMA during synthesis).
- `uvaSeqNew(n)`: copies sequence *n* from ROM into one buffer sized for the longest sequence,
  `alCSeqNew`, `alSeqpSetBank(music)`, `alSeqpSetSeq`. Tempo/volume via `uvaSeqSetTempo/Vol`.
- Audio thread (`__amMain`) runs on scheduler retrace: frame = 22050/60 → 368 samples (rounded
  to 16), ±16 adjust vs `osAiGetLength` (`EXTRA_SAMPLES` 100); 3 `AudioInfo` buffers, 2 Acmd
  lists of `0x4B00` bytes.
- DMA callback `__amDMA`: 48 cached `0x800`-byte ROM buffers, freed after `FRAME_LAG` 2 frames.
  A native port can replace it with direct slices of the ctl/tbl images.

## RSP audio (input to the HLE)
- Task `M_AUDTASK`, microcode `aspMain` (text ROM `0x48E10`, data `0x51B70`), classic ABI
  (`include/libultra/PR/abi.h`: `A_ADPCM`, `A_RESAMPLE`, `A_ENVMIXER`, `A_SETBUFF`, …), stereo
  16-bit interleaved output. Task is sent with `OS_SC_NEEDS_RDP` (unusual; harmless for HLE).
- `A_ADPCM` = `pw64_audio_data::vadpcm` (matrix form, floor(acc/2048), s16 saturation);
  `A_LOOP` flag loads `ALADPCMloop.state` as history.

## RSP audio HLE + AI (`crates/pw64-audio`, `pw64-platform/src/ai.rs`, `pw64/src/audio.rs`)
- Acmd histogram, 600 retraces from boot (title): SETBUFF 188k, LOADBUFF 44k, SAVEBUFF 42k,
  MIXER 41k, SETVOL 22k, LOADADPCM 21k, ADPCM/ENVMIXER/RESAMPLE 15k each, CLEARBUFF 9k,
  POLEF 5.4k, DMEMMOVE/INTERLEAVE/SEGMENT ~1.8k (3 per task), SETLOOP 110. No SPNOOP/unknown.
- Addresses in the list are `osVirtualToPhysical`/`K0_TO_PHYS` values: host = addr + 0x80000000
  (mod 2^32). Segments are always 0 → ignored. List = host `{u32 w0, w1}` pairs at `data_ptr`.
- DMEM: byte array, s16 host-endian; LOAD/SAVEBUFF copy raw bytes with SP-DMA rules (both
  addresses `& ~7`, length rounded up to 8 — `_decodeChunk` relies on it: loads from
  `dramLoc - align`, decodes from `inp + align`). So all s16 RDRAM buffers are host-LE.
- Semantics derived from the emitting C (MIT): ADPCM writes the previous 16 samples first, then
  `ceil(count/32)` frames (count 0 still writes the 16); RESAMPLE reads 4 tail samples before
  `in`, Q16.16 position, phase = frac>>10 (64×4 taps); ENVMIXER rate = Q16.16 multiplier per 8
  samples (`_getRate` computes `a^8`), linear inside the 8, clamp at target; SETVOL A_AUX: dry = w0
  low 16, wet = w1 **low** 16; POLEF = one-pole lowpass in 8-wide matrix form (Q14, table from
  `_init_lpfilter`). State buffers use a private layout (only the HLE reads them).
- VERSION_D `aPoleFilter` packs the gain in **8 bits**: fgain 0x1800/0x1000/0x800 → 0, so the 3
  lowpass reverb sections (2, 5, 8) output silence — faithful to the list, likely as on HW.
- Resampler table: ROM `0x51C30` (aspMain data + 0xC0), 64 phases × 4 BE s16, phase p = phase
  63−p reversed, taps sum ≈ 0x8000. Read at run time (`lut::from_rom`); `lut::linear` fallback.
- Bug found: reverb.c `&r->input[-d->output]` with u32 fields → +4G elements on 64-bit (LOADBUFF
  of 0x3520 bytes at 0xfffffbe0, then a C crash). Patched with `-(s32)`.
- AI: `osAiSetFrequency(22050)` → DAC divider 2208 → 22047 Hz. `ai.rs` drains a 2-slot FIFO
  against `osGetTime` (host clock + skipped idle), so `osAiGetLength` pacing is real; full FIFO
  → -1 (buffer lost, as HW). Samples go to the sink at submit time.
- Output (`output.rs`): ring of source frames, linear resample to the device rate, rate nudged
  ≤ ±0.5% toward a 60 ms fill (P-control; steady fill ≈ 70 ms), silence + re-prime on underrun,
  drop to target above 250 ms (unthrottled runs). 600-retrace run: 0 drops, 0 underruns.
- Verified: AI stream 367 frames/retrace, RMS ≈ 5800, no clicks (max |2nd diff| 1317), spectral
  peaks on equal-tempered notes (F3/A3/F4/G3/B3 within ~5 cents). Env: `PW64_NO_AUDIO`,
  `PW64_DUMP_AUDIO=tmp/x.wav`, `PW64_AUDIO_STATS` (histogram). Don't drop the cpal stream in a
  TLS destructor at exit (panics): `audio::finish` leaks it.
- Not bit-exact vs HW (vector accumulation order/rounding may differ by 1–2 LSB); no reference
  dump to compare against yet.

## Fidelity audit ("doesn't sound right")
- **aspMain disassembled** (ROM text `0x48E10`, loads at IMEM `0x1080`; jump table = data
  `+0x10`, 16 × u16 IMEM addrs; DMEM buffer base `0x5C0`; w0 in `k0`, w1 in `t9`; a throwaway
  RSP disassembler is enough, nothing committed). All handlers match the HLE:
  SETBUFF/SETVOL field slots, ADPCM (16 prev samples first, scale capped at 12, `>>11` floor),
  RESAMPLE (Q16 pos, phase = frac>>10, per-tap `vmulf` rounding, 8 outputs/iter), ENVMIXER
  (first 8-block linear from vol toward vol·rate, later blocks multiply every lane by rate,
  clamp to target via `vge` (rate hi ≤ 0) / `vcl`), MIXER (32 bytes/iter → HLE now aligns 32),
  INTERLEAVE (L first).
- **POLEF gain is read as 16 bits** (`andi k0,0xffff`) while the ROM's `_filterBuffer` packs
  `fgain & 0xFF` (VERSION_D macro, confirmed in the ROM: `andi t9,t7,0xff`). So the 3 lowpass
  reverb sections really are silent on hardware; the HLE is faithful. Don't "fix" it.
- Pan: every music voice is centred (L/R target ratio 0.987 = eqpower[64]/[63]) because the
  sequences send CC10 = 64 on their channels (seq 01 ch8 = 56 only). Not a bug. SFX are panned.
- Tempo (onset autocorrelation of the dump): title 112.25 BPM vs seq 00's 112; flight ≈129.3
  vs 130. No AI-frame-boundary discontinuities (|Δ²| at boundaries = elsewhere). The big |Δ²|
  bursts at 18.4/20.0/21.7 s are the menu-confirm SFX.
- **Root cause of audible dropouts (real-time only; WAV dumps can't show it):** the game keeps
  only +100 samples (4.5 ms) of AI lead; the cooperative OS delivered retraces only at OS calls,
  so host stretches (DL interpretation, loading, load on the box) made the emulated DAC starve.
  `ai.rs` restarted the DAC at `now`, losing that time → the game produced fewer samples than
  the device played → output-ring underruns (silence + 60 ms re-prime). Throttled headless
  flight: 21–106 starves / run (up to 1.5 s lost).
- **Root cause of late AI buffers (2/3 of them, gaps ≤100 ms):** gfx tasks ran synchronously at
  `osSpTaskStartGo` in `headless.rs`, so every thread the scheduler woke after it waited for the
  whole display-list HLE (~5 ms of the 16.7 ms frame) — the audio thread's `osAiSetNextBuffer`
  landed 2–5 ms after the retrace, eating most of the 4.5 ms lead. Fixed by deferring the
  started gfx task: `run_pending_rsp` runs it in the OS root loop at priority
  `RSP_PRI` (100: after scheduler/PI/audio, before game threads) and before any interrupt is
  delivered, matching the RSP/RDP running in parallel with the CPU.
- **DAC clock model (after the deferral made the catch-up path unobservable):** keeping the clock
  continuous through late submits (an earlier fix) was wrong in principle — the game paces
  `frame = 468 − left` against the *front* buffer, so it settles one buffer deep with every
  submit ~11 ms late, forever (the "2/3 late" equilibrium). Now a late submit (lag ≤
  `MAX_CATCH_UP` 150 ms) rewinds the DAC to `LEAD_FRAMES` (100) before the last buffer ended,
  replays that tail, and repays the lag at 1/`REPAY_DIV` (10 % fast); longer gaps still restart
  the clock (time lost) and forgive any unpaid lag (the host ring re-primed after its underrun;
  repaying on would overfill it). `output.rs` stats now include ms-silence and min-fill.
- `PW64_AUDIO_VIRTUAL` (with `PW64_NO_AUDIO`): silent host-clocked 48 kHz device with 10 ms
  callbacks → live underrun stats without a sound device. Verified: 3 vehicles × 2700 retraces
  real-time — 0 late / 0 starves / 0 restarts / 0 underruns, min ring fill 53 ms.
- Replay of logged push times through a model of `output.rs` (6 × 12/24 ms stalls per 42
  tasks): before 38 underruns / 2.07 s silence in 25 s; after 0 underruns, AI stream = 45.0 s.
- `output.rs` resampler linear → Catmull-Rom (linear from 22 kHz: −4 dB at 8 kHz, loud images).
- `[audio] AI:` line at exit: buffers, lost, late submits (max gap), clock restarts.
- Only a hardware/emulator capture can settle: LSB rounding, analog output filter, whether the
  mix level/brightness matches real hardware.

## "Music slow and crackly" (144 Hz laptop)
- Not the frame-rate work: headless WAVs at `PW64_FPS=60` vs `144` hold the same music (per-second
  x-corr lag 0 until the menu inputs shift it); tempo was verified earlier. Device (WASAPI
  default: HDMI, 48 kHz 2 ch F32, 480-frame buffer) = what `PW64_AUDIO_VIRTUAL`
  models; idle runs at 60/144, V-Sync on/off: produced == played ≈ 22047/s, 0 underruns.
- **Cause: CPU load from other processes** (e.g. a parallel cargo/clang build — 16 busy procs on
  16 cores). The OS core thread (the whole N64 CPU, incl. the audio thread) ran at normal
  priority: 6 ms sleeps ended after 20–40 ms. Headless 144 Hz under load: produced 16 000/s
  (music advances at ~73 % speed = "slowed"), 179 underruns / 14.4 s silence per 60 s
  ("crackle"), 93 AI clock restarts. Windowed it was worse: the game blocks in the frame-queue
  sink (`window.rs` `MAX_QUEUED_OPS`) while the starved window thread drains → 1–5 s stalls.
- Fix (`os::run`, throttled runs only, Windows): process `ABOVE_NORMAL_PRIORITY_CLASS` + game
  thread `THREAD_PRIORITY_HIGHEST` (cpal's WASAPI thread is already TIME_CRITICAL). Same load
  after: headless and window × {60,144} × V-Sync {off,on}: 22047 ± 10/s, 0 underruns,
  0 restarts over 60 s. Linux: not done (negative nice needs privileges).
- `PW64_AUDIO_STATS` now also prints a line every 5 s of wall time (produced/played frames per
  second, underruns, silence, fill, drift nudge, AI late/restarts) — the only way to see live
  health of a windowed run (killing the window skips the exit summary).
- Laptop panel seen switching 144 ↔ 60 Hz between/during runs (Windows dynamic refresh);
  the runtime rate switch itself is clean (tested headless 144 → 60 path mid-flight).

## "SFX much louder than the music"
- HLE re-checked against the emitting C (env.c, reverb.c, mainbus/auxbus.c) and the documented
  classic-ABI semantics: ENVMIXER gains = vol·dry / vol·wet (Q15), 4 outputs; fx input =
  0.707·(auxL+auxR) (`aMix` 0xda83 self + 0x5a82); fx output copied to both aux buffers and
  mixed into main at 0x7fff per main-bus source. No HLE bug found.
- **Real bug (native, not HLE): all music played with ONE instrument.** The bank deserialiser's
  arena started at 0x80200000, but `uvMemClearRegions` (memory.c, every level load) zeroes
  0x80125800..kernel_TEXT_START **0x802000A0**, i.e. the music `ALBankFile` + `ALBank` head:
  instCount → 0 (every program change fails `key < instCount`), sampleRate, percussion,
  instArray[0..15]. `__initFromBank` then gave every channel the first non-null slot,
  instArray[16]. Seen as: only 2 codebooks in ~50 k LOADADPCMs of music. Fix: `ARENA_START`
  0x802000A0 (pw64_arena.c). Now ~25 music books.
- Levels (`PW64_AUDIO_METER=1`, RMS per 5 s window; headless fly_hang_glider, 2700 retraces):
  before: flight music dry ≈3400 + reverb return ≈510 vs SFX (wind) ≈2200; AI RMS 4853,
  104 samples clipped at ±32767. After: music dry ≈2100–2250 (wet send ≈1600, return ≈550) vs
  SFX ≈2200–2400 (unchanged); AI RMS 3218, 0 clipped (peak 32217). Title: RMS 5006 → 3095.
  So the right instruments are *quieter* than instrument 16 was: music ≈ SFX by RMS in flight.
  That balance comes from game data (inst/sample volumes, CC7, `D_803505C4` per-vehicle
  sfx/music scales, emitter vols); only an emulator/HW capture can say if it matches the N64.
- The reverb return is ~1/3 of the wet send: sections 2/5/8 are silent (POLEF 8-bit gain, above).
- `PW64_AUDIO_METER=1|2`: music vs SFX (split at the largest gap between codebook addresses:
  music ctl is loaded first) dry/wet send RMS, main dry, aux return, every 300 tasks; 2 adds a
  per-book list at exit (`Meter` in abi.rs).
