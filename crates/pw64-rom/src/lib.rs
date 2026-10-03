//! Pilotwings 64 ROM access.
//!
//! Loads a user-supplied ROM (any byte order), verifies it is the supported
//! release, and exposes the game's "UV" filesystem (IFF-style `FORM` files).
//! See `decomp/docs/pilotwings64_filesystem.md` for the format reference.

pub mod fs;
pub mod mio0;
pub mod rom;

pub use fs::{FileEntry, Filesystem, Form, Tag};
pub use rom::{Rom, RomProblem, RomVersion, diagnose};
