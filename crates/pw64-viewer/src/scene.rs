//! Terrain scene → real Fast3D display lists in an RDRAM-like arena.
//!
//! Mirrors how the engine feeds the RSP:
//! - static data (loaded once): UVTX images + their own texture lists with
//!   `SetTImg` addresses patched like `_uvExpandTexture` does, UVCT/UVMD
//!   vertex arrays, and every render state's expanded F3D list (`G_VTX`,
//!   `G_TRI1`, `G_ENDDL`), plus `gGfxDList1`/`gGfxDList2` from graphics.c;
//! - per frame: the matrices and the main list, built like
//!   `uvGfxBegin`/`uvGfxClearScreen`/`uvGfxStateDraw`/`uvSobj_8022C8D0`.
//!
//! As in the game, matrices are camera-relative (object × look transform,
//! loaded with `G_MTX_LOAD|G_MTX_PUSH`, see `uvGfx_802236CC`); the modelview
//! "view" matrix is identity.

use crate::dl::{self, Dl, K0};
use anyhow::{Context, Result};
use pw64_formats::gbi::{geom, mtx};
use pw64_formats::uven::EnvModel;
use pw64_formats::uvmd::{State, Vtx, state as st};
use pw64_formats::{Terra, Uvct, Uven, Uvmd, Uvtp, Uvtx, uven, uvtp, uvtr};
use pw64_gfx::matrix::{self, IDENTITY, Mat4};
use pw64_gfx::memory::VecMemory;
use pw64_rom::{Filesystem, Rom};
use std::collections::HashMap;
use std::path::Path;

/// Addresses of the (never read) color and depth images.
const FRAMEBUFFER: u32 = 0x000D_A800;
const ZBUFFER: u32 = 0x003D_A800;

const MODE_MASK: u32 =
    st::GOURAUD | (1 << 18) | st::CULL_FRONT | st::CULL_BACK | st::ZBUFFER | st::LIGHTING | st::FOG;
const TEXTURE_NONE: u32 = 0xFFF;

/// Everything parsed from the ROM that a terra needs.
pub struct Assets {
    uvtx: Vec<Uvtx>,
    /// Each UVTX's texture display list as raw words (`ParsedUVTX.dlist`).
    uvtx_dl: Vec<Vec<u32>>,
    models: Vec<Uvmd>,
    contours: Vec<Uvct>,
    pub terras: Vec<Terra>,
    pub envs: Vec<Uven>,
    pub palettes: Vec<Uvtp>,
}

impl Assets {
    pub fn load(rom_path: &Path) -> Result<Self> {
        let rom = Rom::load(rom_path)?;
        let fs = Filesystem::open(&rom)?;
        let of = |tag: &'static [u8; 4]| fs.entries.iter().filter(move |e| e.tag.0 == *tag);
        let mut uvtx = Vec::new();
        let mut uvtx_dl = Vec::new();
        for e in of(b"UVTX") {
            let f = fs.read(e)?;
            let t = Uvtx::parse(&f).with_context(|| format!("UVTX {}", e.type_index))?;
            let comm = &f.block(b"COMM").unwrap().data;
            let size = u16::from_be_bytes([comm[0], comm[1]]) as usize;
            let n = u16::from_be_bytes([comm[2], comm[3]]) as usize;
            let words = comm[0x14 + size..0x14 + size + n * 8]
                .as_chunks::<4>()
                .0
                .iter()
                .map(|c| u32::from_be_bytes(*c))
                .collect();
            uvtx.push(t);
            uvtx_dl.push(words);
        }
        let models = of(b"UVMD")
            .map(|e| Uvmd::parse(&fs.read(e)?))
            .collect::<Result<Vec<_>>>()?;
        let contours = of(b"UVCT")
            .map(|e| Uvct::parse(&fs.read(e)?))
            .collect::<Result<Vec<_>>>()?;
        let one = |tag: &'static [u8; 4]| {
            fs.read(
                of(tag)
                    .next()
                    .with_context(|| format!("no {}", String::from_utf8_lossy(tag)))?,
            )
        };
        Ok(Self {
            uvtx,
            uvtx_dl,
            models,
            contours,
            terras: uvtr::parse(&one(b"UVTR")?)?,
            envs: uven::parse(&one(b"UVEN")?)?,
            palettes: uvtp::parse(&one(b"UVTP")?)?,
        })
    }
}

/// A loaded texture slot (`gLevelData.textures[id]`).
#[derive(Clone, Copy)]
struct TexInfo {
    /// Texture list address (`ParsedUVTX.dlist`).
    addr: u32,
    /// `unk12`: high 4 bits = render flags.
    flags: u16,
    channels: u8,
    /// Scroll speeds of the own image (tile 1) and image2 (tile 0).
    scroll: [[f32; 2]; 2],
    /// Width/height of the own image and of image2's texture slot.
    size: [[u16; 2]; 2],
}

type Textures = HashMap<u16, TexInfo>;

/// One `uvEnvModel`: LOD 0 / part 0 states, as `_uvEnvDraw` draws them.
struct EnvDraw {
    flags: u8,
    states: Vec<StateRef>,
}

/// A render state and the address of its expanded display list.
#[derive(Clone, Copy)]
struct StateRef {
    state: u32,
    dl: u32,
}

struct LodDraw {
    billboard: bool,
    parts: Vec<(u8, Vec<StateRef>)>,
}

struct ModelDraw {
    lods: Vec<LodDraw>,
    radii: Vec<f32>,
    scale: f32,
    transparent: bool,
}

struct Placement {
    model: u16,
    /// Per-part matrices (row vectors, tile-relative).
    matrices: Vec<Mat4>,
    pos: [f32; 3],
}

struct CellDraw {
    matrix: Mat4,
    states: Vec<StateRef>,
    placements: Vec<Placement>,
}

