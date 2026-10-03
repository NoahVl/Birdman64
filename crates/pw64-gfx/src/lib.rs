//! pw64-gfx: an HLE renderer for Pilotwings 64 display lists.
//!
//! The game uses **Fast3D** (plain F3D) microcode. [`Interpreter`] walks a
//! display list in any [`Memory`] (segment table included, so it works on
//! standalone asset arenas and on the game's RDRAM), does the RSP's vertex
//! work on the CPU and records RDP state into a resolution-independent
//! [`Frame`]. [`Renderer`] draws a frame into any wgpu target, generating
//! one WGSL shader per color-combiner/blender key.
//!
//! See `docs/notes/renderer.md` for the architecture and known gaps.

pub mod capture;
pub mod combiner;
pub mod frame;
pub mod interp;
pub mod matrix;
pub mod memory;
pub mod pixels;
pub mod rdp;
pub mod renderer;
pub mod shader;
pub mod texture;

pub use frame::{Anchor, Frame, N64_HEIGHT, N64_WIDTH, Wide};
pub use interp::Interpreter;
pub use memory::{Memory, VecMemory};
pub use renderer::{FbOp, RenderOptions, Renderer, TexFilter, Widescreen};

/// Device descriptor for a [`Renderer`] device. Optional features, each
/// requested only when the adapter has it (requiring a missing feature
/// fails `request_device`):
/// - `DEPTH32FLOAT_STENCIL8` (else the renderer uses 24-bit depth);
/// - `TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES`: lifts the WebGPU 1/4-sample
///   guarantee so `PW64_MSAA=8` can use the adapter's real sample counts
///   (`pw64` `opts::resolve_msaa` checks them via `get_texture_format_features`).
pub fn device_descriptor(adapter: &wgpu::Adapter) -> wgpu::DeviceDescriptor<'static> {
    wgpu::DeviceDescriptor {
        required_features: adapter.features()
            & (wgpu::Features::DEPTH32FLOAT_STENCIL8
                | wgpu::Features::TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES),
        ..Default::default()
    }
}

/// Minimal executor for wgpu's futures (they resolve immediately or after a
/// device poll on native backends).
pub fn block_on<F: std::future::Future>(f: F) -> F::Output {
    let mut f = std::pin::pin!(f);
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    loop {
        if let std::task::Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
        std::thread::yield_now();
    }
}
