//! Typed parsers for Pilotwings 64 "UV" asset formats.
//!
//! - [`gbi`]: decoding of the N64 F3D-family display-list (GBI) commands that
//!   assets embed (texture setup and model geometry).
//! - [`tmem`]: a small model of the RDP's 4 KiB texture memory plus texel
//!   decoders, so textures are decoded exactly the way the hardware sees them.
//! - [`uvtx`]: `UVTX` textures (mirrors `_uvExpandTexture`).
//! - [`uvmd`]: `UVMD` models (mirrors `_uvParseUVMD`) plus a small F3D
//!   geometry executor ([`uvmd::Executor`]).
//! - [`uvtr`] / [`uvct`]: terrain grids and their tiles (geometry, collision,
//!   placed models).
//! - [`uvan`] (joint animations), [`uvft`] (fonts), [`uvbt`] (2D blits).
//! - [`uvlv`] (level load lists + the game's env → map/terra/palette table),
//!   [`uven`] (environments: clear/fog colors, sky/sea models), [`uvtp`]
//!   (per-environment texture remaps).

pub mod gbi;
mod reader;
pub mod tmem;
pub mod uvan;
pub mod uvbt;
pub mod uvct;
pub mod uven;
pub mod uvft;
pub mod uvlv;
pub mod uvmd;
pub mod uvtp;
pub mod uvtr;
pub mod uvtx;

pub use reader::mtx_fixed_to_f32;
pub use tmem::Image;
pub use uvan::Uvan;
pub use uvbt::Uvbt;
pub use uvct::Uvct;
pub use uven::Uven;
pub use uvft::Uvft;
pub use uvlv::Uvlv;
pub use uvmd::Uvmd;
pub use uvtp::Uvtp;
pub use uvtr::Terra;
pub use uvtx::Uvtx;