#[derive(Debug, Clone, Copy)]
pub struct Camera {
    /// World position (Z-up).
    pub pos: [f32; 3],
    /// Radians; yaw around +Z from +X, pitch up from the horizon.
    pub yaw: f32,
    pub pitch: f32,
    /// Vertical field of view in degrees.
    pub fov_y: f32,
}

impl Camera {
    pub fn forward(&self) -> [f32; 3] {
        let (sy, cy) = self.yaw.sin_cos();
        let (sp, cp) = self.pitch.sin_cos();
        [cp * cy, cp * sy, sp]
    }

    /// Look transform: world → camera (GL convention: -Z forward, +Y up).
    pub fn view(&self) -> Mat4 {
        let f = self.forward();
        let r = normalize([f[1], -f[0], 0.0]);
        let u = cross(r, f);
        let p = self.pos;
        let dot = |a: [f32; 3], b: [f32; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
        [
            [r[0], u[0], -f[0], 0.0],
            [r[1], u[1], -f[1], 0.0],
            [r[2], u[2], -f[2], 0.0],
            [-dot(p, r), -dot(p, u), dot(p, f), 1.0],
        ]
    }
}

fn normalize(v: [f32; 3]) -> [f32; 3] {
    let l = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt().max(1e-6);
    v.map(|c| c / l)
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

/// Perspective projection, row-vector form of `uvMat4SetFrustrum`.
fn perspective(fov_y: f32, aspect: f32, near: f32, far: f32) -> Mat4 {
    let f = 1.0 / (fov_y.to_radians() / 2.0).tan();
    let mut m = [[0.0; 4]; 4];
    m[0][0] = f / aspect;
    m[1][1] = f;
    m[2][2] = -(far + near) / (far - near);
    m[2][3] = -1.0;
    m[3][2] = -2.0 * far * near / (far - near);
    m
}

#[derive(Debug, Clone, Copy)]
pub struct Environment {
    /// `uvGfxSetFogFactor` (0 = off).
    pub fog: f32,
    pub fog_color: [u8; 3],
    pub sky: [u8; 3],
    pub near: f32,
    pub far: f32,
}

/// What to load besides the terra.
#[derive(Debug, Clone, Copy, Default)]
pub struct Setup {
    /// UVEN environment drawn before the terrain (`_uvEnvDraw`).
    pub env: Option<usize>,
    /// UVTP texture palette (`uvMemLoadPal`).
    pub palette: Option<usize>,
}

pub struct Scene {
    pub mem: VecMemory,
    static_len: usize,
    textures: Textures,
    /// Active texture palette (`D_802B53C0`).
    palette: Option<Uvtp>,
    env: Vec<EnvDraw>,
    models: HashMap<u16, ModelDraw>,
    cells: Vec<CellDraw>,
    dlist1: u32,
    dlist2: u32,
    viewport: u32,
    pub bounds: ([f32; 3], [f32; 3]),
    pub stats: String,
}

fn vtx_bytes(v: &[Vtx]) -> Vec<u8> {
    let mut b = Vec::with_capacity(v.len() * 16);
    for v in v {
        for p in v.pos {
            b.extend_from_slice(&p.to_be_bytes());
        }
        b.extend_from_slice(&v.flag.to_be_bytes());
        for s in v.st {
            b.extend_from_slice(&s.to_be_bytes());
        }
        b.extend_from_slice(&v.color);
    }
    b
}

/// Encodes a state's geometry list with `G_VTX` addresses rebased to `vtx_base`.
fn state_list(mem: &mut VecMemory, s: &State, vtx_base: u32) -> StateRef {
    let mut words = Vec::with_capacity(s.dlist.len() * 2);
    for g in &s.dlist {
        let (w0, mut w1) = g.encode().expect("geometry command");
        if w0 >> 24 == pw64_formats::gbi::op::G_VTX as u32 {
            w1 = (w1 + vtx_base) | K0;
        }
        words.extend([w0, w1]);
    }
    StateRef {
        state: s.state,
        dl: mem.push_words(&words),
    }
}

impl Scene {
    pub fn new(a: &Assets, terra: usize, setup: Setup) -> Result<Self> {
        let t = a.terras.get(terra).context("terra index out of range")?;
        let palette = match setup.palette {
            Some(p) => Some(a.palettes.get(p).context("palette out of range")?.clone()),
            None => None,
        };
        let mut mem = VecMemory::default();
        // Keep address 0 unused.
        mem.push(&[0; 64], 8);

        let mut dlist1 = Dl::default();
        dlist1
            .pipe_sync()
            .cycle_type(1)
            .othermode_h(23, 1, 0) // G_PM_NPRIMITIVE
            .combine(dl::CC_SHADE, dl::CC_PASS2)
            .othermode_h(8, 1, 0) // G_CK_NONE
            .othermode_l(0, 2, 0) // G_AC_NONE
            .othermode_l(2, 1, 0) // G_ZS_PIXEL
            .render_mode(dl::G_RM_OPA_SURF, dl::G_RM_OPA_SURF2)
            .othermode_h(6, 2, 0) // G_CD_MAGICSQ
            .othermode_h(14, 2, 0) // G_TT_NONE
            .othermode_h(19, 1, 1 << 19) // G_TP_PERSP
            .othermode_h(9, 3, 6 << 9) // G_TC_FILT
            .texture_off()
            .end();
        let dlist1 = mem.push_words(&dlist1.0);
        let mut dlist2 = Dl::default();
        dlist2
            .set_geometry(geom::G_SHADE)
            .cycle_type(1)
            .othermode_h(16, 1, 0) // G_TL_TILE
            .combine(dl::CC_SHADE, dl::CC_PASS2)
            .end();
        let dlist2 = mem.push_words(&dlist2.0);
        let mut vp = Vec::new();
        for v in [640i16, 480, 511, 0, 640, 480, 511, 0] {
            vp.extend_from_slice(&v.to_be_bytes());
        }
        let viewport = mem.push(&vp, 8);

        let mut scene = Self {
            mem,
            static_len: 0,
            textures: HashMap::new(),
            palette,
            env: Vec::new(),
            models: HashMap::new(),
            cells: Vec::new(),
            dlist1,
            dlist2,
            viewport,
            bounds: (t.min, t.max),
            stats: String::new(),
        };
        let mut images = HashMap::<u16, u32>::new();
        let mut tris = 0usize;
        for row in 0..t.rows as usize {
            for col in 0..t.cols as usize {
                let Some(cell) = t.cell(col, row) else {
                    continue;
                };
                let c = a
                    .contours
                    .get(cell.contour as usize)
                    .context("contour out of range")?;
                let base = scene.mem.push(&vtx_bytes(&c.vertices), 8);
                let mut states = Vec::new();
                for s in &c.states {
                    scene.add_texture(a, &mut images, s.state.texture());
                    tris += s.state.tri_count as usize;
                    states.push(state_list(&mut scene.mem, &s.state, base));
                }
                let mut placements = Vec::new();
                for p in &c.placements {
                    if p.model == 0xFFFF || a.models.get(p.model as usize).is_none() {
                        continue;
                    }
                    scene.add_model(a, &mut images, p.model);
                    placements.push(Placement {
                        model: p.model,
                        matrices: p.matrices.clone(),
                        pos: p.pos,
                    });
                }
                scene.cells.push(CellDraw {
                    matrix: cell.matrix,
                    states,
                    placements,
                });
            }
        }
        if let Some(e) = setup.env {
            let uven = a.envs.get(e).context("environment out of range")?;
            for m in &uven.models {
                if a.models.get(m.model as usize).is_none() {
                    continue;
                }
                scene.add_model(a, &mut images, m.model);
                let lod = &scene.models[&m.model].lods[0];
                scene.env.push(EnvDraw {
                    flags: m.flags,
                    states: lod.parts.first().map(|p| p.1.clone()).unwrap_or_default(),
                });
            }
        }
        scene.static_len = scene.mem.bytes.len();
        scene.stats = format!(
            "terra {terra} (env {:?}, palette {:?}): {} cells, {} terrain tris, {} models, {} textures, arena {} KiB, box {:?}..{:?}",
            setup.env,
            setup.palette,
            scene.cells.len(),
            tris,
            scene.models.len(),
            scene.textures.len(),
            scene.static_len / 1024,
            t.min,
            t.max
        );
        Ok(scene)
    }

    /// Loads texture slot `id` (and its second image) like `uvLevelAppend` +
    /// `_uvExpandTexture`: with a palette, the slot holds UVTX `remap(id)`,
    /// and image2 is looked up by slot, so it is remapped too.
    fn add_texture(&mut self, a: &Assets, images: &mut HashMap<u16, u32>, id: Option<u16>) {
        let Some(id) = id else { return };
        if self.textures.contains_key(&id) {
            return;
        }
        let remap = |id: u16| self.palette.as_ref().map_or(id, |p| p.remap(id));
        let src = remap(id);
        let Some(t) = a.uvtx.get(src as usize) else {
            return;
        };
        let image2 = remap(t.image2);
        let mut image = |id: u16, mem: &mut VecMemory| {
            *images
                .entry(id)
                .or_insert_with(|| mem.push(&a.uvtx[id as usize].image, 8))
        };
        let own = image(src, &mut self.mem);
        let t2 = (t.image2 != pw64_formats::uvtx::NO_TEXTURE)
            .then(|| a.uvtx.get(image2 as usize))
            .flatten();
        let second = t2.map(|_| image(image2, &mut self.mem));
        let mut words = a.uvtx_dl[src as usize].clone();
        let mut n = 0;
        for pair in words.as_chunks_mut::<2>().0 {
            if pair[0] >> 24 == pw64_formats::gbi::op::G_SETTIMG as u32 {
                let base = if n == 0 { own } else { second.unwrap_or(own) };
                pair[1] = (pair[1] + base) | K0;
                n += 1;
            }
        }
        let addr = self.mem.push_words(&words);
        let t2 = t2.unwrap_or(t);
        self.textures.insert(
            id,
            TexInfo {
                addr,
                flags: t.state,
                channels: t.channels,
                scroll: t.scroll,
                size: [[t.width, t.height], [t2.width, t2.height]],
            },
        );
    }

    fn add_model(&mut self, a: &Assets, images: &mut HashMap<u16, u32>, id: u16) {
        if self.models.contains_key(&id) {
            return;
        }
        let m = &a.models[id as usize];
        let base = self.mem.push(&vtx_bytes(&m.vertices), 8);
        let mut lods = Vec::new();
        for lod in &m.lods {
            let mut parts = Vec::new();
            for p in &lod.parts {
                let mut states = Vec::new();
                for s in &p.states {
                    self.add_texture(a, images, s.texture());
                    states.push(state_list(&mut self.mem, s, base));
                }
                parts.push((p.depth, states));
            }
            lods.push(LodDraw {
                billboard: lod.billboard,
                parts,
            });
        }
        self.models.insert(
            id,
            ModelDraw {
                lods,
                radii: m.lods.iter().map(|l| l.radius).collect(),
                scale: m.scale,
                transparent: m.transparent,
            },
        );
    }

    /// A reasonable starting camera: south of the terra, looking north-down.
    pub fn default_camera(&self) -> Camera {
        let (min, max) = self.bounds;
        let c = [0, 1, 2].map(|i| (min[i] + max[i]) / 2.0);
        let ext = (max[0] - min[0]).max(max[1] - min[1]);
        Camera {
            pos: [c[0], min[1] - ext * 0.15, max[2].max(0.0) + ext * 0.25],
            yaw: std::f32::consts::FRAC_PI_2,
            pitch: -0.45,
            fov_y: 45.0,
        }
    }

    /// Builds this frame's main display list; returns its address. `time`
    /// (seconds) drives UVTX scrolling (`uvSprt_802301A4`).
    pub fn build_frame(&mut self, cam: &Camera, env: &Environment, time: f32) -> u32 {
        self.mem.bytes.truncate(self.static_len);
        let view = cam.view();
        let proj = perspective(cam.fov_y, 4.0 / 3.0, env.near, env.far);
        let mut f = FrameBuilder {
            dl: Dl::default(),
            stack_data: st::AA | st::CULL_BACK | st::GOURAUD | TEXTURE_NONE,
            bound: TEXTURE_NONE,
            fog: env.fog > 0.0,
            time,
        };
        let proj_addr = self.mem.push(&matrix::to_fixed(&proj), 8);
        let ident = self.mem.push(&matrix::to_fixed(&IDENTITY), 8);
        let fog_on = if f.fog { geom::G_FOG } else { 0 };
        let [sr, sg, sb] = env.sky;

        // uvGfxBegin + uvGfxResetState.
        f.dl.segment(0, 0)
            .color_image(FRAMEBUFFER)
            .set_geometry(
                geom::G_ZBUFFER
                    | geom::G_SHADE
                    | geom::G_CULL_BACK
                    | geom::G_SHADING_SMOOTH
                    | fog_on,
            )
            .display_list(self.dlist1)
            .depth_image(ZBUFFER);
        if f.fog {
            let min = (env.fog.min(0.996) * 1000.0) as i32;
            f.dl.fog_position(min, 1000);
        }
        // uvGfx_80222A98: clear the z-buffer through the color image.
        f.dl.scissor(0, 0, 320, 240)
            .pipe_sync()
            .render_mode(dl::G_RM_OPA_SURF, dl::G_RM_OPA_SURF2)
            .cycle_type(3)
            .fill_color(dl::rgba5551(255, 255, 240, 0))
            .color_image(ZBUFFER)
            .fill_rect(0, 0, 319, 239)
            .pipe_sync()
            .color_image(FRAMEBUFFER)
            // uvGfxClearScreen.
            .fill_color(dl::rgba5551(sr, sg, sb, 255))
            .fill_rect(0, 0, 319, 239)
            .pipe_sync()
            .cycle_type(1)
            .viewport(self.viewport)
            .fog_color(env.fog_color)
            .matrix(proj_addr, mtx::G_MTX_PROJECTION | mtx::G_MTX_LOAD)
            .matrix(ident, mtx::G_MTX_LOAD);

        if !self.env.is_empty() {
            self.draw_env(&mut f, cam, env, &view, proj_addr);
            self.draw_haze(&mut f, cam, env, &view);
        }
        // uvChan_80204FE4, after _uvEnvDraw.
        let fog_mode = if f.fog {
            dl::G_RM_FOG_SHADE_A
        } else {
            dl::G_RM_PASS
        };
        f.dl.pipe_sync()
            .set_geometry(geom::G_ZBUFFER)
            .set_geometry(geom::G_SHADE)
            .combine(dl::CC_SHADE, dl::CC_PASS2)
            .render_mode(fog_mode, dl::RM_AA_ZB_OPA_SURF2);

        let mut xlu: Vec<(f32, usize, usize)> = Vec::new();
        for (ci, cell) in self.cells.iter().enumerate() {
            let m = matrix::mul(&cell.matrix, &view);
            let addr = self.mem.push(&matrix::to_fixed(&m), 8);
            f.dl.matrix(addr, mtx::G_MTX_LOAD | mtx::G_MTX_PUSH);
            for s in &cell.states {
                f.state_draw(s, &self.textures, self.dlist2);
            }
            for (pi, p) in cell.placements.iter().enumerate() {
                let dist = Self::distance(cell, p, cam);
                if self.models[&p.model].transparent {
                    xlu.push((dist, ci, pi));
                } else {
                    Self::draw_placement(
                        &mut self.mem,
                        &self.models,
                        &self.textures,
                        self.dlist2,
                        &mut f,
                        cell,
                        p,
                        cam,
                        dist,
                    );
                }
            }
            f.dl.pop_matrix();
        }
        // Transparent models last, far to near (`_uvSortAdd`).
        xlu.sort_by(|a, b| b.0.total_cmp(&a.0));
        for (dist, ci, pi) in xlu {
            let cell = &self.cells[ci];
            let m = matrix::mul(&cell.matrix, &view);
            let addr = self.mem.push(&matrix::to_fixed(&m), 8);
            f.dl.matrix(addr, mtx::G_MTX_LOAD | mtx::G_MTX_PUSH);
            Self::draw_placement(
                &mut self.mem,
                &self.models,
                &self.textures,
                self.dlist2,
                &mut f,
                cell,
                &cell.placements[pi],
                cam,
                dist,
            );
            f.dl.pop_matrix();
        }
        f.dl.full_sync().end();
        self.mem.push_words(&f.dl.0)
    }

    /// Mirrors `_uvEnvDraw`: each env model's LOD 0 / part 0 states at the
    /// origin (or following the camera in X/Y), in model units, before the
    /// terrain.
    fn draw_env(
        &mut self,
        f: &mut FrameBuilder,
        cam: &Camera,
        env: &Environment,
        view: &Mat4,
        proj: u32,
    ) {
        let far_proj = perspective(cam.fov_y, 4.0 / 3.0, env.near, 27000.0);
        let far_proj = self.mem.push(&matrix::to_fixed(&far_proj), 8);
        for e in &self.env {
            let mut m = IDENTITY;
            if e.flags & EnvModel::FOLLOW_CAMERA != 0 {
                m[3][0] = cam.pos[0];
                m[3][1] = cam.pos[1];
            }
            f.dl.fog_color(env.fog_color);
            f.set_fog(if e.flags & EnvModel::FOG != 0 {
                env.fog
            } else {
                0.0
            });
            let far = e.flags & EnvModel::FAR_PROJECTION != 0;
            if far {
                f.dl.matrix(far_proj, mtx::G_MTX_PROJECTION | mtx::G_MTX_LOAD);
            }
            let addr = self.mem.push(&matrix::to_fixed(&matrix::mul(&m, view)), 8);
            f.dl.matrix(addr, mtx::G_MTX_LOAD | mtx::G_MTX_PUSH);
            for s in &e.states {
                let mut s = *s;
                if e.flags & EnvModel::KEEP_ZBUFFER == 0 {
                    s.state &= !st::ZBUFFER;
                }
                f.state_draw(&s, &self.textures, self.dlist2);
            }
            if far {
                f.dl.matrix(proj, mtx::G_MTX_PROJECTION | mtx::G_MTX_LOAD);
            }
            f.dl.pop_matrix();
        }
        f.set_fog(env.fog);
    }

    /// Mirrors `env_802E0CF0`, the env callback the game installs for every
    /// flight (`uvEnvFunc`): a fog-colored translucent quad across the view
    /// at the horizon, opaque at `far` (just above sea level) fading to clear
    /// on the sea at 0.875 × `far`. It hides where the sea plane is clipped.
    fn draw_haze(&mut self, f: &mut FrameBuilder, cam: &Camera, env: &Environment, view: &Mat4) {
        let far = env.far;
        f.set_fog(0.0);
        let fwd = cam.forward();
        let right = [fwd[1], -fwd[0], 0.0];
        let up = normalize(cross(right, fwd));
        let p = cam.pos;
        // Where the plane `far'` ahead, spanned by `up`, crosses z = 0.
        let sea = |d: f32| {
            let c = [0, 1, 2].map(|i| p[i] + d * fwd[i]);
            let dz = d * up[2];
            let s = if dz == 0.0 { 0.0 } else { -c[2] / dz };
            ([c[0] + s * d * up[0], c[1] + s * d * up[1]], dz)
        };
        let (bottom, dz) = sea(0.875 * far);
        if dz < far * 0.1 {
            f.set_fog(env.fog);
            return;
        }
        let (top, _) = sea(far);
        let tan_x = (cam.fov_y.to_radians() / 2.0).tan() * 4.0 / 3.0;
        let w = 2.0 * far * tan_x;
        let side = [fwd[1] * w, -fwd[0] * w];
        let h = (p[2] / 15.0).max(15.0);
        let [r, g, b] = env.fog_color;
        let mut vtx = Vec::new();
        for (xy, sign, z, a) in [
            (top, 1.0, h, 255),
            (top, -1.0, h, 255),
            (bottom, -1.0, 0.0, 0),
            (bottom, 1.0, 0.0, 0),
        ] {
            // uvVtx takes s32 and stores s16.
            for c in [xy[0] + sign * side[0], xy[1] + sign * side[1], z] {
                vtx.extend_from_slice(&(c as i32 as i16).to_be_bytes());
            }
            vtx.extend_from_slice(&[0; 6]);
            vtx.extend_from_slice(&[r, g, b, a]);
        }
        let vtx = self.mem.push(&vtx, 8);
        let saved = f.stack_data;
        let v =
            (saved | st::XLU | st::AA | st::GOURAUD | TEXTURE_NONE) & !(st::DECAL | st::ZBUFFER);
        f.apply_state(v, &self.textures, self.dlist2);
        let addr = self.mem.push(&matrix::to_fixed(view), 8);
        f.dl.matrix(addr, mtx::G_MTX_LOAD | mtx::G_MTX_PUSH)
            .vertex(vtx, 4, 0)
            .tri1(0, 1, 2)
            .tri1(0, 2, 3)
            .pop_matrix();
        f.apply_state(saved, &self.textures, self.dlist2);
        f.set_fog(env.fog);
    }

    fn distance(cell: &CellDraw, p: &Placement, cam: &Camera) -> f32 {
        let w = matrix::transform(p.pos, &cell.matrix);
        let d = [0, 1, 2].map(|i| w[i] - cam.pos[i]);
        (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
    }

    /// Mirrors `uvSobj_8022C8D0` / `uvSobj_8022CC28` (billboards).
    #[allow(clippy::too_many_arguments)]
    fn draw_placement(
        mem: &mut VecMemory,
        models: &HashMap<u16, ModelDraw>,
        textures: &Textures,
        dlist2: u32,
        f: &mut FrameBuilder,
        cell: &CellDraw,
        p: &Placement,
        cam: &Camera,
        dist: f32,
    ) {
        let model = &models[&p.model];
        // uvSobjGetLODIndex.
        let n = model.radii.len();
        if n == 0 || model.radii[n - 1] <= dist {
            return;
        }
        let lod_index = (1..=n)
            .rev()
            .find(|&i| model.radii[i - 1] < dist)
            .unwrap_or(0);
        let Some(lod) = model.lods.get(lod_index) else {
            return;
        };
        let mut mats = p.matrices.clone();
        if lod.billboard && !mats.is_empty() {
            // Face the camera about Z; tile-local camera offset.
            let dx = p.pos[0] - (cam.pos[0] - cell.matrix[3][0]);
            let dy = p.pos[1] - (cam.pos[1] - cell.matrix[3][1]);
            let l = (dx * dx + dy * dy).sqrt().max(1e-6);
            let (dx, dy) = (dx / l / model.scale, dy / l / model.scale);
            mats[0][0][0] = -dy;
            mats[0][0][1] = dx;
            mats[0][1][1] = -dy;
            mats[0][1][0] = -dx;
        }
        let parts = &lod.parts;
        for (i, (depth, states)) in parts.iter().enumerate() {
            let m = mats.get(i).copied().unwrap_or(IDENTITY);
            let addr = mem.push(&matrix::to_fixed(&m), 8);
            f.dl.matrix(addr, mtx::G_MTX_PUSH);
            for s in states {
                f.state_draw(s, textures, dlist2);
            }
            if i + 1 < parts.len() {
                let pops = *depth as i32 - parts[i + 1].0 as i32;
                for _ in 0..=pops {
                    f.dl.pop_matrix();
                }
            }
        }
        if let Some((last, _)) = parts.last() {
            for _ in 0..=*last {
                f.dl.pop_matrix();
            }
        }
    }
}

/// Per-frame copy of graphics.c's state cache (`gGfxStateStackData`, `gGfxBoundTexture`).
struct FrameBuilder {
    dl: Dl,
    stack_data: u32,
    bound: u32,
    /// `gGfxFogFactor > 0`.
    fog: bool,
    /// Seconds, for UVTX scrolling.
    time: f32,
}

impl FrameBuilder {
    /// Mirrors `uvGfxSetFogFactor`.
    fn set_fog(&mut self, factor: f32) {
        let factor = factor.clamp(0.0, 0.996);
        self.fog = factor > 0.0;
        if self.fog {
            self.dl
                .set_geometry(geom::G_FOG)
                .fog_position((factor * 1000.0) as i32, 1000);
        } else {
            self.dl.clear_geometry(geom::G_FOG);
        }
    }

    /// Mirrors `uvGfxStateDrawDL` (both passes) — see `uvGfxStateDraw` for the
    /// non-DL states.
    fn state_draw(&mut self, s: &StateRef, textures: &Textures, dlist2: u32) {
        let v = if self.fog {
            s.state | st::FOG | st::GOURAUD
        } else {
            s.state
        };
        if v & st::DRAW_DL != 0 {
            // Pass 1: color image = z image (`pw64-gfx`'s `DepthImage` mode) —
            // the volume's back faces test depth and write the blender word as
            // depth; the combiner is black / alpha 1 so the word is `0x0001`.
            // The decomp pushes an identity modelview around the dlist, a no-op
            // here because state lists contain no matrix commands.
            self.dl
                .pipe_sync()
                .clear_geometry(geom::G_CULL_BACK | geom::G_LIGHTING)
                .set_geometry(geom::G_ZBUFFER | geom::G_SHADE | geom::G_CULL_FRONT)
                .cycle_type(1)
                .combine(dl::CC_BLACK_A1, dl::CC_PASS2)
                .pipe_sync()
                .render_mode(dl::G_RM_PASS, dl::RM_ZB_XLU_SURF2)
                .color_image(ZBUFFER)
                .display_list(s.dl);
            // Pass 2: the translucent front faces into the framebuffer.
            self.dl
                .pipe_sync()
                .color_image(FRAMEBUFFER)
                .combine(dl::CC_SHADE, dl::CC_PASS2)
                .render_mode(dl::G_RM_PASS, dl::RM_ZB_XLU_SURF2)
                .clear_geometry(geom::G_CULL_FRONT)
                .set_geometry(geom::G_CULL_BACK)
                .display_list(s.dl);
            return;
        }
        self.apply_state(s.state, textures, dlist2);
        self.dl.display_list(s.dl);
    }

    /// `uvGfxStateDraw` without the geometry (as `uvGfxSetFlags` & co. use it).
    fn apply_state(&mut self, state: u32, textures: &Textures, dlist2: u32) {
        let v = if self.fog {
            state | st::FOG | st::GOURAUD
        } else {
            state
        };
        let render_mask = st::DECAL | st::XLU | st::AA | st::ZBUFFER;
        let mut tex_id = v & TEXTURE_NONE;
        if v != self.stack_data {
            self.dl.pipe_sync();
            if (v & MODE_MASK) != (self.stack_data & MODE_MASK) {
                let (mut set, mut clear) = (0, 0);
                let mut flag = |state_bit: u32, g: u32| {
                    if v & state_bit != 0 {
                        set |= g;
                    } else {
                        clear |= g;
                    }
                };
                flag(st::LIGHTING, geom::G_TEXTURE_GEN | geom::G_LIGHTING);
                flag(st::CULL_BACK, geom::G_CULL_BACK);
                flag(st::CULL_FRONT, geom::G_CULL_FRONT);
                flag(st::GOURAUD, geom::G_SHADING_SMOOTH);
                flag(st::ZBUFFER, geom::G_ZBUFFER);
                flag(st::FOG, geom::G_FOG);
                if clear != 0 {
                    self.dl.clear_geometry(clear);
                }
                if set != 0 {
                    self.dl.set_geometry(set);
                }
            }
            if tex_id == 0xFFE {
                tex_id = TEXTURE_NONE;
            }
            let tex = (tex_id != TEXTURE_NONE)
                .then(|| textures.get(&(tex_id as u16)))
                .flatten();
            if tex.is_none() {
                tex_id = TEXTURE_NONE;
            }
            let bound = match tex {
                Some(t) => (t.flags as u32 & 0xF000) | tex_id,
                None => TEXTURE_NONE,
            };
            if self.bound != bound {
                match tex {
                    None => {
                        self.dl.display_list(dlist2);
                    }
                    Some(t) => self.texture_draw(t),
                };
            }
            // uvGfxTextureDL: turning texturing off for untextured states.
            let decal_toggle = (self.stack_data ^ v) & st::DECAL != 0;
            if tex_id == TEXTURE_NONE
                && ((self.stack_data & TEXTURE_NONE) != TEXTURE_NONE || decal_toggle)
            {
                self.dl.texture_off();
            }
            let xlu_surf =
                tex.is_some_and(|t| t.flags & 0x8000 != 0 || t.channels == 1 || v & (1 << 26) != 0);
            let mode2 = match v & render_mask {
                x if x == st::DECAL | st::XLU | st::AA
                    || x == st::DECAL | st::XLU | st::AA | st::ZBUFFER =>
                {
                    dl::RM_AA_ZB_XLU_DECAL2
                }
                x if x == st::DECAL | st::XLU || x == st::DECAL | st::XLU | st::ZBUFFER => {
                    dl::RM_ZB_XLU_DECAL2
                }
                x if x == st::DECAL | st::AA || x == st::DECAL | st::AA | st::ZBUFFER => {
                    dl::RM_AA_ZB_OPA_DECAL2
                }
                x if x == st::DECAL || x == st::DECAL | st::ZBUFFER => dl::RM_ZB_OPA_DECAL2,
                x if x == st::XLU | st::AA | st::ZBUFFER => {
                    if tex.is_none() {
                        dl::RM_AA_ZB_XLU_INTER2
                    } else if xlu_surf {
                        dl::RM_AA_ZB_XLU_SURF2
                    } else {
                        dl::RM_AA_ZB_TEX_TERR2
                    }
                }
                x if x == st::XLU | st::AA => {
                    if tex.is_none_or(|t| t.flags & 0x8000 != 0) {
                        dl::RM_AA_XLU_SURF2
                    } else {
                        dl::RM_AA_TEX_TERR2
                    }
                }
                x if x == st::XLU | st::ZBUFFER => dl::RM_ZB_XLU_SURF2,
                x if x == st::AA | st::ZBUFFER => dl::RM_AA_ZB_OPA_TERR2,
                x if x == st::ZBUFFER => dl::RM_ZB_OPA_SURF2,
                x if x == st::AA => dl::RM_AA_OPA_TERR2,
                x if x == st::XLU => dl::RM_XLU_SURF2,
                _ => dl::G_RM_OPA_SURF2,
            };
            let mode1 = if v & st::FOG != 0 {
                dl::G_RM_FOG_SHADE_A
            } else {
                dl::G_RM_PASS
            };
            self.dl.render_mode(mode1, mode2);
            self.bound = bound;
            self.stack_data = v;
        }
        if v & st::FOG != 0 && v & render_mask == st::XLU | st::AA | st::ZBUFFER {
            let c0 = if tex_id == TEXTURE_NONE {
                dl::CC_SHADE
            } else {
                dl::CC_MODULATEIDECALA
            };
            self.dl.combine(c0, dl::CC_PASS2);
        }
    }

    /// Mirrors `_uvTxtDraw`: the texture list, then the UVTX scroll offsets
    /// (`uvSprt_802301A4` accumulates speed × frame time, wrapped to [0, 1))
    /// as tile sizes: own image on tile 1, image2 on tile 0.
    fn texture_draw(&mut self, t: &TexInfo) {
        self.dl.display_list(t.addr);
        for (i, tile) in [(0, 1), (1, 0)] {
            let [ss, st] = t.scroll[i];
            if ss == 0.0 && st == 0.0 {
                continue;
            }
            let [w, h] = t.size[i].map(|v| v as f32 * 4.0);
            let s = ((ss * self.time).rem_euclid(1.0) * w) as i32;
            let tt = ((st * self.time).rem_euclid(1.0) * h) as i32;
            self.dl
                .tile_size(tile, s, tt, w as i32 + s - 1, h as i32 + tt - 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pw64_gfx::{Interpreter, RenderOptions, Renderer, Widescreen};

    /// One flat quad (object units of 1/100 screen half-extent).
    fn quad(mem: &mut VecMemory, x0: i16, x1: i16, z: i16, rgba: [u8; 4]) -> u32 {
        let v = [(x0, -100i16), (x1, -100), (x1, 100), (x0, 100)]
            .map(|(x, y)| Vtx {
                pos: [x, y, z],
                flag: 0,
                st: [0, 0],
                color: rgba,
            })
            .to_vec();
        mem.push(&vtx_bytes(&v), 8)
    }

    /// `uvGfxStateDrawDL` through [`FrameBuilder::state_draw`]: an opaque
    /// ground quad on the right, a shadow volume over the left half (pass 1 =
    /// back faces into the z image, pass 2 = translucent front faces), then a
    /// far blue quad drawn last. Pass 1 writes nearest depth over the volume's
    /// background, so there the front faces and the blue quad are hidden (the
    /// pre-z-image viewer drew a dark green polygon instead); over the ground
    /// (inside the volume) the shadow blends half black.
    #[test]
    fn state_draw_dl_draws_both_passes() {
        let instance = wgpu::Instance::default();
        let adapter =
            pw64_gfx::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
                .ok();
        let Some(adapter) = adapter else {
            eprintln!("no GPU adapter; skipped");
            return;
        };
        let Ok((device, queue)) =
            pw64_gfx::block_on(adapter.request_device(&pw64_gfx::device_descriptor(&adapter)))
        else {
            eprintln!("no GPU device; skipped");
            return;
        };
        let mut mem = VecMemory::default();
        mem.push(&[0; 64], 8);
        let mut scale = matrix::IDENTITY;
        (0..3).for_each(|i| scale[i][i] = 0.01);
        let ident = mem.push(&matrix::to_fixed(&matrix::IDENTITY), 8);
        let mv = mem.push(&matrix::to_fixed(&scale), 8);
        let ground = quad(&mut mem, 0, 100, 0, [255, 0, 0, 255]);
        let blue = quad(&mut mem, -100, 0, 80, [0, 0, 255, 255]);
        // Shadow volume spanning the left sky and part of the ground: front
        // face (CCW = camera-facing) near at z=-50, back face (CW) far at
        // z=+50. Pass 1 (G_CULL_FRONT) draws only the back face, pass 2
        // (G_CULL_BACK) only the front face.
        let mut vtx = Vec::new();
        for (z, face) in [
            (
                -50i16,
                [(-100, -100i16), (40, -100), (40, 100), (-100, 100)],
            ),
            (50, [(40, -100), (-100, -100), (-100, 100), (40, 100)]),
        ] {
            for (x, y) in face {
                vtx.extend_from_slice(&vtx_bytes(&[Vtx {
                    pos: [x, y, z],
                    flag: 0,
                    st: [0, 0],
                    color: [0, 0, 0, 128],
                }]));
            }
        }
        let vol_vtx = mem.push(&vtx, 8);
        let mut shadow = Dl::default();
        shadow
            .vertex(vol_vtx, 8, 0)
            .tri1(0, 1, 2)
            .tri1(0, 2, 3)
            .tri1(4, 5, 6)
            .tri1(4, 6, 7)
            .end();
        let shadow_dl = mem.push_words(&shadow.0);

        // The frame: clear z + sky, matrices, ground, shadow state, blue quad.
        let mut f = FrameBuilder {
            dl: Dl::default(),
            stack_data: 0,
            bound: 0,
            fog: false,
            time: 0.0,
        };
        f.dl.segment(0, 0)
            .color_image(FRAMEBUFFER)
            .set_geometry(pw64_formats::gbi::geom::G_ZBUFFER | pw64_formats::gbi::geom::G_SHADE)
            .depth_image(ZBUFFER)
            .pipe_sync()
            .render_mode(dl::G_RM_OPA_SURF, dl::G_RM_OPA_SURF2)
            .cycle_type(3)
            .fill_color(dl::rgba5551(255, 255, 240, 0))
            .color_image(ZBUFFER)
            .fill_rect(0, 0, 319, 239)
            .pipe_sync()
            .color_image(FRAMEBUFFER)
            .fill_color(dl::rgba5551(0, 255, 0, 255))
            .fill_rect(0, 0, 319, 239)
            .pipe_sync()
            .cycle_type(1)
            .matrix(
                ident,
                pw64_formats::gbi::mtx::G_MTX_PROJECTION | pw64_formats::gbi::mtx::G_MTX_LOAD,
            )
            .matrix(mv, pw64_formats::gbi::mtx::G_MTX_LOAD);
        // Ground and blue quads: no culling, opaque, shade-colored.
        f.dl.clear_geometry(
            pw64_formats::gbi::geom::G_CULL_FRONT | pw64_formats::gbi::geom::G_CULL_BACK,
        );
        f.dl.combine(dl::CC_SHADE, dl::CC_PASS2);
        f.dl.render_mode(dl::G_RM_PASS, dl::RM_ZB_OPA_SURF2);
        f.dl.vertex(ground, 4, 0).tri1(0, 1, 2).tri1(0, 2, 3);
        f.state_draw(
            &StateRef {
                state: st::DRAW_DL | st::XLU,
                dl: shadow_dl,
            },
            &Textures::default(),
            0,
        );
        f.dl.clear_geometry(
            pw64_formats::gbi::geom::G_CULL_FRONT | pw64_formats::gbi::geom::G_CULL_BACK,
        );
        f.dl.vertex(blue, 4, 0).tri1(0, 1, 2).tri1(0, 2, 3);
        f.dl.full_sync().end();
        let dl = mem.push_words(&f.dl.0);

        let frame = Interpreter::new().run(&mem, dl);
        let modes: Vec<_> = frame.draws.iter().map(|d| d.pipeline.shader.mode).collect();
        assert_eq!(
            modes,
            [
                pw64_gfx::frame::ShaderMode::DepthClear,
                pw64_gfx::frame::ShaderMode::Fill,
                pw64_gfx::frame::ShaderMode::Normal,
                pw64_gfx::frame::ShaderMode::DepthImage,
                pw64_gfx::frame::ShaderMode::Normal,
                pw64_gfx::frame::ShaderMode::Normal,
            ],
            "z clear, sky fill, ground, z-image pass, front-face pass, blue quad"
        );
        let mut r = Renderer::new(
            &device,
            &queue,
            wgpu::TextureFormat::Rgba8Unorm,
            RenderOptions {
                msaa: 1,
                widescreen: Widescreen::Off,
                fill_view: false,
                ..Default::default()
            },
        );
        let (w, h) = (64u32, 48u32);
        let img = r.render_to_rgba(&frame, (w, h));
        let px = |x: u32, y: u32| {
            let o = ((y * w + x) * 4) as usize;
            [img[o], img[o + 1], img[o + 2]]
        };
        let near = |c: u8, v: i32| (c as i32 - 20..=c as i32 + 20).contains(&v);
        // Left half = volume over the sky: nothing drawn (pass 1 hides it all).
        assert!(
            near(0, px(16, 24)[0] as i32) && px(16, 24)[1] > 200 && px(16, 24)[2] < 20,
            "sky stays green, got {:?}",
            px(16, 24)
        );
        // Over the ground inside the volume: half-black red (pass 2 blends).
        assert!(
            near(128, px(40, 24)[0] as i32) && px(40, 24)[1] < 20,
            "shadow over ground, got {:?}",
            px(40, 24)
        );
        // Over the ground outside the volume: pure red.
        assert_eq!(px(56, 24), [255, 0, 0]);
    }
}
