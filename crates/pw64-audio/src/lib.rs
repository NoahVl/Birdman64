//! Audio for the native port (Phase 5):
//! - [`abi`]: HLE of the RSP audio microcode (`aspMain`) command lists that
//!   the libultra synthesizer builds each frame.
//! - [`lut`]: the microcode's resampler table, read from the ROM.
//! - [`output`]: `cpal` output fed from the AI buffers, with drift control.
//! - [`wav`]: streaming stereo WAV dump of the AI stream.
//!
//! See docs/notes/audio.md ("RSP audio HLE").

pub mod abi;
pub mod lut;
pub mod output;
pub mod wav;

pub use abi::{AudioHle, Rdram};
pub use output::Output;
pub use wav::WavWriter;

#[cfg(test)]
mod tests;
