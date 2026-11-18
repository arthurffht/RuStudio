//! Direct3D 11 viewport renderer. Draws the scene into a float (HDR, MSAA-resolved)
//! target, then a post pass tone-maps it into the texture egui shows as an
//! image, so the 3D view docks like any other panel. The sun's shadow map is
//! drawn first.

#[path = "env_probe_gpu.rs"]
mod env_probe_gpu;
#[path = "vf_render.rs"]
pub mod vf;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::time::Instant;

use bytemuck::{Pod, Zeroable};
use glam::{Mat4, Vec3};
use rbx_dom_weak::types::Ref;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use crate::d3d11::shaders;
use crate::math::Xform;

/// The scene is lit in linear light into this target...
const HDR_FORMAT: DXGI_FORMAT = DXGI_FORMAT_R16G16B16A16_FLOAT;
/// ...and the post pass writes display values (gamma 2.0) here.
const DISPLAY_FORMAT: DXGI_FORMAT = DXGI_FORMAT_R8G8B8A8_UNORM;
const DEPTH_FORMAT: DXGI_FORMAT = DXGI_FORMAT_D32_FLOAT;
/// EVSM moments of the local light shadow maps (Studio's warps of 5.54 keep
/// e^(2·5.54) under half float's 65504).
const LSHADOW_FORMAT: DXGI_FORMAT = DXGI_FORMAT_R16G16B16A16_FLOAT;

/// The local light shadow atlas (Studio's Future path: a cube map per
/// shadowed light, Studio measurements "Local light shadows").
pub struct LightShadows {
    side: u32,
    moments_rtv: ID3D11RenderTargetView,
    moments_srv: ID3D11ShaderResourceView,
    depth_dsv: ID3D11DepthStencilView,
    /// ShadowSlot per light (6 view-projections + position / 1 over Range).
    slots_buf: ID3D11Buffer,
    slots_srv: ID3D11ShaderResourceView,
    /// LFace per face, uploaded right before drawing each face (SM 5.0 CB).
    face_cb: ID3D11Buffer,
    /// What the atlas holds: static scene generation + the lights; redrawn when it changes.
    key: Option<u64>,
}

impl LightShadows {
    fn new(device: &ID3D11Device, side: u32) -> Self {
        let side = side.max(1);
        let moments_tex = crate::d3d11::create_tex2d(
            device,
            &crate::d3d11::Tex2dSpec {
                width: side,
                height: side,
                format: LSHADOW_FORMAT,
                bind_flags: (D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE).0 as u32,
                ..Default::default()
            },
            None,
        );
        let moments_rtv = crate::d3d11::create_rtv(device, &moments_tex, LSHADOW_FORMAT, 0, false);
        let moments_srv = crate::d3d11::create_srv_2d(device, &moments_tex, LSHADOW_FORMAT, false, 0, 1);
        let depth_tex = crate::d3d11::create_tex2d(
            device,
            &crate::d3d11::Tex2dSpec {
                width: side,
                height: side,
                format: DEPTH_FORMAT,
                bind_flags: D3D11_BIND_DEPTH_STENCIL.0 as u32,
                ..Default::default()
            },
            None,
        );
        let depth_dsv = crate::d3d11::create_dsv(device, &depth_tex, DEPTH_FORMAT, false, false);
        let n = crate::mech::LSHADOW_MAX_LIGHTS.max(1) as u32;
        let (slots_buf, slots_srv) = crate::d3d11::create_structured_srv(device, 6 * 64 + 16, n, true, None);
        let face_cb = crate::d3d11::create_cb(device, 96);
        Self {
            side,
            moments_rtv,
            moments_srv,
            depth_dsv,
            slots_buf,
            slots_srv,
            face_cb,
            key: None,
        }
    }

    /// The six face view-projections of a light: 93° frusta (Studio's
    /// 1/0.94896), near LSHADOW_NEAR, far its Range.
    fn faces(pos: Vec3, range: f32) -> [(Mat4, Vec3); 6] {
        let axes = [Vec3::X, -Vec3::X, Vec3::Y, -Vec3::Y, Vec3::Z, -Vec3::Z];
        let fov = 2.0 * crate::mech::LSHADOW_TAN_HALF_FOV.atan();
        let proj = Mat4::perspective_rh(fov, 1.0, crate::mech::LSHADOW_NEAR, range.max(crate::mech::LSHADOW_NEAR * 2.0));
        axes.map(|a| {
            let up = if a.y.abs() > 0.5 { Vec3::Z } else { Vec3::Y };
            (proj * Mat4::look_to_rh(pos, a, up), a)
        })
    }
}
/// DepthOfFieldEffect intensity from which Studio runs the reduced near chain
/// instead of the full-resolution cheap pass (DOF_CHEAP_BELOW).
const DOF_CHEAP_BELOW: f32 = 0.22;
struct SunShadowRes {
    side: u32,
    depth_dsv: ID3D11DepthStencilView,
    sun: crate::sun_shadow::SunShadows,
}

impl SunShadowRes {
    fn new(device: &ID3D11Device, side: u32) -> Self {
        let side = side.max(1);
        let shadow_tex = crate::d3d11::create_tex2d(
            device,
            &crate::d3d11::Tex2dSpec {
                width: side,
                height: side,
                format: DXGI_FORMAT_R32_TYPELESS,
                bind_flags: (D3D11_BIND_DEPTH_STENCIL | D3D11_BIND_SHADER_RESOURCE).0 as u32,
                ..Default::default()
            },
            None,
        );
        let depth_dsv = crate::d3d11::create_dsv(device, &shadow_tex, DEPTH_FORMAT, false, false);
        let depth_srv = crate::d3d11::create_srv_2d(device, &shadow_tex, DXGI_FORMAT_R32_FLOAT, false, 0, 1);
        let sun = crate::sun_shadow::SunShadows::new(device, &depth_srv, side);
        Self { side, depth_dsv, sun }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Globals {
    pub view_proj: [[f32; 4]; 4],
    pub inv_view_proj: [[f32; 4]; 4],
    pub cam_pos: [f32; 4],
    pub sun_dir: [f32; 4],
    pub sun_color: [f32; 4],
    pub ambient: [f32; 4],
    pub outdoor: [f32; 4],
    pub sky_top: [f32; 4],
    pub sky_horizon: [f32; 4],
    pub params: [f32; 4],
    pub fog: [f32; 4],
    pub fog_color: [f32; 4],
    pub cc: [f32; 4],
    pub cc_tint: [f32; 4],
    pub bloom: [f32; 4],
    /// The sun's shadow cascades (sun_shadow.rs): world to each tile's
    /// (u, v, depth), each sphere (centre, radius²), each tile in the atlas.
    pub sun_casc: [[[f32; 4]; 4]; 4],
    pub sun_sphere: [[f32; 4]; 4],
    pub sun_rect: [[f32; 4]; 4],
    pub sky_rot: [f32; 4],
    /// Skybox irradiance facing +X, −X, +Y, −Y, +Z, −Z, times EnvironmentDiffuseScale.
    pub env: [[f32; 4]; 6],
    /// The sky's brightness (and its light's) by time of day.
    pub sky_tint: [f32; 4],
    /// Light grid (lightgrid.rs): xyz its first voxel, w studs out along the normal (0 = none yet).
    pub lightgrid: [f32; 4],
    /// Atmosphere (round 26): rgb the haze band colour (linear), w the
    /// band strength at the horizon (0 = no Atmosphere).
    pub atmos: [f32; 4],
    /// x EnvironmentSpecularScale (0 = no sky reflection), y roughness (round 26).
    pub env_spec: [f32; 4],
    /// Parts blocking local lights (round 26, lightocc.rs): xyz the grid's
    /// first voxel, w studs per voxel (0 = no grid).
    pub lightocc: [f32; 4],
    /// Atmosphere haze toward +X, −X, +Y, −Y, +Z, −Z (AdvSkyFS irradiance × FogColor).
    pub atmos_cube: [[f32; 4]; 6],
    /// x fogOffset.x, y fogOffset.y, z Glare, w Offset.
    pub atmos_sky: [f32; 4],
    /// xyz the glare's exponents per channel (from Decay).
    pub atmos_glare: [f32; 4],
    /// Studio's indoor environment light (AmbientCube[6..11]): the
    /// irradiance of textures/sky/indoor512 × EnvironmentDiffuseScale,
    /// +X, −X, +Y, −Y, +Z, −Z (Studio measurements "Environment light and AO").
    pub env_indoor: [[f32; 4]; 6],
    /// SkyGradientTop / Bottom (CB0[30] / [31]): the time-of-day sky
    /// levels c0², c1²; w EnvironmentDiffuse / SpecularScale.
    pub sky_grad: [[f32; 4]; 2],
    /// Lamp1: the back light (faces turned from Lamp0), rgb.
    pub lamp1: [f32; 4],
    /// Screen-space AO: x, y half the projection diagonal, z near plane, w on.
    pub ssao: [f32; 4],
    pub dof: [f32; 4],
}

/// One local light as the shader reads it (see `Light` in `src/shaders/common.hlsli`).
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug, Default)]
pub struct LightRaw {
    pub pos_range: [f32; 4],
    pub color: [f32; 4],
    pub dir: [f32; 4],
    pub extra: [f32; 4],
    pub surface_u: [f32; 4],
    pub surface_v: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq)]
pub struct Vertex {
    pub pos: [f32; 3],
    pub normal: [f32; 3],
    pub uv: [f32; 2],
    pub projection: [f32; 4],
    /// Per-vertex colour (sRGB 0..1); alpha 0 = use the part's Color.
    pub color: [f32; 4],
}

/// Box-projected UVs for built-in shapes. Each face is seen from outside
/// with its image upright: the front face (−Z) has image-left at +X (the
/// part's right), matching the classic clothing-template layout.
fn box_uv(p: Vec3, n: Vec3) -> [f32; 2] {
    let a = n.abs();
    if a.x >= a.y && a.x >= a.z {
        if n.x > 0.0 {
            [0.5 - p.z, 0.5 - p.y]
        } else {
            [p.z + 0.5, 0.5 - p.y]
        }
    } else if a.y >= a.z {
        if n.y > 0.0 {
            [0.5 - p.x, 0.5 - p.z]
        } else {
            [0.5 - p.x, p.z + 0.5]
        }
    } else if n.z < 0.0 {
        [0.5 - p.x, 0.5 - p.y]
    } else {
        [p.x + 0.5, 0.5 - p.y]
    }
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct LineVertex {
    pub pos: [f32; 3],
    pub color: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct InstanceRaw {
    r0: [f32; 4],
    r1: [f32; 4],
    r2: [f32; 4],
    size: [f32; 4],
    color: [f32; 4],
    extra: [f32; 4],
    uvt: [f32; 4],
    /// SurfaceAppearance emission: EmissiveTint² × EmissiveStrength.
    emissive: [f32; 4],
    /// SurfaceAppearance.Color (rgb, clamped 0..1) and w 1 for a part drawn
    /// with a SurfaceAppearance (Studio's per-instance constant beside the
    /// instance colour: a script tinting it only rewrites instance data).
    tint: [f32; 4],
}

/// How a texture combines with the part (shader `extra.w`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TexMode {
    None,
    /// Over the part colour (clothing, SurfaceAppearance AlphaMode Overlay):
    /// transparent texels show Color.
    Over,
    /// A decal: the texture's own alpha, tinted by the colour.
    Decal,
    /// The texture alone, alpha ignored (MeshPart TextureID, SpecialMesh
    /// TextureId: Studio shows neither Color nor alpha through them).
    Opaque,
    /// SpecialMesh TextureId multiplied by its VertexColor.
    OpaqueTinted,
    /// SurfaceAppearance AlphaMode Transparency: blended by texel alpha
    /// (drawn with the see-through parts even at Transparency 0).
    Cutout,
    /// SurfaceAppearance AlphaMode TintMask: texel alpha says where Color
    /// tints the texture; the part stays opaque.
    TintMask,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MeshKind {
    Block = 0,
    Ball = 1,
    Cylinder = 2,
    Wedge = 3,
    CornerWedge = 4,
}

/// One part as the renderer sees it.
#[derive(Debug, Clone, Copy)]
pub struct PartDraw {
    pub xf: Xform,
    pub size: Vec3,
    /// Linear RGB.
    pub color: [f32; 3],
    pub alpha: f32,
    pub mesh: MeshKind,
    /// 0 plastic, 1 neon, 2 smooth, 3 glass.
    pub material: f32,
    pub placeholder: bool,
    pub hover: bool,
    /// Reflectance (0..1): the sky's reflection mixed into the colour.
    pub reflectance: f32,
    /// A loaded mesh (index from [`Renderer::add_mesh`]) drawn instead of `mesh`.
    pub custom_mesh: Option<u32>,
    /// A texture from [`Renderer::add_texture`].
    pub texture: Option<u32>,
    pub tex_mode: TexMode,
    /// UV scale (x, y) and offset (z, w), e.g. Texture tiling.
    pub uvt: [f32; 4],
    /// Emission (linear): the texture times its emissive mask times this.
    pub emissive: [f32; 3],
    /// The Material's darkening of textures (the part colour already has it).
    pub tex_albedo: f32,
    /// Legacy surfaces, 3 bits per face in Enum.NormalId order (Right, Top,
    /// Back, Left, Bottom, Front): 0 smooth, 1 studs, 2 weld / glue,
    /// 3 inlet, 4 universal (bands of Studio's studs.dds).
    pub surfaces: u32,
    /// Enum.Material value.
    pub material_id: u32,
    /// Material texture array layer (-1: none, draw flat).
    pub mat_layer: i32,
    /// Drawn with a SurfaceAppearance: its Color (sRGB, clamped 0..1), which
    /// multiplies the ColorMap texel before squaring (Studio's
    /// SurfaceAppearanceFS); None for every other part.
    pub surface_tint: Option<[f32; 3]>,
    /// BasePart.CastShadow: false keeps it out of the sun's shadow map.
    pub cast_shadow: bool,
}

impl PartDraw {
    pub const NO_UVT: [f32; 4] = [1.0, 1.0, 0.0, 0.0];
}

pub struct Frame<'a> {
    pub globals: Globals,
    /// Ordinary (not reverse-Z) view-projection, for culling.
    pub cull_view_proj: Mat4,
    pub fov_y: f32,
    /// Local lights (at most MAX_LIGHTS).
    pub lights: &'a [LightRaw],
    /// Draw the sun's shadow cascades (`sun_cascades`).
    pub shadows: bool,
    pub sun_cascades: [crate::sun_shadow::Cascade; crate::sun_shadow::CASCADES],
    /// Drawn on top of what's already there (hover highlight).
    pub overlay: &'a [PartDraw],
    pub lines: &'a [LineVertex],
    /// Draw the sky (an orthographic game camera has none: Studio leaves
    /// black where nothing is).
    pub sky: bool,
    /// Terrain water's waves, animation and clearness (water.rs).
    pub water: crate::water::WaterLook,
    /// Terrain.Clouds (clouds.rs), drawn with the sky.
    pub clouds: Option<crate::clouds::CloudsLook>,
    /// The scene environment probe's work this frame (env_probe.rs).
    pub probe: Option<crate::env_probe::ProbeFrame>,
    /// Defer bloom + DoF + post composite until `finalize_post` after world GUI / highlights.
    pub defer_post: bool,
}

/// Studs per side of the cells static parts are grouped into.
const CELL: f32 = crate::mech::RENDER_CELL;

/// Which pass a draw belongs to; None = blended part, sorted every frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Pass {
    Opaque,
    Decal,
}

fn pass_of(p: &PartDraw) -> Option<Pass> {
    if p.tex_mode == TexMode::Decal && p.texture.is_some() {
        // Texture batching changes coplanar decal layer order. Keep decals
        // with the sorted translucent draws so their source order survives
        // equal distances, and translucent hosts draw behind their decals.
        None
    } else if p.tex_mode == TexMode::Cutout && p.texture.is_some() {
        // SurfaceAppearance AlphaMode Transparency: blended by texel alpha
        // on an opaque part too, never cut out (Studio 0.747: alpha 0.5 shows
        // half, 0.31 faintly; checks/sa-color).
        None
    } else if p.alpha >= 0.999 && p.material_id != 1584 {
        Some(Pass::Opaque)
    } else {
        None
    }
}

fn transparent_order(a_kind: f32, a_distance: f32, b_kind: f32, b_distance: f32) -> std::cmp::Ordering {
    let a_glass = (a_kind as u32 & 7) == 3;
    let b_glass = (b_kind as u32 & 7) == 3;
    b_glass.cmp(&a_glass).then_with(|| b_distance.total_cmp(&a_distance))
}

#[cfg(test)]
mod gpu_tests {
    use super::*;

    #[test]
    fn primitive_parity_geometry() {
        let meshes = build_meshes();
        let points: Vec<_> = ball_outline_points().collect();
        for vertex in &meshes[MeshKind::Ball as usize] {
            assert!(points.iter().any(|p| p.distance(Vec3::from_array(vertex.pos)) < 1e-6));
        }
        for index in [MeshKind::Wedge as usize, MeshKind::CornerWedge as usize] {
            assert!(meshes[index].iter().any(|v| v.projection[0] == -99.0));
        }
        for index in [0usize, MeshKind::Ball as usize, MeshKind::Cylinder as usize] {
            assert!(meshes[index].iter().all(|v| v.projection[0] == 0.0));
        }
    }

    #[test]
    fn native_cell_distance_selection_counts() {
        // Actual quality21 native draws, camera (16,20,-20), 128 domains.
        for (lo,hi,near,allocated,expected) in [
            ([-4.,-4.,-4.],[68.,68.,68.],false,1664,1489),
            ([-4.,-68.,-4.],[68.,4.,68.],false,1664,1423),
            ([-4.,-4.,-4.],[68.,68.,68.],true,2048,1830),
            ([-4.,-68.,-4.],[68.,4.,68.],true,2048,1749),
        ] {
            let range = GrassRange {start:0,ordinals:(0..allocated).collect(),allocated,near,lo:Vec3::from_array(lo),hi:Vec3::from_array(hi)};
            assert_eq!(range.count(Vec3::new(16.,20.,-20.))/6,expected);
        }
    }

    #[test]
    fn native_sun_grass_near_mid_selection_counts() {
        // Actual sun writer + receiver GPU ranges, native frames 377 / 8833.
        // Both X regions have identical distance at camera X=2 (margin 4).
        for x in [-64.0, 0.0] {
            for (z, near, allocated, camera_z, vertices) in [
                (-64.0, true, 8192, -34.0, 24570),
                (-64.0, false, 6656, -34.0, 19968),
                (0.0, true, 8192, -34.0, 19962),
                (0.0, false, 6656, -34.0, 16224),
                (-64.0, false, 6656, -150.0, 11163),
                (0.0, false, 6656, -150.0, 6594),
            ] {
                let range = GrassRange { start: 0, ordinals: (0..allocated).collect(), allocated, near,
                    lo: Vec3::new(x-4.0, -4.0, z-4.0),
                    hi: Vec3::new(x+68.0, 68.0, z+68.0) };
                // Rust uploads both sides; native submits one three-vertex triangle.
                assert_eq!(range.count(Vec3::new(2.0,28.0,camera_z)), vertices*2);
            }
        }
    }

    #[test]
    #[ignore = "writes integrated full-domain renderer selection evidence"]
    fn export_integrated_grass_selection() {
        let Ok(rs) = crate::d3d11::device::create_device() else {
            return;
        };
        let mut renderer = Renderer::new(&rs);
        let mut vox = crate::terrain::Voxels::empty();
        // Same FillBlock: x,z in [0,32), y in [-16,0), full Grass voxels.
        for x in 0..8 { for y in -4..0 { for z in 0..8 { vox.set([x,y,z],2,1.0); } } }
        let mut rows = Vec::new();
        for length in [0.7,0.1] {
            renderer.clear_terrain();
            for y in -1..=0 { for x in -1..=0 { for z in -1..=0 {
                let key = [x,y,z];
                let geometry = crate::terrain::grass_geometry_density(&vox,key,length,1.0);
                let positions: Vec<_> = geometry.cells.iter().map(|cell| cell.batch.blades.iter().map(|&(_,first)| geometry.vertices[first..first+3].iter().map(|v|v.pos).collect::<Vec<_>>()).collect::<Vec<_>>()).collect();
                renderer.set_terrain_grass_geometry(&rs.device,key,geometry);
                if let Some(chunk) = renderer.terrain_grass.get(&key) {
                    for (range, blades) in chunk.grass_ranges.iter().zip(positions) {
                        let count = range.count(Vec3::new(16.,20.,-20.)) as usize / 6;
                        rows.push(serde_json::json!({"length":length,"key":key,"near":range.near,"lo":range.lo.to_array(),"hi":range.hi.to_array(),"allocated":range.allocated,"ordinals":range.ordinals,"selected_ordinals":range.ordinals[..count],"selected_triangles":blades[..count],"resident_triangles":blades}));
                    }
                }
            } } }
        }
        std::fs::write("checks/grass-full-re/integrated-selection/renderer-selection.json",serde_json::to_vec(&rows).unwrap()).unwrap();
    }

    /// Every pipeline/shader/state of the 3D view, built on this machine's GPU
    /// via native Direct3D 11 (`d3d11::create_device`), including 1x and MSAA
    /// target switches. Skipped with a note where there is no GPU.
    #[test]
    fn renderer_pipelines_build_on_this_gpu() {
        let Ok(rs) = crate::d3d11::device::create_device() else {
            eprintln!("no D3D11 GPU adapter here: skipped");
            return;
        };
        let mut r = Renderer::new(&rs);
        let q_low = crate::quality::QualityProfile::from_level(1, &rs.caps, false);
        let q_high = crate::quality::QualityProfile::from_level(21, &rs.caps, false);
        r.set_quality(q_low);
        r.ensure_target(&rs, [64, 64]);
        r.set_quality(q_high);
        r.ensure_target(&rs, [64, 64]);
        drop(r);
        eprintln!("renderer built on {} (D3D11)", rs.caps.adapter_name);
    }
}

#[cfg(test)]
mod see_bounds_tests {
    use super::SeeBounds;
    use glam::{Mat4, Vec3};

    /// A camera at the origin looking down -Z (90° vertical field, square view).
    fn bounds() -> SeeBounds {
        let vp = Mat4::perspective_rh(std::f32::consts::FRAC_PI_2, 1.0, 0.1, 1000.0);
        SeeBounds::new(vp, [320, 320])
    }

    fn covers(rects: &[[u32; 4]], x: u32, y: u32) -> bool {
        rects.iter().any(|r| x >= r[0] && x < r[0] + r[2] && y >= r[1] && y < r[1] + r[3])
    }

    #[test]
    fn nothing_marks_nothing() {
        assert!(bounds().rects().is_empty());
    }

    #[test]
    fn a_small_part_marks_only_the_tiles_under_it() {
        let mut b = bounds();
        b.add_sphere(Vec3::new(0.0, 0.0, -10.0), 1.0);
        let rects = b.rects();
        // centre covered, merged into one rectangle, far corners not
        assert_eq!(rects.len(), 1, "{rects:?}");
        assert!(covers(&rects, 160, 160));
        assert!(!covers(&rects, 5, 5) && !covers(&rects, 315, 315));
        let area: u32 = rects.iter().map(|r| r[2] * r[3]).sum();
        assert!(area < 320 * 320 / 8, "{rects:?}");
    }

    #[test]
    fn a_part_reaching_behind_the_camera_marks_everything() {
        let mut b = bounds();
        b.add_sphere(Vec3::new(0.0, 0.0, -0.5), 2.0);
        assert_eq!(b.rects(), vec![[0, 0, 320, 320]]);
    }

    #[test]
    fn a_part_outside_the_view_marks_nothing() {
        let mut b = bounds();
        b.add_sphere(Vec3::new(500.0, 0.0, -10.0), 1.0);
        assert!(b.rects().is_empty());
    }

    #[test]
    fn two_parts_mark_two_regions() {
        let mut b = bounds();
        b.add_sphere(Vec3::new(-6.0, 6.0, -10.0), 0.5);
        b.add_sphere(Vec3::new(6.0, -6.0, -10.0), 0.5);
        let rects = b.rects();
        assert!(covers(&rects, 64, 64) && covers(&rects, 256, 256), "{rects:?}");
        assert!(!covers(&rects, 160, 160) && !covers(&rects, 256, 64), "{rects:?}");
    }
}

#[cfg(test)]
mod transparent_order_tests {
    use super::transparent_order;
    use std::cmp::Ordering;

    #[test]
    fn glass_precedes_other_transparents_regardless_of_distance() {
        assert_eq!(transparent_order(3.0, 1.0, 0.0, 100.0), Ordering::Less);
        assert_eq!(transparent_order(6.0, 100.0, 3.0, 1.0), Ordering::Greater);
    }

    #[test]
    fn each_group_remains_back_to_front() {
        for kind in [0.0, 3.0, 6.0] {
            assert_eq!(transparent_order(kind, 100.0, kind, 1.0), Ordering::Less);
            assert_eq!(transparent_order(kind, 1.0, kind, 1.0), Ordering::Equal);
        }
    }
}

fn shows(p: &PartDraw) -> bool {
    p.alpha > 0.001
}

fn radius_of(p: &PartDraw) -> f32 {
    p.size.length() * 0.5
}

/// The camera is inside the part's box.
fn camera_inside(p: &PartDraw, cam: Vec3) -> bool {
    let local = p.xf.rot.transpose() * (cam - p.xf.pos);
    local.abs().cmple(p.size * 0.5).all()
}

#[derive(Debug, Clone, PartialEq)]
enum ItemState {
    /// New: drawn per frame until the next static rebuild.
    Pending,
    /// In the static buffer: slot per draw (u32::MAX = not drawn).
    Static(Vec<u32>),
    /// Moves (or is see-through): drawn per frame.
    Dynamic,
}

/// One part's draws (the part itself plus its decals).
struct Item {
    draws: Vec<PartDraw>,
    bias: f32,
    state: ItemState,
    /// Last time the part changed place or look (Dynamic parts quiet for
    /// DYNAMIC_SETTLE_S go back to the static batches).
    moved: Instant,
}

/// Same place within float noise: a CFrame write of the same value comes
/// back through the scene's transform maths with its low bits changed.
fn near_xf(a: &crate::math::Xform, b: &crate::math::Xform) -> bool {
    a.pos.abs_diff_eq(b.pos, 1e-4) && a.rot.abs_diff_eq(b.rot, 1e-5)
}

struct Batch {
    pass: Pass,
    key: (usize, usize),
    lo: Vec3,
    hi: Vec3,
    start: u32,
    count: u32,
}

/// Anchored, opaque parts: uploaded once, sorted by (pass, mesh/texture,
/// cell, size) so each cell is one instanced draw that can be culled and
/// trimmed of its smallest parts with distance.
#[derive(Default)]
struct StaticLayout {
    raw: Vec<InstanceRaw>,
    /// Bounding radius per instance (descending within a batch).
    radii: Vec<f32>,
    batches: Vec<Batch>,
    hidden: usize,
    /// Slots changed in place, uploaded next frame.
    writes: Vec<u32>,
}

/// Counters for the last frame (shown in the bench report).
#[derive(Default, Debug, Clone, Copy)]
pub struct RenderStats {
    pub items: usize,
    pub static_instances: usize,
    pub batches: usize,
    pub per_frame_items: usize,
    pub drawn_instances: usize,
    pub draw_calls: usize,
    pub culled_batches: usize,
    pub lod_skipped: usize,
    pub rebuilds: u64,
}

/// A small, stable depth nudge per part so exactly coplanar faces of two
/// parts always resolve the same way instead of flickering. From the part's
/// own size and placement, not its Ref (Refs are random per run: which of
/// two coplanar floors showed changed from run to run).
fn bias_of(draws: &[PartDraw]) -> f32 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    if let Some(d) = draws.first() {
        for v in d.size.to_array().into_iter().chain(d.xf.pos.to_array()) {
            v.to_bits().hash(&mut h);
        }
    }
    (h.finish() % 16) as f32 * crate::mech::DEPTH_BIAS_STEP
}

fn spread(v: u64) -> u64 {
    let mut x = v & 0x1f_ffff;
    x = (x | x << 32) & 0x1f_0000_0000_ffff;
    x = (x | x << 16) & 0x1f_0000_ff00_00ff;
    x = (x | x << 8) & 0x100f_00f0_0f00_f00f;
    x = (x | x << 4) & 0x10c3_0c30_c30c_30c3;
    (x | x << 2) & 0x1249_2492_4924_9249
}

/// Z-order of a cell, so neighbouring cells sit next to each other.
fn morton(c: [i32; 3]) -> u64 {
    let o = |v: i32| (v + (1 << 20)) as u64;
    spread(o(c[0])) | spread(o(c[1])) << 1 | spread(o(c[2])) << 2
}

/// World-space bounding box of a draw.
fn aabb_of(p: &PartDraw) -> (Vec3, Vec3) {
    let r = p.xf.rot;
    let h = p.size * 0.5;
    let ext = r.x_axis.abs() * h.x + r.y_axis.abs() * h.y + r.z_axis.abs() * h.z;
    (p.xf.pos - ext, p.xf.pos + ext)
}

/// Frustum planes (normal, d) from a view-projection matrix (depth 0..1).
pub fn frustum(m: Mat4) -> [glam::Vec4; 5] {
    let (r0, r1, r2, r3) = (m.row(0), m.row(1), m.row(2), m.row(3));
    let n = |p: glam::Vec4| p / p.truncate().length().max(1e-6);
    [n(r3 + r0), n(r3 - r0), n(r3 + r1), n(r3 - r1), n(r2)]
}

fn box_visible(planes: &[glam::Vec4; 5], lo: Vec3, hi: Vec3) -> bool {
    planes.iter().all(|p| {
        let v = Vec3::new(
            if p.x > 0.0 { hi.x } else { lo.x },
            if p.y > 0.0 { hi.y } else { lo.y },
            if p.z > 0.0 { hi.z } else { lo.z },
        );
        p.truncate().dot(v) + p.w >= 0.0
    })
}

pub fn sphere_visible(planes: &[glam::Vec4; 5], c: Vec3, r: f32) -> bool {
    planes.iter().all(|p| p.truncate().dot(c) + p.w >= -r)
}

/// The pixels a frame's decals and see-through parts can cover: the
/// see-through encode and decode passes (SeeThrough) only run there, since
/// outside every layer they change nothing (encode writes alpha 0, decode
/// keeps alpha-0 samples as they are). Kept as a TILES x TILES mask of the
/// view: each item marks the tiles under its screen rectangle. Conservative:
/// an item's bounding sphere (its box's, as frustum culling uses), plus a
/// margin; a box behind or across the camera plane, or a skinned mesh,
/// marks the whole view.
/// RUSTUDIO_SEE_FULL=1: always the whole view.
struct SeeBounds {
    view_proj: Mat4,
    size: [u32; 2],
    rows: [u32; SeeBounds::TILES],
}

impl SeeBounds {
    const TILES: usize = 32;
    /// Pixels added on each side (MSAA samples and rasterisation at the edge).
    const MARGIN: f32 = 4.0;

    fn new(view_proj: Mat4, size: [u32; 2]) -> Self {
        static FULL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let mut b = SeeBounds { view_proj, size, rows: [0; Self::TILES] };
        if *FULL.get_or_init(|| std::env::var_os("RUSTUDIO_SEE_FULL").is_some()) {
            b.set_full();
        }
        b
    }

    fn set_full(&mut self) {
        self.rows = [u32::MAX; Self::TILES];
    }

    /// The tile column / row holding pixel coordinate `v` of `extent`.
    fn tile(v: f32, extent: u32) -> usize {
        ((v / extent as f32 * Self::TILES as f32).floor().max(0.0) as usize).min(Self::TILES - 1)
    }

    fn add_box(&mut self, lo: Vec3, hi: Vec3) {
        if self.rows[0] == u32::MAX && self.rows.iter().all(|r| *r == u32::MAX) {
            return;
        }
        if !(lo.is_finite() && hi.is_finite()) {
            self.set_full();
            return;
        }
        let (w, h) = (self.size[0] as f32, self.size[1] as f32);
        let (mut a, mut b) = (glam::Vec2::splat(f32::MAX), glam::Vec2::splat(f32::MIN));
        for i in 0..8 {
            let c = Vec3::new(
                if i & 1 == 0 { lo.x } else { hi.x },
                if i & 2 == 0 { lo.y } else { hi.y },
                if i & 4 == 0 { lo.z } else { hi.z },
            );
            let clip = self.view_proj * c.extend(1.0);
            if !(clip.w > 1e-3) {
                self.set_full();
                return;
            }
            let p = glam::Vec2::new((clip.x / clip.w * 0.5 + 0.5) * w, (0.5 - clip.y / clip.w * 0.5) * h);
            a = a.min(p);
            b = b.max(p);
        }
        a -= glam::Vec2::splat(Self::MARGIN);
        b += glam::Vec2::splat(Self::MARGIN);
        if b.x < 0.0 || b.y < 0.0 || a.x >= w || a.y >= h {
            return;
        }
        let (c0, c1) = (Self::tile(a.x, self.size[0]), Self::tile(b.x, self.size[0]));
        let (r0, r1) = (Self::tile(a.y, self.size[1]), Self::tile(b.y, self.size[1]));
        let mask = ((1u64 << (c1 + 1)) - (1u64 << c0)) as u32;
        for row in &mut self.rows[r0..=r1] {
            *row |= mask;
        }
    }

    fn add_sphere(&mut self, c: Vec3, r: f32) {
        let r = Vec3::splat(r);
        self.add_box(c - r, c + r);
    }

    /// The marked tiles as scissor rectangles (x, y, width, height): runs of
    /// tiles along a row, merged with the same run on the rows below.
    fn rects(&self) -> Vec<[u32; 4]> {
        let edge = |i: usize, extent: u32| (i as u64 * extent as u64 / Self::TILES as u64) as u32;
        let mut out: Vec<([u32; 4], usize)> = Vec::new();
        for (r, &bits) in self.rows.iter().enumerate() {
            let mut c = 0;
            while c < Self::TILES {
                if bits & (1 << c) == 0 {
                    c += 1;
                    continue;
                }
                let c0 = c;
                while c < Self::TILES && bits & (1 << c) != 0 {
                    c += 1;
                }
                let (x0, x1) = (edge(c0, self.size[0]), edge(c, self.size[0]));
                let (y0, y1) = (edge(r, self.size[1]), edge(r + 1, self.size[1]));
                match out.iter_mut().find(|(o, last)| *last + 1 == r && o[0] == x0 && o[2] == x1 - x0) {
                    Some((o, last)) => {
                        o[3] = y1 - o[1];
                        *last = r;
                    }
                    None => out.push(([x0, y0, x1 - x0, y1 - y0], r)),
                }
            }
        }
        out.into_iter().map(|(o, _)| o).filter(|o| o[2] > 0 && o[3] > 0).collect()
    }
}

fn box_distance(p: Vec3, lo: Vec3, hi: Vec3) -> f32 {
    (p.clamp(lo, hi) - p).length()
}

const HIDDEN: InstanceRaw = InstanceRaw {
    r0: [0.0; 4],
    r1: [0.0; 4],
    r2: [0.0; 4],
    size: [0.0; 4],
    color: [0.0; 4],
    extra: [0.0; 4],
    uvt: [0.0; 4],
    emissive: [0.0; 4],
    tint: [0.0; 4],
};

/// Terrain palette entries: 23 SmoothGrid materials × top / side / bottom.
pub const TERRAIN_PALETTE: usize = 69;

/// One terrain chunk's triangles (round 21) and their bounds.
struct GrassRange {
    start: u32,
    ordinals: Vec<usize>,
    allocated: usize,
    near: bool,
    lo: Vec3,
    hi: Vec3,
}
impl GrassRange {
    fn count(&self, camera: Vec3) -> u32 {
        let distance = camera.distance(camera.clamp(self.lo, self.hi));
        let cap = crate::mech::GRASS_RADIUS;
        let factor = (2.0 / (1.0 + distance.min(cap) / cap) - 1.0).clamp(0.0, 1.0);
        let group = if self.near {3} else {1};
        let limit = ((self.allocated / group) as f32 * factor).trunc() as usize * group;
        (self.ordinals.partition_point(|&o| o < limit) * 6) as u32
    }
}
struct TerrainChunk {
    grass_ranges: Vec<GrassRange>,
    diagnostic_positions: Vec<[f32; 3]>,
    vbuf: ID3D11Buffer,
    count: u32,
    lo: Vec3,
    hi: Vec3,
}

/// Layers with their own Studio material constants in `MaterialArrays::params`.
const MAT_PARAM_LAYERS: usize = 256;

/// Two RGBA8 texture arrays of material maps (layer = pack), mipmapped, and
/// the part shader's per-layer material constants (shader `mat_params`):
/// 5 vec4 per layer: detail/tiling, noise/type, far tiling/emissive R,
/// cutoffs/emissive G, cutoff scales/emissive B; then Studio's 32²
/// noise and 128² Wang index, packed as exact integers in f32 lanes.
struct MaterialArrays {
    albedo: ID3D11Texture2D,
    surface: ID3D11Texture2D,
    emission: ID3D11Texture2D,
    albedo_srv: ID3D11ShaderResourceView,
    surface_srv: ID3D11ShaderResourceView,
    emission_srv: ID3D11ShaderResourceView,
    size: u32,
    capacity: u32,
    layers: std::collections::HashMap<u64, u32>,
    cpu: Vec<std::sync::Arc<crate::matpack::Maps>>,
    params: Vec<[f32; 4]>,
    params_cb: ID3D11Buffer,
}

impl MaterialArrays {
    /// The default constants: no detail map, textureTiling 1, tiled; noise.
    fn default_params() -> Vec<[f32; 4]> {
        let mut p = Vec::with_capacity(MAT_PARAM_LAYERS * 5 + 256 + 1024);
        for _ in 0..MAT_PARAM_LAYERS {
            p.push([-1.0, 0.0, 0.0, 1.0]);
            p.push([0.0, 0.0, 4.0, 1.0]);
            p.push([1.0, 1.0, 1.0, 0.0]);
            p.push([0.0; 4]);
            p.push([1.0, 1.0, 1.0, 0.0]);
        }
        p.extend(crate::matpack::detile_noise().chunks(4).map(|c| [c[0], c[1], c[2], c[3]]));
        p.extend(crate::matpack::wang_index());
        p
    }

    fn new(device: &ID3D11Device, ctx: &ID3D11DeviceContext, size: u32, capacity: u32) -> Self {
        Self::with_params(device, ctx, size, capacity, Self::default_params())
    }

    fn with_params(
        device: &ID3D11Device,
        ctx: &ID3D11DeviceContext,
        size: u32,
        capacity: u32,
        params: Vec<[f32; 4]>,
    ) -> Self {
        let mips = mip_levels(size, size);
        let make = |format: DXGI_FORMAT| {
            crate::d3d11::create_tex2d(
                device,
                &crate::d3d11::Tex2dSpec {
                    width: size,
                    height: size,
                    mips,
                    array_size: capacity,
                    format,
                    bind_flags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                    ..Default::default()
                },
                None,
            )
        };
        let view = |t: &ID3D11Texture2D, format: DXGI_FORMAT| {
            crate::d3d11::create_srv_2d_array(device, t, format, mips, 0, capacity)
        };
        let albedo = make(DXGI_FORMAT_R8G8B8A8_UNORM);
        let surface = make(DXGI_FORMAT_R8G8B8A8_UNORM);
        let emission = make(DXGI_FORMAT_R8_UNORM);
        let params_cb = crate::d3d11::create_cb(device, std::mem::size_of_val(params.as_slice()));
        crate::d3d11::update_dynamic(ctx, &params_cb, bytemuck::cast_slice(&params));
        MaterialArrays {
            albedo_srv: view(&albedo, DXGI_FORMAT_R8G8B8A8_UNORM),
            surface_srv: view(&surface, DXGI_FORMAT_R8G8B8A8_UNORM),
            emission_srv: view(&emission, DXGI_FORMAT_R8_UNORM),
            albedo,
            surface,
            emission,
            size,
            capacity,
            layers: Default::default(),
            cpu: Vec::new(),
            params,
            params_cb,
        }
    }

    /// Copies `from`'s first `layers` layers (every mip) into these arrays.
    fn copy_layers(&self, ctx: &ID3D11DeviceContext, from: &MaterialArrays, layers: u32) {
        if layers == 0 || from.size != self.size {
            return;
        }
        let mips = mip_levels(self.size, self.size);
        for (src, dst) in [(&from.albedo, &self.albedo), (&from.surface, &self.surface), (&from.emission, &self.emission)] {
            for layer in 0..layers {
                for mip in 0..mips {
                    let sub = mip + layer * mips;
                    unsafe { ctx.CopySubresourceRegion(dst, sub, 0, 0, 0, src, sub, None) };
                }
            }
        }
    }

    fn upload(&self, ctx: &ID3D11DeviceContext, layer: u32, maps: &crate::matpack::Pixels) {
        let mips = mip_levels(self.size, self.size);
        for (tex, data, channels) in [
            (&self.albedo, &maps.albedo, 4u32),
            (&self.surface, &maps.surface, 4u32),
            (&self.emission, &maps.emissive, 1u32),
        ] {
            for (mip, (at, size)) in crate::matpack::mip_levels().into_iter().enumerate() {
                if (mip as u32) >= mips {
                    break;
                }
                let at = at / (4 / channels) as usize;
                let row_pitch = size * channels;
                let bytes = (row_pitch * size) as usize;
                let Some(level) = data.get(at..at + bytes) else {
                    break;
                };
                let sub = mip as u32 + layer * mips;
                unsafe {
                    ctx.UpdateSubresource(tex, sub, None, level.as_ptr().cast(), row_pitch, bytes as u32);
                }
            }
        }
    }
}

struct Mesh {
    vbuf: ID3D11Buffer,
    count: u32,
    /// Back faces culled (Some(true)), as Studio draws a MeshPart with
    /// DoubleSided off (an inverted-hull outline shell then shows only as its
    /// rim); Some(false) two-sided by choice (DoubleSided); None two-sided
    /// because the winding isn't known (CSG).
    cull: Option<bool>,
    /// Made for the skinning pass (DEFAULT usage for GPU skinning CopyResource).
    storage: bool,
}

#[derive(Clone, Copy)]
enum SeePipeKind {
    Transparent,
    TransparentCulled,
    Glass,
    GlassCulled,
    Overlay,
}

/// Texture SRVs held for one uploaded part/material texture slot (`t15..t17`).
struct PartTex {
    albedo_srv: ID3D11ShaderResourceView,
    emissive_srv: ID3D11ShaderResourceView,
    surface_srv: ID3D11ShaderResourceView,
}

struct ShaderPair {
    x1: ID3D11PixelShader,
    msaa: ID3D11PixelShader,
}

impl ShaderPair {
    fn get(&self, msaa: bool) -> &ID3D11PixelShader {
        if msaa { &self.msaa } else { &self.x1 }
    }
}

/// Studio blends see-through surfaces in its scene buffer's encoding,
/// e = sqrt(clamp(L x 0.25 x exposure)) (scene.hlsl see_enc), not in linear
/// light. Frames with decals or see-through parts copy the MSAA scene into
/// that encoding (per sample), draw them there, and decode the pixels they
/// covered back (the rest keep their linear value, highlights above the
/// encoding's ceiling too). The copy and the decode run only over the
/// screen tiles those layers can cover (SeeBounds), not the whole view.
/// RUSTUDIO_LINEAR_BLEND=1 blends in linear light as before (for comparisons).
struct SeeThrough {
    encode: ShaderPair,
    decode: ShaderPair,
    linear: bool,
}

impl SeeThrough {
    fn new(device: &ID3D11Device) -> Self {
        SeeThrough {
            encode: ShaderPair {
                x1: crate::d3d11::create_ps(device, shaders::PS_SEE_ENCODE_1X),
                msaa: crate::d3d11::create_ps(device, shaders::PS_SEE_ENCODE_MSAA),
            },
            decode: ShaderPair {
                x1: crate::d3d11::create_ps(device, shaders::PS_SEE_DECODE_1X),
                msaa: crate::d3d11::create_ps(device, shaders::PS_SEE_DECODE_MSAA),
            },
            linear: std::env::var_os("RUSTUDIO_LINEAR_BLEND").is_some(),
        }
    }
}

struct BloomTargets {
    bright_capture: Option<ID3D11Texture2D>,
    sizes: [[u32; 2]; 5],
    textures: Vec<ID3D11Texture2D>,
    rtvs: Vec<ID3D11RenderTargetView>,
    srvs: Vec<ID3D11ShaderResourceView>,
}

struct DofTargets {
    rtvs: [ID3D11RenderTargetView; 2],
    srvs: [ID3D11ShaderResourceView; 2],
}

struct NearTargets {
    sizes: [[u32; 2]; 11],
    rtvs: Vec<ID3D11RenderTargetView>,
    srvs: Vec<ID3D11ShaderResourceView>,
}

struct SsaoTargets {
    depth_rtv: ID3D11RenderTargetView,
    depth_srv: ID3D11ShaderResourceView,
    rtvs: [ID3D11RenderTargetView; 2],
    srvs: [ID3D11ShaderResourceView; 2],
}

struct Target {
    size: [u32; 2],
    samples: u32,
    half_size: [u32; 2],
    msaa_tex: ID3D11Texture2D,
    msaa_rtv: ID3D11RenderTargetView,
    msaa_srv: ID3D11ShaderResourceView,
    /// The scene in the see-through encoding (SeeThrough).
    see_rtv: ID3D11RenderTargetView,
    see_srv: ID3D11ShaderResourceView,
    depth_dsv: ID3D11DepthStencilView,
    depth_dsv_ro: ID3D11DepthStencilView,
    depth_srv: ID3D11ShaderResourceView,
    /// The resolved scene in linear light (same resource as `msaa_tex` when `samples == 1`).
    hdr_tex: ID3D11Texture2D,
    hdr_rtv: ID3D11RenderTargetView,
    hdr_srv: ID3D11ShaderResourceView,
    /// Optional targets allocated lazily only when the active passes need them.
    scene_copy: Option<(ID3D11Texture2D, ID3D11ShaderResourceView)>,
    water_depth_dsv: Option<ID3D11DepthStencilView>,
    glass_depth: Option<(ID3D11RenderTargetView, ID3D11ShaderResourceView)>,
    dof: Option<DofTargets>,
    near: Option<NearTargets>,
    bloom: Option<BloomTargets>,
    ssao: Option<SsaoTargets>,
    /// The tone-mapped image egui shows.
    display_tex: ID3D11Texture2D,
    display_rtv: ID3D11RenderTargetView,
    #[allow(dead_code)]
    display_srv: ID3D11ShaderResourceView,
}

impl Target {
    fn mk_rt(
        device: &ID3D11Device,
        w: u32,
        h: u32,
        format: DXGI_FORMAT,
        s: u32,
    ) -> (ID3D11Texture2D, ID3D11RenderTargetView, ID3D11ShaderResourceView) {
        let tex = crate::d3d11::create_tex2d(
            device,
            &crate::d3d11::Tex2dSpec {
                width: w,
                height: h,
                mips: 1,
                array_size: 1,
                format,
                samples: s,
                bind_flags: (D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE).0 as u32,
                misc_flags: 0,
            },
            None,
        );
        let rtv = crate::d3d11::create_rtv(device, &tex, format, 0, s > 1);
        let srv = crate::d3d11::create_srv_2d(device, &tex, format, s > 1, 0, 1);
        (tex, rtv, srv)
    }

    fn ensure_scene_copy(&mut self, device: &ID3D11Device) -> &(ID3D11Texture2D, ID3D11ShaderResourceView) {
        self.scene_copy.get_or_insert_with(|| {
            let (tex, _, srv) = Self::mk_rt(device, self.size[0], self.size[1], HDR_FORMAT, 1);
            (tex, srv)
        })
    }

    fn ensure_water_depth(&mut self, device: &ID3D11Device) -> &ID3D11DepthStencilView {
        self.water_depth_dsv.get_or_insert_with(|| {
            let water_depth_tex = crate::d3d11::create_tex2d(
                device,
                &crate::d3d11::Tex2dSpec {
                    width: self.size[0],
                    height: self.size[1],
                    format: DEPTH_FORMAT,
                    samples: self.samples,
                    bind_flags: D3D11_BIND_DEPTH_STENCIL.0 as u32,
                    ..Default::default()
                },
                None,
            );
            crate::d3d11::create_dsv(device, &water_depth_tex, DEPTH_FORMAT, self.samples > 1, false)
        })
    }

    fn ensure_glass_depth(&mut self, device: &ID3D11Device) -> &(ID3D11RenderTargetView, ID3D11ShaderResourceView) {
        self.glass_depth.get_or_insert_with(|| {
            let (_, rtv, srv) = Self::mk_rt(device, self.size[0], self.size[1], DXGI_FORMAT_R32_FLOAT, 1);
            (rtv, srv)
        })
    }

    fn ensure_bloom(&mut self, device: &ID3D11Device) -> &BloomTargets {
        self.bloom.get_or_insert_with(|| {
            let mut sizes = [[1u32; 2]; 5];
            let mut textures = Vec::with_capacity(5);
            let mut rtvs = Vec::with_capacity(5);
            let mut srvs = Vec::with_capacity(5);
            for level in 1..=5usize {
                let bw = (self.size[0] >> level).max(1);
                let bh = (self.size[1] >> level).max(1);
                sizes[level - 1] = [bw, bh];
                let (tex, rtv, srv) = Self::mk_rt(device, bw, bh, HDR_FORMAT, 1);
                textures.push(tex);
                rtvs.push(rtv);
                srvs.push(srv);
            }
            let bright_capture = std::env::var_os("RUSTUDIO_DUMP_HDR").map(|_| {
                crate::d3d11::create_tex2d(
                    device,
                    &crate::d3d11::Tex2dSpec {
                        width: sizes[0][0],
                        height: sizes[0][1],
                        format: HDR_FORMAT,
                        bind_flags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                        ..Default::default()
                    },
                    None,
                )
            });
            BloomTargets {
                bright_capture,
                sizes,
                textures,
                rtvs,
                srvs,
            }
        })
    }

    fn ensure_dof(&mut self, device: &ID3D11Device) -> &DofTargets {
        self.dof.get_or_insert_with(|| {
            let (_, dof_rtv0, dof_srv0) = Self::mk_rt(device, self.size[0], self.size[1], HDR_FORMAT, 1);
            let (_, dof_rtv1, dof_srv1) = Self::mk_rt(device, self.size[0], self.size[1], HDR_FORMAT, 1);
            DofTargets {
                rtvs: [dof_rtv0, dof_rtv1],
                srvs: [dof_srv0, dof_srv1],
            }
        })
    }

    fn ensure_near(&mut self, device: &ID3D11Device) -> &NearTargets {
        self.near.get_or_insert_with(|| {
            let mut sizes = [[1u32; 2]; 11];
            let mut rtvs = Vec::with_capacity(11);
            let mut srvs = Vec::with_capacity(11);
            for i in 0..11usize {
                let nw = (self.size[0] / 4).max(1);
                let nh = if i == 0 { self.size[1] } else { (self.size[1] / 4).max(1) };
                sizes[i] = [nw, nh];
                let (_, rtv, srv) = Self::mk_rt(device, nw, nh, HDR_FORMAT, 1);
                rtvs.push(rtv);
                srvs.push(srv);
            }
            NearTargets { sizes, rtvs, srvs }
        })
    }

    fn ensure_ssao(&mut self, device: &ID3D11Device) -> &SsaoTargets {
        self.ssao.get_or_insert_with(|| {
            let (_, depth_rtv, depth_srv) =
                Self::mk_rt(device, self.half_size[0], self.half_size[1], DXGI_FORMAT_R32_FLOAT, 1);
            let (_, rtv0, srv0) =
                Self::mk_rt(device, self.half_size[0], self.half_size[1], DXGI_FORMAT_R32G32_FLOAT, 1);
            let (_, rtv1, srv1) =
                Self::mk_rt(device, self.half_size[0], self.half_size[1], DXGI_FORMAT_R32G32_FLOAT, 1);
            SsaoTargets {
                depth_rtv,
                depth_srv,
                rtvs: [rtv0, rtv1],
                srvs: [srv0, srv1],
            }
        })
    }
}

pub struct Renderer {
    quality: crate::quality::QualityProfile,
    can_msaa_4x: bool,
    last_cascades: Option<usize>,
    probe_samples: u32,
    /// Skinned parts deformed on the GPU (skin_gpu.rs).
    skin: crate::skin_gpu::SkinGpu,
    /// Highlight instances over the finished image (highlight.rs), paired with its MSAA sample count.
    highlight: Option<(u32, crate::highlight::HighlightPass)>,
    /// The last frame's view-projection (Highlights draw with it).
    last_view_proj: Mat4,
    /// SurfaceGuis / BillboardGuis tested against the scene's depth (round 23).
    world_gui: Option<crate::world_gui::WorldGuiPass>,
    /// Terrain water (round 24, water.rs).
    water: crate::water::WaterPass,
    material_epoch: Instant,
    /// Fog for the world GUI pass (gui_fog).
    gui_fog: ([f32; 4], [f32; 3]),
    /// The cascades as the beams read them (matrices, spheres, tiles).
    gui_sun: crate::world_gui::SunCascades,
    gui_lightgrid: [f32; 4],
    gui_sun_params: [f32; 4],
    /// Local lights in lights_buf this frame (the effects read them).
    gui_n_lights: u32,
    // Input layouts & D3D11 fixed-function states
    il_part: ID3D11InputLayout,
    il_terrain: ID3D11InputLayout,
    il_line: ID3D11InputLayout,
    rs_cull_back: ID3D11RasterizerState,
    rs_cull_none: ID3D11RasterizerState,
    rs_scissor_back: ID3D11RasterizerState,
    rs_scissor_none: ID3D11RasterizerState,
    ds_none: ID3D11DepthStencilState,
    ds_always_ro: ID3D11DepthStencilState,
    ds_greater_rw: ID3D11DepthStencilState,
    ds_greater_ro: ID3D11DepthStencilState,
    ds_gequal_ro: ID3D11DepthStencilState,
    ds_less_eq_rw: ID3D11DepthStencilState,
    bs_opaque: ID3D11BlendState,
    bs_premul: ID3D11BlendState,
    bs_alpha: ID3D11BlendState,
    bs_no_color: ID3D11BlendState,
    bs_bloom_up: ID3D11BlendState,
    bs_ssao_apply: ID3D11BlendState,
    // Shaders
    vs_sky: ID3D11VertexShader,
    ps_sky: ID3D11PixelShader,
    vs_part: ID3D11VertexShader,
    ps_part_opaque: ID3D11PixelShader,
    ps_part_low: ID3D11PixelShader,
    ps_part: ID3D11PixelShader,
    ps_part_enc: ID3D11PixelShader,
    ps_decal_depth: ID3D11PixelShader,
    vs_terrain: ID3D11VertexShader,
    ps_terrain_low: ID3D11PixelShader,
    ps_terrain: ID3D11PixelShader,
    vs_terrain_shadow: ID3D11VertexShader,
    vs_shadow: ID3D11VertexShader,
    vs_light_shadow: ID3D11VertexShader,
    ps_light_shadow: ID3D11PixelShader,
    vs_line: ID3D11VertexShader,
    ps_line: ID3D11PixelShader,
    vs_post: ID3D11VertexShader,
    ps_glass_depth: ShaderPair,
    ps_post: ShaderPair,
    post_dof: bool,
    post_dof_near: bool,
    ps_dof_h: ShaderPair,
    ps_dof_v: ShaderPair,
    ps_near_h: ShaderPair,
    ps_near_v: ShaderPair,
    ps_near_masks: [ID3D11PixelShader; 6],
    ps_near_edge: ID3D11PixelShader,
    ps_near_bokeh: ID3D11PixelShader,
    ps_near_tent: ID3D11PixelShader,
    ps_ssao_down: ShaderPair,
    ps_ssao_main: ID3D11PixelShader,
    ps_ssao_blur_x: ID3D11PixelShader,
    ps_ssao_blur_y: ID3D11PixelShader,
    ps_ssao_apply: ID3D11PixelShader,
    ps_bloom_bright: ID3D11PixelShader,
    ps_bloom_down: ID3D11PixelShader,
    ps_bloom_up: ID3D11PixelShader,
    post_bloom_size: f32,
    post_deferred: bool,
    /// Local light shadow maps (Studio's Future path), allocated lazily when enabled.
    lshadow: Option<LightShadows>,
    lshadow_sampler: ID3D11SamplerState,
    lshadow_empty_slots_srv: ID3D11ShaderResourceView,
    /// Bumped whenever the static instances change (light shadow cache key).
    static_gen: u64,
    terrain_params_cb: ID3D11Buffer,
    terrain_params_cpu: RefCell<Vec<[f32; 4]>>,
    terrain_chunks: HashMap<[i32; 3], TerrainChunk>,
    /// Decoration grass blades by chunk (color and sun shadow passes).
    terrain_grass: HashMap<[i32; 3], TerrainChunk>,
    globals_cb: ID3D11Buffer,
    lights_buf: ID3D11Buffer,
    lights_srv: ID3D11ShaderResourceView,
    /// Sun shadow depth map + EVSM atlas, allocated lazily when sun shadows are enabled.
    sun_res: Option<SunShadowRes>,
    /// Terrain.Clouds' passes (clouds.rs), made on first use.
    clouds: Option<crate::clouds::CloudsGpu>,
    sky_srv: ID3D11ShaderResourceView,
    /// Studio's indoor environment (textures/sky/indoor512); white 1×1 until
    /// loaded. The scene probe's slots start as it (env_probe_gpu).
    indoor_srv: ID3D11ShaderResourceView,
    /// Studio's scene environment probe (env_probe.rs) and the globals its
    /// faces are drawn with.
    probe: env_probe_gpu::ProbeGpu,
    /// Studio's split-sum BRDF table (brdfLUT.dds, RG16F).
    brdf_srv: ID3D11ShaderResourceView,
    sky_sampler: ID3D11SamplerState,
    /// Studio's light grid: skylight and sun per 4-stud voxel (lightgrid.rs).
    lightgrid_tex: ID3D11Texture3D,
    lightgrid_srv: ID3D11ShaderResourceView,
    localgrid_tex: ID3D11Texture3D,
    localgrid_srv: ID3D11ShaderResourceView,
    /// How much of each voxel parts fill, for local lights (lightocc.rs).
    lightocc_tex: ID3D11Texture3D,
    lightocc_srv: ID3D11ShaderResourceView,
    /// Studio's legacy surface atlas (studs.dds); grey (no change) until loaded.
    studs_srv: ID3D11ShaderResourceView,
    /// Material texture packs (matpack.rs): one array layer per pack,
    /// colour + height and normal + roughness + metalness.
    materials: MaterialArrays,
    /// Size of the loaded skybox faces (0 = none, draw the gradient).
    pub sky_size: u32,
    /// 1×1 black: the emissive mask of textures without one.
    black_srv: ID3D11ShaderResourceView,
    meshes: Vec<Mesh>,
    /// (mesh, cull back) -> the second slot drawing it the other way.
    cull_alias: HashMap<(u32, bool), u32>,
    sampler_low: ID3D11SamplerState,
    sampler_mid: ID3D11SamplerState,
    sampler: ID3D11SamplerState,
    /// Texture slots (`t15..t17`); index 0 is plain white.
    textures: Vec<PartTex>,
    /// See-through draws in Studio's encoded blend (SeeThrough).
    see: SeeThrough,
    /// Per-frame instances: moving and see-through parts, overlay.
    instance_buf: ID3D11Buffer,
    instance_cap: usize,
    static_buf: ID3D11Buffer,
    static_cap: usize,
    items: HashMap<Ref, Item>,
    /// Items drawn from the per-frame list (pending and dynamic).
    per_frame: HashSet<Ref>,
    /// ViewportFrames' parts (vf_render.rs): never in the world's lists;
    /// each frame's items, and a generation bumped whenever one changes.
    vf: vf::Layers,
    st: StaticLayout,
    pending: usize,
    pending_at: Option<Instant>,
    /// Last static rebuild / quiet-part scan (both rate-limited).
    last_rebuild: Instant,
    last_quiet_scan: Instant,
    pub stats: RenderStats,
    line_buf: ID3D11Buffer,
    line_cap: usize,
    target: Option<Target>,
    pub texture_id: Option<egui::TextureId>,
}

fn unbind_rtv_srvs(ctx: &ID3D11DeviceContext) {
    crate::d3d11::unbind_ps_srvs(ctx, 2, 1);
    crate::d3d11::unbind_ps_srvs(ctx, 8, 1);
    crate::d3d11::unbind_ps_srvs(ctx, 10, 1);
    crate::d3d11::unbind_ps_srvs(ctx, 14, 10);
}

fn input_elem(
    index: u32,
    format: DXGI_FORMAT,
    slot: u32,
    offset: usize,
    per_instance: bool,
) -> D3D11_INPUT_ELEMENT_DESC {
    D3D11_INPUT_ELEMENT_DESC {
        SemanticName: windows::core::s!("TEXCOORD"),
        SemanticIndex: index,
        Format: format,
        InputSlot: slot,
        AlignedByteOffset: offset as u32,
        InputSlotClass: if per_instance {
            D3D11_INPUT_PER_INSTANCE_DATA
        } else {
            D3D11_INPUT_PER_VERTEX_DATA
        },
        InstanceDataStepRate: if per_instance { 1 } else { 0 },
    }
}

fn make_blend_custom(
    device: &ID3D11Device,
    src: D3D11_BLEND,
    dst: D3D11_BLEND,
    op: D3D11_BLEND_OP,
    src_a: D3D11_BLEND,
    dst_a: D3D11_BLEND,
    op_a: D3D11_BLEND_OP,
) -> ID3D11BlendState {
    let mut desc = D3D11_BLEND_DESC::default();
    desc.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC {
        BlendEnable: true.into(),
        SrcBlend: src,
        DestBlend: dst,
        BlendOp: op,
        SrcBlendAlpha: src_a,
        DestBlendAlpha: dst_a,
        BlendOpAlpha: op_a,
        RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
    };
    let mut out = None;
    unsafe { device.CreateBlendState(&desc, Some(&mut out)) }.expect("CreateBlendState custom");
    out.unwrap()
}

fn make_tex2d_data(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    mips: u32,
    array_size: u32,
    format: DXGI_FORMAT,
    misc_flags: u32,
    data: &[u8],
    bpp: u32,
) -> ID3D11Texture2D {
    let (w, h, mips, array_size) = (width.max(1), height.max(1), mips.max(1), array_size.max(1));
    let mut subs = Vec::with_capacity((mips * array_size) as usize);
    let mut off = 0usize;
    for _ in 0..array_size {
        for m in 0..mips {
            let mw = (w >> m).max(1);
            let mh = (h >> m).max(1);
            let pitch = mw * bpp;
            let slice = pitch * mh;
            let end = off + slice as usize;
            assert!(
                end <= data.len(),
                "texture upload truncated: need {end}, got {} ({w}x{h}, mips={mips}, array={array_size}, bpp={bpp})",
                data.len()
            );
            subs.push(D3D11_SUBRESOURCE_DATA {
                pSysMem: data[off..].as_ptr().cast(),
                SysMemPitch: pitch,
                SysMemSlicePitch: slice,
            });
            off = end;
        }
    }
    crate::d3d11::create_tex2d(
        device,
        &crate::d3d11::Tex2dSpec {
            width: w,
            height: h,
            mips,
            array_size,
            format,
            samples: 1,
            bind_flags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            misc_flags,
        },
        Some(&subs),
    )
}

impl Renderer {
    pub fn new(rs: &crate::d3d11::RenderState) -> Self {
        let device = &rs.device;
        let ctx = &rs.context;
        let can_msaa_4x = rs.caps.msaa_fp16_max_samples >= 4;
        let mut quality = crate::quality::QualityProfile::resolve(&rs.caps, 0, true, None);
        quality.msaa_samples = if quality.msaa_samples >= 4 && can_msaa_4x { 4 } else { 1 };
        let samples = quality.msaa_samples;

        let globals_cb = crate::d3d11::create_cb(device, std::mem::size_of::<Globals>());
        let (lights_buf, lights_srv) = crate::d3d11::create_structured_srv(
            device,
            std::mem::size_of::<LightRaw>() as u32,
            crate::mech::MAX_LIGHTS.max(1) as u32,
            true,
            None,
        );

        let [lg_x, lg_y, lg_z] = crate::lightgrid::dims().map(|n| n as u32);
        let lightgrid_tex = crate::d3d11::create_tex3d(
            device,
            lg_x,
            lg_z,
            lg_y,
            1,
            DXGI_FORMAT_R8G8_UNORM,
            D3D11_BIND_SHADER_RESOURCE.0 as u32,
            None,
        );
        let lightgrid_srv = crate::d3d11::create_srv_3d(device, &lightgrid_tex, DXGI_FORMAT_R8G8_UNORM, 1);

        let localgrid_tex = crate::d3d11::create_tex3d(
            device,
            lg_x,
            lg_z,
            lg_y,
            1,
            DXGI_FORMAT_R8G8B8A8_UNORM,
            D3D11_BIND_SHADER_RESOURCE.0 as u32,
            None,
        );
        let localgrid_srv = crate::d3d11::create_srv_3d(device, &localgrid_tex, DXGI_FORMAT_R8G8B8A8_UNORM, 1);

        let [lo_x, lo_y, lo_z] = crate::lightocc::dims().map(|n| n as u32);
        let lightocc_tex = crate::d3d11::create_tex3d(
            device,
            lo_x,
            lo_y,
            lo_z,
            1,
            DXGI_FORMAT_R8_UNORM,
            D3D11_BIND_SHADER_RESOURCE.0 as u32,
            None,
        );
        let lightocc_srv = crate::d3d11::create_srv_3d(device, &lightocc_tex, DXGI_FORMAT_R8_UNORM, 1);

        let sky_srv = Self::cube_view(
            device,
            1,
            &[vec![255u8; 4], vec![255; 4], vec![255; 4], vec![255; 4], vec![255; 4], vec![255; 4]],
        );
        let indoor_srv = Self::cube_view(
            device,
            1,
            &[
                vec![0u8, 0, 0, 255],
                vec![0, 0, 0, 255],
                vec![0, 0, 0, 255],
                vec![0, 0, 0, 255],
                vec![0, 0, 0, 255],
                vec![0, 0, 0, 255],
            ],
        );
        let brdf_srv = Self::brdf_lut_view(device);
        let sky_sampler = crate::d3d11::create_sampler(
            device,
            D3D11_FILTER_MIN_MAG_MIP_LINEAR,
            D3D11_TEXTURE_ADDRESS_CLAMP,
            D3D11_TEXTURE_ADDRESS_CLAMP,
            D3D11_TEXTURE_ADDRESS_CLAMP,
            1,
        );
        let studs_srv = Self::studs_texture(device, 1, 1, &[128, 128, 128, 255]);
        let materials = MaterialArrays::new(device, ctx, 1, 1);
        let lshadow_sampler = crate::d3d11::create_sampler(
            device,
            D3D11_FILTER_MIN_MAG_LINEAR_MIP_POINT,
            D3D11_TEXTURE_ADDRESS_CLAMP,
            D3D11_TEXTURE_ADDRESS_CLAMP,
            D3D11_TEXTURE_ADDRESS_CLAMP,
            1,
        );
        let (_, lshadow_empty_slots_srv) = crate::d3d11::create_structured_srv(device, 6 * 64 + 16, 1, false, None);
        // Studio's scene environment probe (env_probe.rs): t8 is current slot, t14 blend target.
        let probe = env_probe_gpu::ProbeGpu::new(device, ctx, samples, std::mem::size_of::<Globals>());

        let black_tex = make_tex2d_data(device, 1, 1, 1, 1, DXGI_FORMAT_R8G8B8A8_UNORM, 0, &[0, 0, 0, 255], 4);
        let black_srv = crate::d3d11::create_srv_2d(device, &black_tex, DXGI_FORMAT_R8G8B8A8_UNORM, false, 0, 1);
        let sampler_low = crate::d3d11::create_sampler(
            device,
            D3D11_FILTER_MIN_MAG_MIP_LINEAR,
            D3D11_TEXTURE_ADDRESS_WRAP,
            D3D11_TEXTURE_ADDRESS_WRAP,
            D3D11_TEXTURE_ADDRESS_WRAP,
            crate::mech::QUALITY_ANISO_LOW,
        );
        let sampler_mid = crate::d3d11::create_sampler(
            device,
            D3D11_FILTER_ANISOTROPIC,
            D3D11_TEXTURE_ADDRESS_WRAP,
            D3D11_TEXTURE_ADDRESS_WRAP,
            D3D11_TEXTURE_ADDRESS_WRAP,
            crate::mech::QUALITY_ANISO_MID,
        );
        let sampler = crate::d3d11::create_sampler(
            device,
            D3D11_FILTER_ANISOTROPIC,
            D3D11_TEXTURE_ADDRESS_WRAP,
            D3D11_TEXTURE_ADDRESS_WRAP,
            D3D11_TEXTURE_ADDRESS_WRAP,
            crate::mech::TEXTURE_ANISOTROPY as u32,
        );

        let part_descs = [
            input_elem(0, DXGI_FORMAT_R32G32B32_FLOAT, 0, std::mem::offset_of!(Vertex, pos), false),
            input_elem(1, DXGI_FORMAT_R32G32B32_FLOAT, 0, std::mem::offset_of!(Vertex, normal), false),
            input_elem(8, DXGI_FORMAT_R32G32_FLOAT, 0, std::mem::offset_of!(Vertex, uv), false),
            input_elem(12, DXGI_FORMAT_R32G32B32A32_FLOAT, 0, std::mem::offset_of!(Vertex, projection), false),
            input_elem(11, DXGI_FORMAT_R32G32B32A32_FLOAT, 0, std::mem::offset_of!(Vertex, color), false),
            input_elem(2, DXGI_FORMAT_R32G32B32A32_FLOAT, 1, std::mem::offset_of!(InstanceRaw, r0), true),
            input_elem(3, DXGI_FORMAT_R32G32B32A32_FLOAT, 1, std::mem::offset_of!(InstanceRaw, r1), true),
            input_elem(4, DXGI_FORMAT_R32G32B32A32_FLOAT, 1, std::mem::offset_of!(InstanceRaw, r2), true),
            input_elem(5, DXGI_FORMAT_R32G32B32A32_FLOAT, 1, std::mem::offset_of!(InstanceRaw, size), true),
            input_elem(6, DXGI_FORMAT_R32G32B32A32_FLOAT, 1, std::mem::offset_of!(InstanceRaw, color), true),
            input_elem(7, DXGI_FORMAT_R32G32B32A32_FLOAT, 1, std::mem::offset_of!(InstanceRaw, extra), true),
            input_elem(9, DXGI_FORMAT_R32G32B32A32_FLOAT, 1, std::mem::offset_of!(InstanceRaw, uvt), true),
            input_elem(10, DXGI_FORMAT_R32G32B32A32_FLOAT, 1, std::mem::offset_of!(InstanceRaw, emissive), true),
            input_elem(13, DXGI_FORMAT_R32G32B32A32_FLOAT, 1, std::mem::offset_of!(InstanceRaw, tint), true),
        ];
        let terrain_descs = [
            input_elem(0, DXGI_FORMAT_R32G32B32_FLOAT, 0, std::mem::offset_of!(crate::terrain::TerrainVertex, pos), false),
            input_elem(1, DXGI_FORMAT_R8G8B8A8_SNORM, 0, std::mem::offset_of!(crate::terrain::TerrainVertex, normal), false),
            input_elem(2, DXGI_FORMAT_R8G8B8A8_UINT, 0, std::mem::offset_of!(crate::terrain::TerrainVertex, palette), false),
            input_elem(3, DXGI_FORMAT_R8G8B8A8_UINT, 0, std::mem::offset_of!(crate::terrain::TerrainVertex, axis), false),
            input_elem(4, DXGI_FORMAT_R8G8B8A8_UINT, 0, std::mem::offset_of!(crate::terrain::TerrainVertex, rand), false),
            input_elem(5, DXGI_FORMAT_R32G32B32A32_FLOAT, 0, std::mem::offset_of!(crate::terrain::TerrainVertex, wind_local), false),
        ];
        let line_descs = [
            input_elem(0, DXGI_FORMAT_R32G32B32_FLOAT, 0, std::mem::offset_of!(LineVertex, pos), false),
            input_elem(1, DXGI_FORMAT_R32G32B32A32_FLOAT, 0, std::mem::offset_of!(LineVertex, color), false),
        ];

        let il_part = crate::d3d11::create_input_layout(device, &part_descs, shaders::VS_PART);
        let il_terrain = crate::d3d11::create_input_layout(device, &terrain_descs, shaders::VS_TERRAIN);
        let il_line = crate::d3d11::create_input_layout(device, &line_descs, shaders::VS_LINE);

        let rs_cull_back = crate::d3d11::create_raster_state(device, D3D11_CULL_BACK, false, false, 0, 0.0);
        let rs_cull_none = crate::d3d11::create_raster_state(device, D3D11_CULL_NONE, false, false, 0, 0.0);
        let rs_scissor_back = crate::d3d11::create_raster_state(device, D3D11_CULL_BACK, false, true, 0, 0.0);
        let rs_scissor_none = crate::d3d11::create_raster_state(device, D3D11_CULL_NONE, false, true, 0, 0.0);

        let ds_none = crate::d3d11::create_depth_state(device, false, false, D3D11_COMPARISON_ALWAYS);
        let ds_always_ro = crate::d3d11::create_depth_state(device, true, false, D3D11_COMPARISON_ALWAYS);
        let ds_greater_rw = crate::d3d11::create_depth_state(device, true, true, D3D11_COMPARISON_GREATER);
        let ds_greater_ro = crate::d3d11::create_depth_state(device, true, false, D3D11_COMPARISON_GREATER);
        let ds_gequal_ro = crate::d3d11::create_depth_state(device, true, false, D3D11_COMPARISON_GREATER_EQUAL);
        let ds_less_eq_rw = crate::d3d11::create_depth_state(device, true, true, D3D11_COMPARISON_LESS_EQUAL);

        let bs_opaque = crate::d3d11::create_blend_state(device, crate::d3d11::BlendMode::Opaque);
        let bs_premul = crate::d3d11::create_blend_state(device, crate::d3d11::BlendMode::PremulAlpha);
        let bs_alpha = crate::d3d11::create_blend_state(device, crate::d3d11::BlendMode::AlphaBlend);
        let bs_no_color = crate::d3d11::create_blend_state(device, crate::d3d11::BlendMode::NoColorWrite);
        let bs_bloom_up = make_blend_custom(
            device,
            D3D11_BLEND_ONE,
            D3D11_BLEND_ONE,
            D3D11_BLEND_OP_ADD,
            D3D11_BLEND_ONE,
            D3D11_BLEND_ZERO,
            D3D11_BLEND_OP_ADD,
        );
        let bs_ssao_apply = make_blend_custom(
            device,
            D3D11_BLEND_ZERO,
            D3D11_BLEND_SRC_COLOR,
            D3D11_BLEND_OP_ADD,
            D3D11_BLEND_ONE,
            D3D11_BLEND_INV_SRC_ALPHA,
            D3D11_BLEND_OP_ADD,
        );

        let pair = |x1: &[u8], msaa: &[u8]| ShaderPair {
            x1: crate::d3d11::create_ps(device, x1),
            msaa: crate::d3d11::create_ps(device, msaa),
        };

        let vs_sky = crate::d3d11::create_vs(device, shaders::VS_SKY);
        let ps_sky = crate::d3d11::create_ps(device, shaders::PS_SKY);
        let vs_part = crate::d3d11::create_vs(device, shaders::VS_PART);
        let ps_part_opaque = crate::d3d11::create_ps(device, shaders::PS_PART_OPAQUE);
        let ps_part_low = crate::d3d11::create_ps(device, shaders::PS_PART_LOW);
        let ps_part = crate::d3d11::create_ps(device, shaders::PS_PART);
        let ps_part_enc = crate::d3d11::create_ps(device, shaders::PS_PART_ENC);
        let ps_decal_depth = crate::d3d11::create_ps(device, shaders::PS_DECAL_DEPTH);
        let vs_terrain = crate::d3d11::create_vs(device, shaders::VS_TERRAIN);
        let ps_terrain_low = crate::d3d11::create_ps(device, shaders::PS_TERRAIN_LOW);
        let ps_terrain = crate::d3d11::create_ps(device, shaders::PS_TERRAIN);
        let vs_terrain_shadow = crate::d3d11::create_vs(device, shaders::VS_TERRAIN_SHADOW);
        let vs_shadow = crate::d3d11::create_vs(device, shaders::VS_SHADOW);
        let vs_light_shadow = crate::d3d11::create_vs(device, shaders::VS_LIGHT_SHADOW);
        let ps_light_shadow = crate::d3d11::create_ps(device, shaders::PS_LIGHT_SHADOW);
        let vs_line = crate::d3d11::create_vs(device, shaders::VS_LINE);
        let ps_line = crate::d3d11::create_ps(device, shaders::PS_LINE);
        let vs_post = crate::d3d11::create_vs(device, shaders::VS_POST);

        let ps_glass_depth = pair(shaders::PS_GLASS_DEPTH_1X, shaders::PS_GLASS_DEPTH_MSAA);
        let ps_post = pair(shaders::PS_POST_1X, shaders::PS_POST_MSAA);
        let ps_dof_h = pair(shaders::PS_DOF_H_1X, shaders::PS_DOF_H_MSAA);
        let ps_dof_v = pair(shaders::PS_DOF_V_1X, shaders::PS_DOF_V_MSAA);
        let ps_near_h = pair(shaders::PS_NEAR_H_1X, shaders::PS_NEAR_H_MSAA);
        let ps_near_v = pair(shaders::PS_NEAR_V_1X, shaders::PS_NEAR_V_MSAA);
        let ps_near_masks = [
            crate::d3d11::create_ps(device, shaders::PS_NEAR_MASK0),
            crate::d3d11::create_ps(device, shaders::PS_NEAR_MASK1),
            crate::d3d11::create_ps(device, shaders::PS_NEAR_MASK2),
            crate::d3d11::create_ps(device, shaders::PS_NEAR_MASK3),
            crate::d3d11::create_ps(device, shaders::PS_NEAR_MASK4),
            crate::d3d11::create_ps(device, shaders::PS_NEAR_MASK5),
        ];
        let ps_near_edge = crate::d3d11::create_ps(device, shaders::PS_NEAR_EDGE);
        let ps_near_bokeh = crate::d3d11::create_ps(device, shaders::PS_NEAR_BOKEH);
        let ps_near_tent = crate::d3d11::create_ps(device, shaders::PS_NEAR_TENT);

        let ps_ssao_down = pair(shaders::PS_SSAO_DOWN_1X, shaders::PS_SSAO_DOWN_MSAA);
        let ps_ssao_main = crate::d3d11::create_ps(device, shaders::PS_SSAO);
        let ps_ssao_blur_x = crate::d3d11::create_ps(device, shaders::PS_SSAO_BLUR_X);
        let ps_ssao_blur_y = crate::d3d11::create_ps(device, shaders::PS_SSAO_BLUR_Y);
        let ps_ssao_apply = crate::d3d11::create_ps(device, shaders::PS_SSAO_APPLY);

        let ps_bloom_bright = crate::d3d11::create_ps(device, shaders::PS_BLOOM_BRIGHT);
        let ps_bloom_down = crate::d3d11::create_ps(device, shaders::PS_BLOOM_DOWN);
        let ps_bloom_up = crate::d3d11::create_ps(device, shaders::PS_BLOOM_UP);

        let see = SeeThrough::new(device);
        let water = crate::water::WaterPass::new(device, ctx, samples);
        let terrain_params_cpu = RefCell::new(vec![[0.0f32; 4]; TERRAIN_PALETTE * 4]);
        let terrain_params_cb = crate::d3d11::create_cb(device, TERRAIN_PALETTE * 4 * 16);

        let meshes = build_meshes()
            .into_iter()
            .map(|verts| Mesh {
                cull: Some(true),
                storage: false,
                count: verts.len() as u32,
                vbuf: crate::d3d11::create_default_buffer(
                    device,
                    D3D11_BIND_VERTEX_BUFFER.0 as u32,
                    std::mem::size_of_val(verts.as_slice()),
                    Some(bytemuck::cast_slice(&verts)),
                ),
            })
            .collect();

        let instance_cap = 1024;
        let line_cap = 4096;
        let mut r = Renderer {
            quality,
            can_msaa_4x,
            last_cascades: None,
            probe_samples: samples,
            skin: crate::skin_gpu::SkinGpu::new(device),
            gui_fog: ([0.0, 1e9, 0.0, 0.0], [0.0; 3]),
            gui_sun: Default::default(),
            gui_lightgrid: [0.0; 4],
            gui_sun_params: [0.0, 580.0, 0.05, 0.0],
            gui_n_lights: 0,
            world_gui: None,
            highlight: None,
            last_view_proj: Mat4::IDENTITY,
            water,
            material_epoch: Instant::now(),
            il_part,
            il_terrain,
            il_line,
            rs_cull_back,
            rs_cull_none,
            rs_scissor_back,
            rs_scissor_none,
            ds_none,
            ds_always_ro,
            ds_greater_rw,
            ds_greater_ro,
            ds_gequal_ro,
            ds_less_eq_rw,
            bs_opaque,
            bs_premul,
            bs_alpha,
            bs_no_color,
            bs_bloom_up,
            bs_ssao_apply,
            vs_sky,
            ps_sky,
            vs_part,
            ps_part_opaque,
            ps_part_low,
            ps_part,
            ps_part_enc,
            ps_decal_depth,
            vs_terrain,
            ps_terrain_low,
            ps_terrain,
            vs_terrain_shadow,
            vs_shadow,
            vs_light_shadow,
            ps_light_shadow,
            vs_line,
            ps_line,
            vs_post,
            ps_glass_depth,
            ps_post,
            post_dof: false,
            post_dof_near: false,
            ps_dof_h,
            ps_dof_v,
            ps_near_h,
            ps_near_v,
            ps_near_masks,
            ps_near_edge,
            ps_near_bokeh,
            ps_near_tent,
            ps_ssao_down,
            ps_ssao_main,
            ps_ssao_blur_x,
            ps_ssao_blur_y,
            ps_ssao_apply,
            ps_bloom_bright,
            ps_bloom_down,
            ps_bloom_up,
            post_bloom_size: 24.0,
            post_deferred: false,
            lshadow: None,
            lshadow_sampler,
            lshadow_empty_slots_srv,
            static_gen: 0,
            terrain_params_cb,
            terrain_params_cpu,
            terrain_chunks: HashMap::new(),
            terrain_grass: HashMap::new(),
            globals_cb,
            lights_buf,
            lights_srv,
            sun_res: None,
            clouds: None,
            sky_srv,
            indoor_srv,
            probe,
            brdf_srv,
            sky_sampler,
            lightgrid_tex,
            lightgrid_srv,
            localgrid_tex,
            localgrid_srv,
            lightocc_tex,
            lightocc_srv,
            studs_srv,
            materials,
            sky_size: 0,
            meshes,
            cull_alias: HashMap::new(),
            sampler_low,
            sampler_mid,
            sampler,
            textures: Vec::new(),
            black_srv,
            see,
            instance_buf: Self::make_buf(device, instance_cap * std::mem::size_of::<InstanceRaw>()),
            instance_cap,
            static_buf: Self::make_static_buf(device, instance_cap * std::mem::size_of::<InstanceRaw>()),
            static_cap: instance_cap,
            items: HashMap::new(),
            per_frame: HashSet::new(),
            vf: Default::default(),
            st: StaticLayout::default(),
            pending: 0,
            pending_at: None,
            last_rebuild: Instant::now(),
            last_quiet_scan: Instant::now(),
            stats: RenderStats::default(),
            line_buf: Self::make_buf(device, line_cap * std::mem::size_of::<LineVertex>()),
            line_cap,
            target: None,
            texture_id: None,
        };
        r.add_texture(device, ctx, 1, 1, &[255, 255, 255, 255]);
        r
    }

    pub fn set_quality(&mut self, mut q: crate::quality::QualityProfile) {
        q.msaa_samples = if q.msaa_samples >= 4 && self.can_msaa_4x { 4 } else { 1 };
        if !q.enable_shadows
            || q.shadow_cascades == 0
            || q.shadow_atlas_side != self.quality.shadow_atlas_side
        {
            self.sun_res = None;
            self.last_cascades = None;
        } else if q.shadow_cascades != self.quality.shadow_cascades {
            if let Some(s) = &mut self.sun_res {
                s.sun.forget();
            }
        }
        if !q.enable_shadows
            || q.max_local_shadows == 0
            || q.shadow_atlas_side != self.quality.shadow_atlas_side
        {
            self.lshadow = None;
        } else if q.max_local_shadows != self.quality.max_local_shadows {
            if let Some(l) = &mut self.lshadow {
                l.key = None;
            }
        }
        if let Some(t) = self.target.as_mut() {
            if !q.enable_bloom {
                t.bloom = None;
            }
            if !q.enable_ssao {
                t.ssao = None;
            }
            if !q.enable_dof {
                t.dof = None;
                t.near = None;
            }
            if q.level < crate::mech::QUALITY_WATER_REFRACT_MIN_LEVEL {
                t.water_depth_dsv = None;
            }
        }
        self.quality = q;
    }

    pub(super) fn samples(&self) -> u32 {
        self.quality.msaa_samples
    }

    fn active_sampler(&self) -> &ID3D11SamplerState {
        if self.quality.max_anisotropy <= crate::mech::QUALITY_ANISO_LOW {
            &self.sampler_low
        } else if self.quality.max_anisotropy <= crate::mech::QUALITY_ANISO_MID {
            &self.sampler_mid
        } else {
            &self.sampler
        }
    }

    /// The last frame as shown (RGBA8, top row first): width, height,
    /// pixels. Waits for the GPU; for tests.
    pub fn read_frame(&self, rs: &crate::d3d11::RenderState) -> Option<(u32, u32, Vec<u8>)> {
        let target = self.target.as_ref()?;
        let [w, h] = target.size;
        let out = crate::d3d11::read_tex2d_rgba8(&rs.device, &rs.context, &target.display_tex, w, h);
        Some((w, h, out))
    }

    /// Bumps whenever the static scene is rebuilt: the scene environment
    /// probe's world revision (env_probe.rs).
    pub fn static_gen(&self) -> u64 {
        self.static_gen
    }

    /// Raw RGBA16Float scene before bloom/tone, for render diagnostics.
    pub fn read_hdr(&self, rs: &crate::d3d11::RenderState) -> Option<(u32, u32, Vec<u8>)> {
        let target = self.target.as_ref()?;
        if let Ok(path) = std::env::var("RUSTUDIO_DUMP_HDR") {
            let half = [(target.size[0] >> 1).max(1), (target.size[1] >> 1).max(1)];
            if let Some(bloom) = &target.bloom {
                if let Some(texture) = &bloom.bright_capture {
                    if let Some((w, h, data)) = self.read_hdr_texture(rs, texture, half) {
                        let _ = std::fs::write(format!("{path}.extract-{w}x{h}"), data);
                    }
                }
                if let Some((w, h, data)) = self.read_hdr_texture(rs, &bloom.textures[0], half) {
                    let _ = std::fs::write(format!("{path}.bloom-{w}x{h}"), data);
                }
            }
            // The scene environment probe's current slot, level 0 (env_probe.rs).
            for f in 0..6 {
                let size = [crate::env_probe::SIZE; 2];
                if let Some((w, h, data)) = self.read_hdr_layer(rs, &self.probe.slots[self.probe.cur], size, f) {
                    let _ = std::fs::write(format!("{path}.probe{f}-{w}x{h}"), data);
                }
            }
        }
        self.read_hdr_texture(rs, &target.hdr_tex, target.size)
    }

    fn read_hdr_texture(
        &self,
        rs: &crate::d3d11::RenderState,
        texture: &ID3D11Texture2D,
        size: [u32; 2],
    ) -> Option<(u32, u32, Vec<u8>)> {
        let [w, h] = size;
        let out = crate::d3d11::read_tex2d_rgba16f(&rs.device, &rs.context, texture, w, h);
        Some((w, h, out))
    }

    fn read_hdr_layer(
        &self,
        rs: &crate::d3d11::RenderState,
        texture: &ID3D11Texture2D,
        size: [u32; 2],
        layer: u32,
    ) -> Option<(u32, u32, Vec<u8>)> {
        let [w, h] = size;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        let src_sub = layer * desc.MipLevels.max(1);
        desc.Width = w;
        desc.Height = h;
        desc.MipLevels = 1;
        desc.ArraySize = 1;
        desc.SampleDesc = DXGI_SAMPLE_DESC { Count: 1, Quality: 0 };
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        desc.MiscFlags = 0;

        let mut staging = None;
        unsafe { rs.device.CreateTexture2D(&desc, None, Some(&mut staging)) }.ok()?;
        let staging = staging?;
        unsafe { rs.context.CopySubresourceRegion(&staging, 0, 0, 0, 0, texture, src_sub, None) };

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe { rs.context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }.ok()?;
        let row_bytes = (w * 8) as usize;
        let pitch = mapped.RowPitch as usize;
        let mut out = Vec::with_capacity(row_bytes * h as usize);
        for y in 0..h as usize {
            unsafe {
                let src = (mapped.pData as *const u8).add(y * pitch);
                out.extend_from_slice(std::slice::from_raw_parts(src, row_bytes));
            }
        }
        unsafe { rs.context.Unmap(&staging, 0) };
        Some((w, h, out))
    }

    /// Uploads an RGBA8 (sRGB) image; returns its index for [`PartDraw::texture`].
    pub fn add_texture(
        &mut self,
        device: &ID3D11Device,
        ctx: &ID3D11DeviceContext,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> u32 {
        self.add_texture_emissive(device, ctx, width, height, rgba, None)
    }

    /// A texture with a SurfaceAppearance emissive mask (RGBA8, its own size;
    /// read as data, not sRGB).
    pub fn add_texture_emissive(
        &mut self,
        device: &ID3D11Device,
        ctx: &ID3D11DeviceContext,
        width: u32,
        height: u32,
        rgba: &[u8],
        mask: Option<(u32, u32, &[u8])>,
    ) -> u32 {
        self.add_texture_surface(device, ctx, width, height, rgba, mask, None)
    }

    pub fn add_texture_surface(
        &mut self,
        device: &ID3D11Device,
        _ctx: &ID3D11DeviceContext,
        width: u32,
        height: u32,
        rgba: &[u8],
        mask: Option<(u32, u32, &[u8])>,
        surface: Option<(u32, u32, &[u8])>,
    ) -> u32 {
        let (sw, sh, sp) = surface.unwrap_or((1, 1, &[128, 128, 255, 0]));
        let (sw, sh) = (sw.max(1), sh.max(1));
        let surface_mips = mip_levels(sw, sh);
        let surface_data = data_mip_chain(sw, sh, sp);
        let surface_tex = make_tex2d_data(
            device,
            sw,
            sh,
            surface_mips,
            1,
            DXGI_FORMAT_R8G8B8A8_UNORM,
            0,
            &surface_data,
            4,
        );
        let surface_srv = crate::d3d11::create_srv_2d(
            device,
            &surface_tex,
            DXGI_FORMAT_R8G8B8A8_UNORM,
            false,
            0,
            surface_mips,
        );

        let emissive_srv = mask
            .map(|(w, h, px)| {
                let (mw, mh) = (w.max(1), h.max(1));
                let tex = make_tex2d_data(device, mw, mh, 1, 1, DXGI_FORMAT_R8G8B8A8_UNORM, 0, px, 4);
                crate::d3d11::create_srv_2d(device, &tex, DXGI_FORMAT_R8G8B8A8_UNORM, false, 0, 1)
            })
            .unwrap_or_else(|| self.black_srv.clone());

        let (w, h) = (width.max(1), height.max(1));
        let ready = READY_MIPS.with(|c| {
            let mut c = c.borrow_mut();
            match c.as_ref() {
                Some((p, _)) if *p == rgba.as_ptr() as usize => c.take().map(|m| m.1),
                _ => None,
            }
        });
        let (levels, mips_vec): (u32, Vec<u8>) = if rgba.len() == (w * h * 4) as usize {
            (mip_levels(w, h), ready.unwrap_or_else(|| mip_chain(w, h, rgba)))
        } else {
            (1, rgba.to_vec())
        };
        let tex = make_tex2d_data(
            device,
            w,
            h,
            levels,
            1,
            DXGI_FORMAT_R8G8B8A8_UNORM_SRGB,
            0,
            &mips_vec,
            4,
        );
        let albedo_srv = crate::d3d11::create_srv_2d(
            device,
            &tex,
            DXGI_FORMAT_R8G8B8A8_UNORM_SRGB,
            false,
            0,
            levels,
        );
        let part_tex = PartTex { albedo_srv, emissive_srv, surface_srv };
        // An EditableImage texture re-uploaded: into its old slot (the old
        // COM views and their textures are freed), not a new one.
        if let Some(i) = REUSE_TEXTURE_SLOT.with(|c| c.take()) {
            if let Some(slot) = self.textures.get_mut(i as usize) {
                *slot = part_tex;
                return i;
            }
        }
        self.textures.push(part_tex);
        (self.textures.len() - 1) as u32
    }

    pub(super) fn bind_scene_globals_with(&self, ctx: &ID3D11DeviceContext, globals_cb: &ID3D11Buffer) {
        let cbs = [Some(globals_cb.clone()), Some(self.materials.params_cb.clone())];
        let sun_srv = self.sun_res.as_ref().map_or(&self.black_srv, |s| &s.sun.atlas_srv);
        let lshadow_srv = self.lshadow.as_ref().map_or(&self.black_srv, |l| &l.moments_srv);
        let lshadow_slots_srv = self.lshadow.as_ref().map_or(&self.lshadow_empty_slots_srv, |l| &l.slots_srv);
        let srvs = [
            Some(self.lights_srv.clone()),                        // t0
            Some(self.sky_srv.clone()),                           // t1
            Some(sun_srv.clone()),                                // t2
            Some(self.studs_srv.clone()),                         // t3
            Some(self.materials.albedo_srv.clone()),              // t4
            Some(self.materials.surface_srv.clone()),             // t5
            Some(self.lightgrid_srv.clone()),                     // t6
            Some(self.lightocc_srv.clone()),                      // t7
            Some(self.probe.slot_srvs[self.probe.cur].clone()),   // t8
            Some(self.brdf_srv.clone()),                          // t9
            Some(lshadow_srv.clone()),                            // t10
            Some(lshadow_slots_srv.clone()),                      // t11
            Some(self.materials.emission_srv.clone()),            // t12
            Some(self.localgrid_srv.clone()),                     // t13
            Some(self.probe.slot_srvs[self.probe.next()].clone()), // t14
        ];
        let samps = [
            Some(self.sky_sampler.clone()),     // s0
            Some(self.active_sampler().clone()), // s1
            Some(self.lshadow_sampler.clone()), // s2
        ];
        let glass_fallback = [Some(self.black_srv.clone()), Some(self.black_srv.clone())];
        unsafe {
            ctx.VSSetConstantBuffers(0, Some(&cbs));
            ctx.PSSetConstantBuffers(0, Some(&cbs));
            // Vertex shaders only read globals/material CBs and vertex buffers, never t0..t14 / s0..s2.
            ctx.PSSetShaderResources(0, Some(&srvs));
            ctx.PSSetSamplers(0, Some(&samps));
            ctx.PSSetShaderResources(19, Some(&glass_fallback));
        }
    }

    pub(super) fn bind_instance_buffer(ctx: &ID3D11DeviceContext, inst_buf: &ID3D11Buffer) {
        let stride = std::mem::size_of::<InstanceRaw>() as u32;
        let offset = 0u32;
        unsafe {
            ctx.IASetVertexBuffers(1, 1, Some(&Some(inst_buf.clone())), Some(&stride), Some(&offset));
        }
    }

    pub(super) fn bind_part_pipeline(
        &self,
        ctx: &ID3D11DeviceContext,
        transparent: bool,
        cull_back: bool,
    ) {
        let low_shaders = self.quality.level <= crate::mech::QUALITY_LOW_SHADER_MAX_LEVEL;
        let ps = if low_shaders {
            &self.ps_part_low
        } else if transparent {
            &self.ps_part
        } else {
            &self.ps_part_opaque
        };
        unsafe {
            ctx.IASetInputLayout(&self.il_part);
            ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            ctx.VSSetShader(&self.vs_part, None);
            ctx.PSSetShader(ps, None);
            ctx.PSSetSamplers(3, Some(&[Some(self.active_sampler().clone())]));
            ctx.RSSetState(if cull_back { &self.rs_cull_back } else { &self.rs_cull_none });
            ctx.OMSetDepthStencilState(
                if transparent { &self.ds_gequal_ro } else { &self.ds_greater_rw },
                0,
            );
            ctx.OMSetBlendState(
                if transparent { &self.bs_premul } else { &self.bs_opaque },
                Some(&[0.0; 4]),
                0xFFFFFFFF,
            );
        }
    }

    pub(super) fn draw_part_run(
        &self,
        ctx: &ID3D11DeviceContext,
        key: (usize, usize),
        range: std::ops::Range<u32>,
        last_tex: &mut Option<usize>,
    ) {
        let Some(mesh) = self.meshes.get(key.0) else {
            return;
        };
        if *last_tex != Some(key.1) {
            let tex = self.textures.get(key.1).unwrap_or(&self.textures[0]);
            let tex_srvs = [
                Some(tex.albedo_srv.clone()),
                Some(tex.emissive_srv.clone()),
                Some(tex.surface_srv.clone()),
            ];
            unsafe { ctx.PSSetShaderResources(15, Some(&tex_srvs)) };
            *last_tex = Some(key.1);
        }
        let stride = std::mem::size_of::<Vertex>() as u32;
        let offset = 0u32;
        crate::runtime::domains::services::perf::draw(
            crate::runtime::domains::services::perf::SCENE,
            1,
            (mesh.count / 3) as u64 * (range.end - range.start) as u64,
        );
        unsafe {
            ctx.IASetVertexBuffers(0, 1, Some(&Some(mesh.vbuf.clone())), Some(&stride), Some(&offset));
            ctx.DrawInstanced(mesh.count, range.end - range.start, 0, range.start);
        }
    }

    /// Studio's split-sum environment BRDF table: PlatformContent
    /// textures/brdfLUT.dds, 256 × 256 R16G16_FLOAT (DX10 header), read by the
    /// part shaders at (roughness, N·V) as PrecomputedBRDFTexture. Without the
    /// file a 1×1 (1, 0): Fresnel F0 alone.
    fn brdf_lut_view(device: &ID3D11Device) -> ID3D11ShaderResourceView {
        let file = crate::assets::resolve("rbxasset://textures/brdfLUT.dds", None)
            .and_then(|p| std::fs::read(p).ok());
        let parsed = file.and_then(|b| {
            if b.len() < 148 || &b[0..4] != b"DDS " || &b[84..88] != b"DX10" {
                return None;
            }
            let h = u32::from_le_bytes(b[12..16].try_into().ok()?);
            let w = u32::from_le_bytes(b[16..20].try_into().ok()?);
            let fmt = u32::from_le_bytes(b[128..132].try_into().ok()?);
            let n = (w * h * 4) as usize;
            // DXGI_FORMAT_R16G16_FLOAT = 34.
            (fmt == 34 && b.len() >= 148 + n).then(|| (w, h, b[148..148 + n].to_vec()))
        });
        // (f16 1.0 = 0x3C00)
        let (w, h, data) = parsed.unwrap_or((1, 1, vec![0x00, 0x3C, 0, 0]));
        let tex = make_tex2d_data(device, w, h, 1, 1, DXGI_FORMAT_R16G16_FLOAT, 0, &data, 4);
        crate::d3d11::create_srv_2d(device, &tex, DXGI_FORMAT_R16G16_FLOAT, false, 0, 1)
    }

    /// Sets Studio's indoor environment cube (textures/sky/indoor512, faces
    /// in cube order (+X, -X, +Y, -Y, +Z, -Z) like `set_sky`).
    pub fn set_indoor(
        &mut self,
        device: &ID3D11Device,
        ctx: &ID3D11DeviceContext,
        size: u32,
        faces: &[Vec<u8>; 6],
    ) {
        self.indoor_srv = Self::cube_view(device, size, faces);
        // Both probe slots start as this cube (env_probe_gpu fill_default).
        self.probe.fill_default(device, ctx, &self.indoor_srv, size);
    }

    /// A cube texture from six square RGBA8 faces (+X, −X, +Y, −Y, +Z, −Z).
    /// Texels stay gamma-encoded; the shader squares them as Studio does.
    fn cube_view(device: &ID3D11Device, size: u32, faces: &[Vec<u8>; 6]) -> ID3D11ShaderResourceView {
        // Each face with its mip chain (layer-major): the sky's reflection
        // on rough parts reads a blurred level.
        let data = cube_mips(size, faces);
        Self::cube_view_mipped(device, size, &data)
    }

    /// A cube texture from faces already with their mip chains (cube_mips).
    fn cube_view_mipped(device: &ID3D11Device, size: u32, data: &[u8]) -> ID3D11ShaderResourceView {
        let mips = mip_levels(size, size);
        let tex = make_tex2d_data(
            device,
            size,
            size,
            mips,
            6,
            DXGI_FORMAT_R8G8B8A8_UNORM,
            D3D11_RESOURCE_MISC_TEXTURECUBE.0 as u32,
            data,
            4,
        );
        crate::d3d11::create_srv_cube(device, &tex, DXGI_FORMAT_R8G8B8A8_UNORM, 0, mips)
    }

    /// The legacy surface atlas (RGBA8, kept as data: the shader reads its
    /// grey 0.5 as "no change"), with mips averaged in linear light.
    fn studs_texture(device: &ID3D11Device, w: u32, h: u32, rgba: &[u8]) -> ID3D11ShaderResourceView {
        let mips = mip_levels(w, h);
        let data = mip_chain(w, h, rgba);
        let tex = make_tex2d_data(device, w, h, mips, 1, DXGI_FORMAT_R8G8B8A8_UNORM, 0, &data, 4);
        crate::d3d11::create_srv_2d(device, &tex, DXGI_FORMAT_R8G8B8A8_UNORM, false, 0, mips)
    }

    /// Studio's studs.dds (PlatformContent/pc/textures): 128 px wide, four
    /// 512 px bands (studs, weld, inlet, universal), 2 × 8 studs each.
    pub fn set_studs(
        &mut self,
        device: &ID3D11Device,
        _ctx: &ID3D11DeviceContext,
        w: u32,
        h: u32,
        rgba: &[u8],
    ) {
        self.studs_srv = Self::studs_texture(device, w, h, rgba);
    }

    /// Replaces one terrain chunk's triangles (none = the chunk goes).
    pub fn set_terrain_chunk(
        &mut self,
        device: &ID3D11Device,
        key: [i32; 3],
        verts: &[crate::terrain::TerrainVertex],
    ) {
        // (the sun's cascades and the lights' shadow maps hold the terrain)
        self.static_gen += 1;
        if verts.is_empty() {
            self.terrain_chunks.remove(&key);
            return;
        }
        let (mut lo, mut hi) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
        for v in verts {
            lo = lo.min(Vec3::from(v.pos));
            hi = hi.max(Vec3::from(v.pos));
        }
        let vbuf = crate::d3d11::create_immutable_buffer(
            device,
            D3D11_BIND_VERTEX_BUFFER.0 as u32,
            bytemuck::cast_slice(verts),
        );
        self.terrain_chunks.insert(
            key,
            TerrainChunk {
                grass_ranges: Vec::new(),
                diagnostic_positions: if std::env::var_os("RUSTUDIO_CULL_EXPORT").is_some() {
                    verts.iter().map(|v| v.pos).collect()
                } else {
                    Vec::new()
                },
                vbuf,
                count: verts.len() as u32,
                lo,
                hi,
            },
        );
    }

    /// Drops every terrain chunk (another place, or no terrain).
    pub fn clear_terrain(&mut self) {
        self.terrain_chunks.clear();
        self.terrain_grass.clear();
        self.static_gen += 1;
    }

    /// Replaces one chunk's decoration grass (none = drop it).
    pub fn set_terrain_grass(
        &mut self,
        device: &ID3D11Device,
        key: [i32; 3],
        verts: &[crate::terrain::TerrainVertex],
    ) {
        let had = self.terrain_chunks.remove(&key);
        self.set_terrain_chunk(device, key, verts);
        match self.terrain_chunks.remove(&key) {
            Some(mut c) => {
                // |noise| <= 1; native default direction=.5, amplitude=1.0275.
                let wind = verts.iter().map(|v| 0.2 * v.wind_local[2]).fold(0.0_f32, f32::max);
                let pad = Vec3::new(wind, 0.0, wind);
                c.lo -= pad;
                c.hi += pad;
                self.terrain_grass.insert(key, c);
            }
            None => {
                self.terrain_grass.remove(&key);
            }
        }
        if let Some(h) = had {
            self.terrain_chunks.insert(key, h);
        }
    }

    pub fn set_terrain_grass_geometry(
        &mut self,
        device: &ID3D11Device,
        key: [i32; 3],
        geometry: crate::terrain::GrassGeometry,
    ) {
        let mut vertices = Vec::new();
        let mut ranges = Vec::new();
        for cell in geometry.cells {
            let start = vertices.len() as u32;
            let mut ordinals = Vec::new();
            for (ordinal, first) in cell.batch.blades {
                ordinals.push(ordinal);
                vertices.extend_from_slice(&geometry.vertices[first..first + 6]);
            }
            ranges.push(GrassRange {
                start,
                ordinals,
                allocated: cell.batch.allocated,
                near: cell.near,
                lo: cell.lo,
                hi: cell.hi,
            });
        }
        if ranges.is_empty() {
            vertices = geometry.vertices;
        }
        self.set_terrain_grass(device, key, &vertices);
        if let Some(chunk) = self.terrain_grass.get_mut(&key) {
            chunk.grass_ranges = ranges;
        }
    }

    /// Triangles in the drawn terrain chunks.
    pub fn terrain_triangles(&self) -> usize {
        self.terrain_chunks.values().map(|c| c.count as usize / 3).sum()
    }

    /// The terrain shader's per-palette parameters (4 vec4s per entry).
    pub fn set_terrain_params(&self, ctx: &ID3D11DeviceContext, params: &[[f32; 4]]) {
        let mut cpu = self.terrain_params_cpu.borrow_mut();
        let n = params.len().min(cpu.len());
        cpu[..n].copy_from_slice(&params[..n]);
        crate::d3d11::update_dynamic(ctx, &self.terrain_params_cb, bytemuck::cast_slice(&cpu));
    }

    /// The texture array layer of a material pack, uploading it (and
    /// growing the arrays) the first time. -1 while its maps are not in
    /// memory (released after another renderer's upload): they are asked
    /// for, and `matpack::generation` changes when they are back.
    pub fn material_layer(
        &mut self,
        device: &ID3D11Device,
        ctx: &ID3D11DeviceContext,
        maps: &std::sync::Arc<crate::matpack::Maps>,
    ) -> i32 {
        if let Some(l) = self.materials.layers.get(&maps.pack) {
            return *l as i32;
        }
        let Some(pixels) = maps.pixels() else {
            crate::matpack::request_pixels(maps);
            return -1;
        };
        let layer = self.materials.cpu.len() as u32;
        self.materials.cpu.push(maps.clone());
        self.materials.layers.insert(maps.pack, layer);
        if layer >= self.materials.capacity || self.materials.size != crate::matpack::LAYER_SIZE {
            let cap = (layer + 1).next_power_of_two().max(4);
            let params = std::mem::take(&mut self.materials.params);
            let mut grown = MaterialArrays::with_params(device, ctx, crate::matpack::LAYER_SIZE, cap, params);
            grown.layers = std::mem::take(&mut self.materials.layers);
            grown.cpu = std::mem::take(&mut self.materials.cpu);
            // The layers already there are copied on the GPU (their maps
            // are no longer in memory).
            grown.copy_layers(ctx, &self.materials, layer);
            self.materials = grown;
        }
        self.materials.upload(ctx, layer, &pixels);
        // On the GPU now: the 14 MB of maps go (see matpack::Maps).
        maps.release_pixels();
        layer as i32
    }

    /// Studio's material constants for the parts drawn with material layer
    /// `layer` (see `MaterialArrays`): written when they change.
    pub fn set_material_params(&mut self, ctx: &ID3D11DeviceContext, layer: i32, p: [[f32; 4]; 2]) {
        let Ok(l) = usize::try_from(layer) else {
            return;
        };
        if l >= MAT_PARAM_LAYERS {
            return;
        }
        let mut full = [p[0], p[1], [1.0, 1.0, 1.0, 0.0], [0.0; 4], [1.0, 1.0, 1.0, 0.0]];
        full[1][2] = 4.0;
        full[1][3] = 1.0;
        if let Some(c) = self.materials.cpu.get(l).and_then(|m| crate::matpack::full_consts(m.pack)) {
            full[1][2] = c[0];
            full[1][3] = c[16];
            full[2] = [c[6], c[7], c[8], c[17]];
            full[3] = [c[9], c[10], c[11], c[18]];
            full[4] = [c[13], c[14], c[15], c[19]];
        }
        if self.materials.params[l * 5..l * 5 + 5] == full {
            return;
        }
        self.materials.params[l * 5..l * 5 + 5].copy_from_slice(&full);
        crate::d3d11::update_dynamic(
            ctx,
            &self.materials.params_cb,
            bytemuck::cast_slice(&self.materials.params),
        );
    }

    /// Sets the skybox: six square RGBA8 faces of `size` in cube
    /// order (+X, −X, +Y, −Y, +Z, −Z). `size` 0 = no skybox.
    /// `mipped`: the six faces with their mip chains, layer-major (cube_mips).
    pub fn set_sky(
        &mut self,
        device: &ID3D11Device,
        _ctx: &ID3D11DeviceContext,
        size: u32,
        mipped: Option<&[u8]>,
    ) {
        match mipped {
            Some(f) if size > 0 => {
                self.sky_srv = Self::cube_view_mipped(device, size, f);
                self.sky_size = size;
            }
            _ => self.sky_size = 0,
        }
    }

    /// Uploads the light grid (lightgrid.rs: skylight and sun bytes, X, then Z, then Y).
    pub fn set_lightgrid(&mut self, ctx: &ID3D11DeviceContext, data: &[u8]) {
        let [nx, ny, nz] = crate::lightgrid::dims().map(|n| n as u32);
        if data.len() != (2 * nx * ny * nz) as usize {
            return;
        }
        unsafe {
            ctx.UpdateSubresource(
                &self.lightgrid_tex,
                0,
                None,
                data.as_ptr().cast(),
                2 * nx,
                2 * nx * nz,
            );
        }
    }

    pub fn set_localgrid(&mut self, ctx: &ID3D11DeviceContext, data: &[u8]) {
        let [nx, ny, nz] = crate::lightgrid::dims().map(|n| n as u32);
        if data.len() != (nx * ny * nz * 4) as usize {
            return;
        }
        unsafe {
            ctx.UpdateSubresource(
                &self.localgrid_tex,
                0,
                None,
                data.as_ptr().cast(),
                nx * 4,
                nx * nz * 4,
            );
        }
    }

    /// Uploads part of the local light channel (lightgrid::LocalPatch).
    pub fn set_localgrid_region(&mut self, ctx: &ID3D11DeviceContext, p: &crate::lightgrid::LocalPatch) {
        let size = [0, 1, 2].map(|k| (p.hi[k] - p.lo[k] + 1) as u32);
        if p.data.len() != (size[0] * size[1] * size[2] * 4) as usize {
            return;
        }
        let dst_box = D3D11_BOX {
            left: p.lo[0] as u32,
            top: p.lo[2] as u32,
            front: p.lo[1] as u32,
            right: p.hi[0] as u32 + 1,
            bottom: p.hi[2] as u32 + 1,
            back: p.hi[1] as u32 + 1,
        };
        unsafe {
            ctx.UpdateSubresource(
                &self.localgrid_tex,
                0,
                Some(&dst_box),
                p.data.as_ptr().cast(),
                size[0] * 4,
                size[0] * size[2] * 4,
            );
        }
    }

    /// Uploads the parts' light occupancy (lightocc.rs: X, then Y, then Z).
    pub fn set_lightocc(&mut self, ctx: &ID3D11DeviceContext, data: &[u8]) {
        let [nx, ny, nz] = crate::lightocc::dims().map(|n| n as u32);
        if data.len() != (nx * ny * nz) as usize {
            return;
        }
        unsafe {
            ctx.UpdateSubresource(
                &self.lightocc_tex,
                0,
                None,
                data.as_ptr().cast(),
                nx,
                nx * ny,
            );
        }
    }

    pub(super) fn make_buf(device: &ID3D11Device, size: usize) -> ID3D11Buffer {
        crate::d3d11::create_dynamic_buffer(
            device,
            D3D11_BIND_VERTEX_BUFFER.0 as u32,
            size.max(64),
        )
    }

    fn make_static_buf(device: &ID3D11Device, size: usize) -> ID3D11Buffer {
        crate::d3d11::create_default_buffer(
            device,
            D3D11_BIND_VERTEX_BUFFER.0 as u32,
            size.max(64),
            None,
        )
    }

    fn ensure_target(&mut self, rs: &crate::d3d11::RenderState, size: [u32; 2]) {
        let samples = self.quality.msaa_samples;
        if self.probe_samples != samples {
            self.probe.set_samples(&rs.device, samples);
            self.probe_samples = samples;
        }
        if self.target.as_ref().is_some_and(|t| t.size == size && t.samples == samples) {
            return;
        }
        let device = &rs.device;
        let (hdr_tex, hdr_rtv, hdr_srv) = Target::mk_rt(device, size[0], size[1], HDR_FORMAT, 1);
        let (msaa_tex, msaa_rtv, msaa_srv) = if samples > 1 {
            Target::mk_rt(device, size[0], size[1], HDR_FORMAT, samples)
        } else {
            (hdr_tex.clone(), hdr_rtv.clone(), hdr_srv.clone())
        };
        let (_see_tex, see_rtv, see_srv) = Target::mk_rt(device, size[0], size[1], HDR_FORMAT, samples);

        let depth_tex = crate::d3d11::create_tex2d(
            device,
            &crate::d3d11::Tex2dSpec {
                width: size[0],
                height: size[1],
                format: DXGI_FORMAT_R32_TYPELESS,
                samples,
                bind_flags: (D3D11_BIND_DEPTH_STENCIL | D3D11_BIND_SHADER_RESOURCE).0 as u32,
                ..Default::default()
            },
            None,
        );
        let depth_dsv = crate::d3d11::create_dsv(device, &depth_tex, DEPTH_FORMAT, samples > 1, false);
        let depth_dsv_ro = crate::d3d11::create_dsv(device, &depth_tex, DEPTH_FORMAT, samples > 1, true);
        let depth_srv = crate::d3d11::create_srv_2d(device, &depth_tex, DXGI_FORMAT_R32_FLOAT, samples > 1, 0, 1);
        let half_size = [size[0].div_ceil(2).max(1), size[1].div_ceil(2).max(1)];

        let (display_tex, display_rtv, display_srv) = Target::mk_rt(device, size[0], size[1], DISPLAY_FORMAT, 1);
        match self.texture_id {
            Some(id) => rs.update_native_texture(id, display_srv.clone(), size),
            None => self.texture_id = Some(rs.register_native_texture(display_srv.clone(), size)),
        }

        self.target = Some(Target {
            size,
            samples,
            half_size,
            msaa_tex,
            msaa_rtv,
            msaa_srv,
            see_rtv,
            see_srv,
            depth_dsv,
            depth_dsv_ro,
            depth_srv,
            hdr_tex,
            hdr_rtv,
            hdr_srv,
            scene_copy: None,
            water_depth_dsv: None,
            glass_depth: None,
            dof: None,
            near: None,
            bloom: None,
            ssao: None,
            display_tex,
            display_rtv,
            display_srv,
        });
    }

    fn bind_see_pipe(&self, ctx: &ID3D11DeviceContext, kind: SeePipeKind, encoded: bool) {
        let low_shaders = self.quality.level <= crate::mech::QUALITY_LOW_SHADER_MAX_LEVEL;
        let ps = if encoded {
            &self.ps_part_enc
        } else if low_shaders {
            &self.ps_part_low
        } else {
            &self.ps_part
        };
        let rs = match kind {
            SeePipeKind::TransparentCulled | SeePipeKind::GlassCulled => &self.rs_cull_back,
            _ => &self.rs_cull_none,
        };
        let ds = match kind {
            SeePipeKind::Glass | SeePipeKind::GlassCulled => &self.ds_greater_ro,
            _ => &self.ds_gequal_ro,
        };
        let bs = match kind {
            SeePipeKind::Glass | SeePipeKind::GlassCulled => &self.bs_opaque,
            _ => &self.bs_premul,
        };
        unsafe {
            ctx.IASetInputLayout(&self.il_part);
            ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            ctx.VSSetShader(&self.vs_part, None);
            ctx.PSSetShader(ps, None);
            ctx.PSSetSamplers(3, Some(&[Some(self.active_sampler().clone())]));
            ctx.RSSetState(rs);
            ctx.OMSetDepthStencilState(ds, 0);
            ctx.OMSetBlendState(bs, Some(&[0.0; 4]), 0xFFFFFFFF);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn run_post_pass(
        &self,
        ctx: &ID3D11DeviceContext,
        rtv: &ID3D11RenderTargetView,
        size: [u32; 2],
        clear: Option<[f32; 4]>,
        ps: &ID3D11PixelShader,
        srvs: [Option<&ID3D11ShaderResourceView>; 8],
        blend: &ID3D11BlendState,
    ) {
        crate::d3d11::unbind_ps_srvs(ctx, 15, 8);
        unsafe { ctx.OMSetRenderTargets(None, None) };
        if let Some(c) = clear {
            unsafe { ctx.ClearRenderTargetView(rtv, &c) };
        }
        unsafe { ctx.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None) };
        crate::d3d11::set_viewport(ctx, 0.0, 0.0, size[0] as f32, size[1] as f32);
        let views: [Option<ID3D11ShaderResourceView>; 8] = srvs.map(|s| s.cloned());
        unsafe {
            ctx.IASetInputLayout(None);
            ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            ctx.VSSetShader(&self.vs_post, None);
            ctx.PSSetShader(ps, None);
            ctx.PSSetSamplers(3, Some(&[Some(self.sky_sampler.clone())]));
            ctx.PSSetShaderResources(15, Some(&views));
            ctx.RSSetState(&self.rs_cull_none);
            ctx.OMSetDepthStencilState(&self.ds_none, 0);
            ctx.OMSetBlendState(blend, Some(&[0.0; 4]), 0xFFFFFFFF);
            ctx.Draw(3, 0);
        }
    }

    fn resolve_msaa(&self, ctx: &ID3D11DeviceContext, target: &Target) {
        unsafe { ctx.OMSetRenderTargets(None, None) };
        if target.samples > 1 {
            unsafe { ctx.ResolveSubresource(&target.hdr_tex, 0, &target.msaa_tex, 0, HDR_FORMAT) };
        }
    }

    fn resolve_glass_depth(&self, device: &ID3D11Device, ctx: &ID3D11DeviceContext, target: &mut Target) {
        target.ensure_glass_depth(device);
        let glass_rtv = target.glass_depth.as_ref().unwrap().0.clone();
        self.run_post_pass(
            ctx,
            &glass_rtv,
            target.size,
            Some([0.0; 4]),
            self.ps_glass_depth.get(target.samples > 1),
            [None, None, None, None, None, None, Some(&target.depth_srv), None],
            &self.bs_opaque,
        );
        crate::d3d11::unbind_ps_srvs(ctx, 15, 8);
        unsafe { ctx.OMSetRenderTargets(None, None) };
    }

    /// World GUIs and effects enter the HDR scene before bloom, DoF and
    /// colour correction (UILitFogFS / ParticleCustomFS). AlwaysOnTop and
    /// screen GUIs are composed separately by egui.
    pub fn draw_world_gui(
        &mut self,
        rs: &crate::d3d11::RenderState,
        batches: &[crate::world_gui::GuiBatch],
        fx: Option<&crate::world_gui::FxFrame>,
        tone_curve: Option<[f32; 5]>,
    ) {
        let Some(t) = self.target.as_ref() else { return };
        let pass = self
            .world_gui
            .get_or_insert_with(|| crate::world_gui::WorldGuiPass::new(&rs.device, &rs.context));
        pass.sun_atlas = self
            .sun_res
            .as_ref()
            .map_or_else(|| self.black_srv.clone(), |s| s.sun.atlas_srv.clone());
        pass.sun_grid = self.lightgrid_srv.clone();
        pass.lightgrid = self.gui_lightgrid;
        pass.sun_params = self.gui_sun_params;
        pass.sun_cascades = self.gui_sun;
        // (effects light themselves from the frame's local lights)
        pass.lights = self.lights_srv.clone();
        pass.n_lights = self.gui_n_lights;
        crate::d3d11::unbind_ps_srvs(&rs.context, 0, 32);
        unsafe { rs.context.OMSetRenderTargets(None, None) };
        if pass.effects(
            rs,
            &t.hdr_rtv,
            &t.hdr_srv,
            t.size,
            &t.depth_srv,
            t.samples > 1,
            batches,
            fx,
            tone_curve,
            self.gui_fog.0,
            self.gui_fog.1,
        ) && !self.post_deferred
        {
            self.finish_again(rs);
        }
    }

    /// Highlights over the finished image (highlight.rs): each one's parts
    /// as drawn this frame (their deformed / clothed meshes included).
    pub fn draw_highlights(
        &mut self,
        rs: &crate::d3d11::RenderState,
        list: &[crate::highlight::HighlightDraw],
    ) {
        if list.is_empty() {
            return;
        }
        let Some(t) = self.target.as_ref() else { return };
        let samples = t.samples;
        let pass = match &mut self.highlight {
            Some((s, p)) if *s == samples => p,
            slot => {
                *slot = Some((samples, crate::highlight::HighlightPass::new(&rs.device, samples)));
                &mut slot.as_mut().unwrap().1
            }
        };
        let mut work = Vec::new();
        for h in list.iter().take(crate::highlight::MAX) {
            let mut meshes = Vec::new();
            for r in &h.items {
                let Some(item) = self.items.get(r) else { continue };
                for d in &item.draws {
                    // (an invisible part, and decals: the part's own shape is enough)
                    if d.alpha <= 0.0 || d.tex_mode == TexMode::Decal {
                        continue;
                    }
                    let Some(m) = self.meshes.get(mesh_slot(d)) else { continue };
                    let raw = to_raw(d, 0.0);
                    let bias = item.bias;
                    meshes.push((
                        &m.vbuf,
                        m.count,
                        crate::highlight::Inst {
                            r0: raw.r0,
                            r1: raw.r1,
                            r2: raw.r2,
                            size: [raw.size[0], raw.size[1], raw.size[2], bias],
                        },
                    ));
                }
            }
            if !meshes.is_empty() {
                work.push((h, meshes));
            }
        }
        crate::d3d11::unbind_ps_srvs(&rs.context, 0, 32);
        unsafe { rs.context.OMSetRenderTargets(None, None) };
        pass.draw(
            &rs.device,
            &rs.context,
            t.size,
            samples,
            &t.depth_dsv_ro,
            &t.display_rtv,
            self.last_view_proj,
            &work,
        );
    }

    /// Runs the bloom downsample/upsample pyramid into `target.bloom`.
    fn encode_bloom(&self, device: &ID3D11Device, ctx: &ID3D11DeviceContext, target: &mut Target, size: f32) {
        let _ = target.ensure_bloom(device);
        let bloom = target.bloom.as_ref().unwrap();
        let scale = ((target.size[0] as f32 * target.size[1] as f32) / 2073600.0).sqrt();
        let levels = ((3.0 + (scale.log2() + (size / 7.0).log2().max(0.0)).max(-1.0)).min(5.999) as usize).clamp(1, 5);
        for i in 0..levels {
            let src = if i == 0 { &target.hdr_srv } else { &bloom.srvs[i - 1] };
            let ps = if i == 0 { &self.ps_bloom_bright } else { &self.ps_bloom_down };
            self.run_post_pass(
                ctx,
                &bloom.rtvs[i],
                bloom.sizes[i],
                Some([0.0; 4]),
                ps,
                [Some(src), None, None, None, None, None, None, None],
                &self.bs_opaque,
            );
            if i == 0 {
                if let Some(capture) = &bloom.bright_capture {
                    unsafe {
                        ctx.OMSetRenderTargets(None, None);
                        ctx.CopyResource(capture, &bloom.textures[0]);
                    }
                }
            }
        }
        for i in (1..levels).rev() {
            self.run_post_pass(
                ctx,
                &bloom.rtvs[i - 1],
                bloom.sizes[i - 1],
                None,
                &self.ps_bloom_up,
                [Some(&bloom.srvs[i]), None, None, None, None, None, None, None],
                &self.bs_bloom_up,
            );
        }
    }

    fn run_dof_and_post(&self, device: &ID3D11Device, ctx: &ID3D11DeviceContext, target: &mut Target) {
        let msaa = target.samples > 1;
        if self.post_dof {
            let _ = target.ensure_dof(device);
            if self.post_dof_near {
                let _ = target.ensure_near(device);
            }
            let dof = target.dof.as_ref().unwrap();
            self.run_post_pass(
                ctx,
                &dof.rtvs[0],
                target.size,
                Some([0.0; 4]),
                self.ps_dof_h.get(msaa),
                [Some(&target.hdr_srv), None, Some(&target.depth_srv), None, None, None, None, None],
                &self.bs_opaque,
            );
            self.run_post_pass(
                ctx,
                &dof.rtvs[1],
                target.size,
                Some([0.0; 4]),
                self.ps_dof_v.get(msaa),
                [Some(&dof.srvs[0]), None, Some(&target.depth_srv), None, None, None, None, None],
                &self.bs_opaque,
            );
            if self.post_dof_near {
                let near = target.near.as_ref().unwrap();
                for i in 0..11usize {
                    let ps = match i {
                        0 => self.ps_near_h.get(msaa),
                        1 => self.ps_near_v.get(msaa),
                        2..=7 => &self.ps_near_masks[i - 2],
                        8 => &self.ps_near_edge,
                        9 => &self.ps_near_bokeh,
                        _ => &self.ps_near_tent,
                    };
                    let t15 = if i == 0 {
                        &dof.srvs[1]
                    } else if i == 8 {
                        &near.srvs[1]
                    } else {
                        &near.srvs[i - 1]
                    };
                    let t16 = if i == 8 || i == 9 { Some(&near.srvs[7]) } else { None };
                    let t17 = if i <= 1 { Some(&target.depth_srv) } else { None };
                    self.run_post_pass(
                        ctx,
                        &near.rtvs[i],
                        near.sizes[i],
                        Some([0.0; 4]),
                        ps,
                        [Some(t15), t16, t17, None, None, None, None, None],
                        &self.bs_opaque,
                    );
                }
            }
        }
        crate::gpu_timing::mark(ctx, "gpu: dof");
        let bloom_srv = if self.quality.enable_bloom {
            target.bloom.as_ref().map_or(&self.black_srv, |b| &b.srvs[0])
        } else {
            &self.black_srv
        };
        let scene_copy_srv = target.scene_copy.as_ref().map_or(&self.black_srv, |s| &s.1);
        let dof_srv = if self.post_dof {
            target.dof.as_ref().map_or(&self.black_srv, |d| &d.srvs[1])
        } else {
            &self.black_srv
        };
        let near_srv = if self.post_dof_near {
            target.near.as_ref().map_or(&self.black_srv, |n| &n.srvs[10])
        } else {
            &self.black_srv
        };
        self.run_post_pass(
            ctx,
            &target.display_rtv,
            target.size,
            Some([0.0; 4]),
            self.ps_post.get(msaa),
            [
                Some(&target.hdr_srv),
                Some(bloom_srv),
                Some(&target.depth_srv),
                None,
                None,
                Some(scene_copy_srv),
                Some(dof_srv),
                Some(near_srv),
            ],
            &self.bs_opaque,
        );
        crate::d3d11::unbind_ps_srvs(ctx, 15, 8);
        unsafe { ctx.OMSetRenderTargets(None, None) };
    }

    fn run_bloom_dof_post(&self, device: &ID3D11Device, ctx: &ID3D11DeviceContext, target: &mut Target) {
        if self.quality.enable_bloom {
            self.encode_bloom(device, ctx, target, self.post_bloom_size);
        }
        crate::gpu_timing::mark(ctx, "gpu: bloom");
        self.run_dof_and_post(device, ctx, target);
        crate::gpu_timing::mark(ctx, "gpu: post");
    }

    pub(super) fn finish_again(&mut self, rs: &crate::d3d11::RenderState) {
        let _b = crate::bench::scope("fx_post_again");
        let Some(mut target) = self.target.take() else { return };
        let ctx = &rs.context;
        self.bind_scene_globals_with(ctx, &self.globals_cb);
        self.run_bloom_dof_post(&rs.device, ctx, &mut target);
        self.target = Some(target);
    }

    #[allow(dead_code)] // Called by viewport.rs when frame.defer_post is true.
    pub fn finalize_post(&mut self, rs: &crate::d3d11::RenderState) {
        if !self.post_deferred {
            return;
        }
        self.post_deferred = false;
        let Some(mut target) = self.target.take() else { return };
        let ctx = &rs.context;
        self.bind_scene_globals_with(ctx, &self.globals_cb);
        self.run_bloom_dof_post(&rs.device, ctx, &mut target);
        self.target = Some(target);
    }

    /// The last frame's fog (`Globals::fog`) and its linear colour, as
    /// fog_rgb works it out (world GUIs are fogged the same).
    fn gui_fog(g: &Globals) -> ([f32; 4], [f32; 3]) {
        let c = [g.fog_color[0], g.fog_color[1], g.fog_color[2]];
        if g.fog[2] > 1.5 && g.fog[2] < 2.5 {
            let k = |i: usize| g.ambient[i] + g.outdoor[i] + g.sun_color[i] * g.sun_dir[3] * g.sun_dir[1].max(0.0);
            return (g.fog, [c[0] * k(0), c[1] * k(1), c[2] * k(2)]);
        }
        if g.fog[2] > 2.5 {
            // Atmosphere: the haze band at the horizon (the sky behind a
            // world GUI is close to it at the distances fog shows).
            return (g.fog, [g.atmos[0], g.atmos[1], g.atmos[2]]);
        }
        (g.fog, c)
    }

    /// Renders a frame at `size` physical pixels; returns the texture to show.
    pub fn render(
        &mut self,
        rs: &crate::d3d11::RenderState,
        size: [u32; 2],
        frame: &Frame,
    ) -> Option<egui::TextureId> {
        let svc_t0 = Instant::now();
        let size = [size[0].clamp(1, 8192), size[1].clamp(1, 8192)];
        self.ensure_target(rs, size);
        let device = &rs.device;
        let ctx = &rs.context;

        let shadows_on = frame.shadows && self.quality.enable_shadows;
        let active_cascades = if shadows_on {
            self.quality.shadow_cascades.min(crate::sun_shadow::CASCADES)
        } else {
            0
        };
        let max_local_shadows = if shadows_on {
            self.quality.max_local_shadows.min(crate::mech::LSHADOW_MAX_LIGHTS)
        } else {
            0
        };

        // cam_pos.w is unused by the camera math. Keep material animation
        // continuous across scene rebuilds, independently of lighting time.
        let mut globals = frame.globals;
        self.last_view_proj = Mat4::from_cols_array_2d(&frame.globals.view_proj);
        if std::env::var_os("RUSTUDIO_RENDER_DIAGNOSTICS").is_some() {
            static LAST: std::sync::Mutex<Option<[f32; 4]>> = std::sync::Mutex::new(None);
            let state = [globals.cc_tint[3], globals.bloom[1], globals.bloom[0], globals.params[1]];
            let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
            if *last != Some(state) {
                eprintln!(
                    "BLOOM intensity={} size={} threshold={} exposure={} dof={:?} pipeline=bright/down/up/composite-before-tone",
                    state[0], state[1], state[2], state[3], globals.dof
                );
                *last = Some(state);
            }
        }
        // Scene environment probe (env_probe.rs): swap the slots when the
        // blend completed; CB0[32].w is the blend toward the next slot.
        globals.env_spec[3] = 0.0;
        if let Some(pf) = frame.probe.as_ref() {
            if pf.swap {
                self.probe.cur = self.probe.next();
            }
            globals.env_spec[3] = pf.blend;
        }
        let low_shaders = self.quality.level <= crate::mech::QUALITY_LOW_SHADER_MAX_LEVEL;
        if low_shaders {
            globals.env_spec = [0.0; 4];
            globals.lightocc[3] = 0.0;
        } else if self.quality.level < crate::mech::QUALITY_LIGHTOCC_MIN_LEVEL {
            globals.lightocc[3] = 0.0;
        }
        if !shadows_on {
            globals.bloom[2] = 0.0;
        }
        for ci in active_cascades..crate::sun_shadow::CASCADES {
            globals.sun_casc[ci] = [[0.0; 4]; 4];
            globals.sun_sphere[ci][3] = 0.0;
            globals.sun_rect[ci] = [0.0; 4];
        }
        let active_sun_dist = if active_cascades > 0 && active_cascades < crate::sun_shadow::CASCADES {
            crate::mech::SUN_CASCADE_SPLITS[active_cascades.saturating_sub(1)].min(crate::mech::SUN_DIST)
        } else {
            crate::mech::SUN_DIST
        };
        if active_cascades > 0 && active_cascades < crate::sun_shadow::CASCADES {
            globals.bloom[3] = active_sun_dist;
        } else {
            globals.bloom[3] = 0.0;
        }
        if !self.quality.enable_ssao {
            globals.ssao[3] = 0.0;
        }
        self.post_bloom_size = globals.bloom[1];
        if !self.quality.enable_bloom {
            globals.cc_tint[3] = 0.0;
        }
        // NearIntensity below 0.22 takes the cheap full-resolution pass
        // (fs_dof_v), not the near chain (DOF_CHEAP_BELOW).
        self.post_dof = self.quality.enable_dof && (globals.dof[2] > 0.0 || globals.dof[3] > 0.0);
        self.post_dof_near = self.post_dof && globals.dof[2] >= DOF_CHEAP_BELOW;
        if !self.quality.enable_dof {
            globals.dof = [0.0; 4];
        }
        globals.cam_pos[3] = self.material_epoch.elapsed().as_secs_f32();
        // Deterministic isolated render probes (no effect in ordinary runs).
        if let Ok(t) = std::env::var("RUSTUDIO_MATERIAL_TIME") {
            if let Ok(t) = t.parse::<f32>() {
                globals.cam_pos[3] = t;
            }
        }
        if let Some(n) = crate::grass_temporal::index() {
            globals.cam_pos[3] = crate::grass_temporal::time(n);
            use std::io::Write;
            let dir = std::env::var("RUSTUDIO_GRASS_TEMPORAL").unwrap();
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(format!("{dir}/frames.jsonl"))
                .unwrap();
            writeln!(
                f,
                "{}",
                serde_json::json!({
                    "frame": n,
                    "shader_time": globals.cam_pos[3],
                    "camera": globals.cam_pos,
                    "spheres": globals.sun_sphere,
                    "matrices": globals.sun_casc,
                    "rects": globals.sun_rect,
                    "viewport": size,
                    "wall": self.material_epoch.elapsed().as_secs_f64()
                })
            )
            .unwrap();
        }
        crate::d3d11::upload_cb(ctx, &self.globals_cb, &globals);
        self.vf.base = Some(globals);
        self.gui_fog = Self::gui_fog(&globals);
        self.gui_lightgrid = globals.lightgrid;
        self.gui_sun_params = [
            if globals.sun_color[3] < 0.5 { 1.0 } else { 0.0 },
            active_sun_dist,
            crate::mech::HYBRID_LERP_SLOPE,
            if shadows_on { 1.0 } else { 0.0 },
        ];
        self.gui_sun = crate::world_gui::SunCascades {
            local: globals.sun_casc,
            sphere: globals.sun_sphere,
            rect: globals.sun_rect,
        };
        let mut stats = RenderStats {
            rebuilds: self.stats.rebuilds,
            ..Default::default()
        };

        self.requeue_quiet();
        // New parts join the static batches once they've settled.
        let settled = self
            .pending_at
            .is_none_or(|t| t.elapsed().as_secs_f32() >= crate::mech::STATIC_SETTLE_S);
        let compact = self.st.hidden > 1024 && self.st.hidden * 4 > self.st.raw.len();
        if (self.pending > 0
            && (self.st.batches.is_empty()
                || ((settled || self.pending > crate::mech::PENDING_MAX)
                    && self.last_rebuild.elapsed().as_secs_f32() >= crate::mech::STATIC_REBUILD_MIN_S)))
            || compact
        {
            self.last_rebuild = Instant::now();
            let _p = crate::bench::scope("render_static_rebuild");
            self.rebuild_static(device, ctx);
            stats.rebuilds += 1;
        }
        self.flush_static_writes(ctx);

        let cam = Vec3::new(
            frame.globals.cam_pos[0],
            frame.globals.cam_pos[1],
            frame.globals.cam_pos[2],
        );
        let planes = frustum(frame.cull_view_proj);
        // A part whose bounding radius covers less than LOD_MIN_PX pixels is skipped.
        let focal = size[1] as f32 * 0.5 / (frame.fov_y * 0.5).tan().max(1e-3);
        let lod_k = crate::mech::LOD_MIN_PX * self.quality.lod_bias.max(0.5) / focal;

        // Static batches: frustum test per cell, then trim the smallest parts.
        let mut static_ranges: Vec<(Pass, (usize, usize), u32, u32)> = Vec::new();
        let mut see_bounds = SeeBounds::new(Mat4::from_cols_array_2d(&frame.globals.view_proj), size);
        for b in &self.st.batches {
            if !box_visible(&planes, b.lo, b.hi) {
                stats.culled_batches += 1;
                continue;
            }
            let r_min = lod_k * box_distance(cam, b.lo, b.hi);
            let radii = &self.st.radii[b.start as usize..(b.start + b.count) as usize];
            let n = radii.partition_point(|r| *r >= r_min) as u32;
            stats.lod_skipped += (b.count - n) as usize;
            if n == 0 {
                continue;
            }
            if b.pass == Pass::Decal {
                see_bounds.add_box(b.lo, b.hi);
            }
            match static_ranges.last_mut() {
                Some(last) if last.0 == b.pass && last.1 == b.key && last.3 == b.start => {
                    last.3 = b.start + n
                }
                _ => static_ranges.push((b.pass, b.key, b.start, b.start + n)),
            }
        }

        // Per-frame items: moving, new and see-through parts.
        let mut dyn_opaque: Vec<((usize, usize), InstanceRaw)> = Vec::new();
        let mut dyn_decal: Vec<((usize, usize), InstanceRaw)> = Vec::new();
        let mut blended: Vec<(f32, (usize, usize), InstanceRaw)> = Vec::new();
        // Decals / Textures on a solid (or invisible) host: part of the
        // surface, so drawn with the opaque scene, before terrain water and
        // glass read it. Studio's water covers a hull's Textures below the
        // waterline (Pirate Island's Cutter); after the water they showed
        // through it. Decals on see-through hosts stay sorted with them.
        let mut surface_decals: Vec<(f32, (usize, usize), InstanceRaw)> = Vec::new();
        let mut water: Vec<((usize, usize), InstanceRaw)> = Vec::new();
        for r in &self.per_frame {
            let Some(item) = self.items.get(r) else {
                continue;
            };
            let solid_host = item.draws.first().is_none_or(|h| {
                h.tex_mode != TexMode::Decal
                    && (!shows(h) || (h.alpha >= 0.999 && h.material_id != 1584))
            });
            for d in &item.draws {
                let rad = radius_of(d);
                if !shows(d) || !sphere_visible(&planes, d.xf.pos, rad) {
                    continue;
                }
                let dist = (d.xf.pos - cam).length();
                if rad < lod_k * (dist - rad).max(0.0) {
                    stats.lod_skipped += 1;
                    continue;
                }
                let raw = to_raw(d, item.bias);
                // (a skinned mesh's vertices can leave its box)
                let see_bound = |b: &mut SeeBounds| {
                    if self.meshes.get(mesh_slot(d)).is_some_and(|m| m.storage) {
                        b.set_full();
                    }
                    b.add_sphere(d.xf.pos, rad);
                };
                match pass_of(d) {
                    Some(Pass::Opaque) => dyn_opaque.push((draw_key(d), raw)),
                    Some(Pass::Decal) => {
                        see_bound(&mut see_bounds);
                        dyn_decal.push((draw_key(d), raw))
                    }
                    // A see-through union is drawn two-sided (its winding
                    // isn't trusted): from inside its box every face Studio
                    // would draw faces away, so nothing shows (Place B's
                    // camera inside a Neon union). File meshes have known
                    // sides: Studio draws a translucent sheet's front from
                    // inside its box and culls its back (both sides when
                    // DoubleSided; checks/translucent-inside), as the GPU
                    // does here. (Not terrain water, material 5, nor
                    // decals: their boxes span the map / the host.)
                    None if d.custom_mesh.is_some()
                        && !self.culling_known(mesh_slot(d))
                        && d.material != 5.0
                        && d.tex_mode != TexMode::Decal
                        && camera_inside(d, cam) => {}
                    // Terrain water: its own pass (water.rs).
                    None if d.custom_mesh.is_some() && d.material == 5.0 => {
                        water.push((draw_key(d), raw))
                    }
                    None if solid_host && d.tex_mode == TexMode::Decal && d.texture.is_some() => {
                        see_bound(&mut see_bounds);
                        surface_decals.push((dist, draw_key(d), raw))
                    }
                    None => {
                        see_bound(&mut see_bounds);
                        blended.push((dist, draw_key(d), raw))
                    }
                }
            }
        }
        dyn_opaque.sort_by_key(|e| e.0);
        dyn_decal.sort_by_key(|e| e.0);
        // Glass samples the opaque scene and must precede other transparents.
        // Preserve the existing back-to-front distance order within each group.
        blended.sort_by(|a, b| transparent_order(a.2.extra[0], a.0, b.2.extra[0], b.0));
        // Back to front; equal distances keep source order (stable sort).
        surface_decals.sort_by(|a, b| b.0.total_cmp(&a.0));
        for d in frame.overlay.iter().filter(|d| shows(d)) {
            if self.meshes.get(mesh_slot(d)).is_some_and(|m| m.storage) {
                see_bounds.set_full();
            }
            see_bounds.add_sphere(d.xf.pos, radius_of(d));
        }
        let see_rects = see_bounds.rects();
        let overlay: Vec<((usize, usize), InstanceRaw)> = frame
            .overlay
            .iter()
            .filter(|d| shows(d))
            .map(|d| (draw_key(d), to_raw(&PartDraw { hover: true, ..*d }, 0.0)))
            .collect();
        let raw: Vec<InstanceRaw> = dyn_opaque
            .iter()
            .map(|e| e.1)
            .chain(dyn_decal.iter().map(|e| e.1))
            .chain(surface_decals.iter().map(|e| e.2))
            .chain(blended.iter().map(|e| e.2))
            .chain(overlay.iter().map(|e| e.1))
            .chain(water.iter().map(|e| e.1))
            .collect();
        if raw.len() > self.instance_cap {
            self.instance_cap = raw.len().next_power_of_two();
            self.instance_buf = Self::make_buf(
                device,
                self.instance_cap * std::mem::size_of::<InstanceRaw>(),
            );
        }
        if !raw.is_empty() {
            crate::d3d11::update_dynamic(ctx, &self.instance_buf, bytemuck::cast_slice(&raw));
        }
        // Runs of equal keys in a per-frame list → (key, start, end) in `raw`.
        let runs = |keys: &mut dyn Iterator<Item = (usize, usize)>,
                    base: u32|
         -> Vec<((usize, usize), u32, u32)> {
            let mut out: Vec<((usize, usize), u32, u32)> = Vec::new();
            for (i, k) in keys.enumerate() {
                let at = base + i as u32;
                match out.last_mut() {
                    Some(l) if l.0 == k && l.2 == at => l.2 = at + 1,
                    _ => out.push((k, at, at + 1)),
                }
            }
            out
        };
        let mut base = 0u32;
        let opaque_runs = runs(&mut dyn_opaque.iter().map(|e| e.0), base);
        base += dyn_opaque.len() as u32;
        let decal_runs = runs(&mut dyn_decal.iter().map(|e| e.0), base);
        base += dyn_decal.len() as u32;
        let surface_decal_runs = runs(&mut surface_decals.iter().map(|e| e.1), base);
        base += surface_decals.len() as u32;
        let glass_count = blended.iter().take_while(|e| (e.2.extra[0] as u32 & 7) == 3).count();
        let glass_end = base + glass_count as u32;
        let mut blend_runs = runs(&mut blended[..glass_count].iter().map(|e| e.1), base);
        blend_runs.extend(runs(&mut blended[glass_count..].iter().map(|e| e.1), glass_end));
        base += blended.len() as u32;
        let overlay_runs = runs(&mut overlay.iter().map(|e| e.0), base);
        base += overlay.len() as u32;
        let water_runs = runs(&mut water.iter().map(|e| e.0), base);
        stats.items = self.items.len();
        stats.static_instances = self.st.raw.len() - self.st.hidden;
        stats.batches = self.st.batches.len();
        stats.per_frame_items = self.per_frame.len();
        stats.drawn_instances = static_ranges
            .iter()
            .map(|r| (r.3 - r.2) as usize)
            .sum::<usize>()
            + raw.len();
        if frame.lines.len() > self.line_cap {
            self.line_cap = frame.lines.len().next_power_of_two();
            self.line_buf = Self::make_buf(
                device,
                self.line_cap * std::mem::size_of::<LineVertex>(),
            );
        }
        if !frame.lines.is_empty() {
            crate::d3d11::update_dynamic(ctx, &self.line_buf, bytemuck::cast_slice(frame.lines));
        }

        let n_lights = frame.lights.len().min(crate::mech::MAX_LIGHTS);
        self.gui_n_lights = n_lights as u32;
        let mut lights_upload: Vec<LightRaw> = frame.lights[..n_lights].to_vec();
        if max_local_shadows < crate::mech::LSHADOW_MAX_LIGHTS {
            for l in &mut lights_upload {
                if l.extra[3] >= max_local_shadows as f32 {
                    l.extra[3] = -1.0;
                }
            }
        }
        if !lights_upload.is_empty() {
            crate::d3d11::update_dynamic(ctx, &self.lights_buf, bytemuck::cast_slice(&lights_upload));
        }

        // The sun's shadow cascades (sun_shadow.rs): which to redraw.
        let diagnostic_back = std::env::var_os("RUSTUDIO_CULL_BACK").is_some()
            || std::env::var_os("RUSTUDIO_CULL_SWITCH").is_some_and(|p| std::path::Path::new(&p).exists());
        let scene_gen = self.static_gen.wrapping_add(if diagnostic_back { 1u64 << 63 } else { 0 });
        static GRASS_SHADOWS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let grass_shadows = *GRASS_SHADOWS.get_or_init(|| std::env::var_os("RUSTUDIO_NO_GRASS_SHADOW").is_none());
        let shadow_side = self.quality.shadow_atlas_side;
        if shadows_on && active_cascades > 0 {
            if self.sun_res.as_ref().is_none_or(|s| s.side != shadow_side) {
                self.sun_res = Some(SunShadowRes::new(device, shadow_side));
                self.last_cascades = None;
            }
        } else {
            self.sun_res = None;
            self.last_cascades = None;
        }
        if shadows_on && max_local_shadows > 0 {
            if self.lshadow.as_ref().is_none_or(|l| l.side != shadow_side) {
                self.lshadow = Some(LightShadows::new(device, shadow_side));
            }
        } else {
            self.lshadow = None;
        }

        let sun_due = if let Some(sun_res) = self.sun_res.as_mut() {
            if self.last_cascades != Some(active_cascades) {
                sun_res.sun.clear_atlas(ctx);
                self.last_cascades = Some(active_cascades);
            }
            sun_res.sun.upload(ctx, &frame.sun_cascades);
            sun_res.sun.plan(
                &frame.sun_cascades,
                scene_gen,
                !opaque_runs.is_empty() || !self.terrain_grass.is_empty(),
            )
        } else {
            [false; crate::sun_shadow::CASCADES]
        };

        let mut target = self.target.take()?;
        let msaa = target.samples > 1;
        let part_stride = std::mem::size_of::<Vertex>() as u32;
        let terrain_stride = std::mem::size_of::<crate::terrain::TerrainVertex>() as u32;
        let line_stride = std::mem::size_of::<LineVertex>() as u32;

        crate::gpu_timing::begin(device, ctx);
        // Skinned parts' vertices for this frame, before anything draws them.
        self.skin.encode(ctx);
        crate::gpu_timing::mark(ctx, "gpu: skinning");

        let clouds_active = self.quality.enable_clouds && frame.sky;
        match frame.clouds.as_ref().filter(|_| clouds_active) {
            Some(look) => {
                self.bind_scene_globals_with(ctx, &self.globals_cb);
                let clouds = self
                    .clouds
                    .get_or_insert_with(|| crate::clouds::CloudsGpu::new(device, ctx));
                clouds.encode(
                    device,
                    ctx,
                    look,
                    -globals.sun_dir[1],
                    target.size,
                    &target.depth_srv,
                    msaa,
                );
                unbind_rtv_srvs(ctx);
                unsafe { ctx.OMSetRenderTargets(None, None) };
            }
            None => {
                if let Some(c) = self.clouds.as_mut() {
                    c.idle();
                }
            }
        }
        crate::gpu_timing::mark(ctx, "gpu: clouds");

        // Sun shadow cascades (0..active_cascades).
        if let Some(sun_res) = self.sun_res.as_ref() {
            for (ci, cascade) in frame.sun_cascades.iter().enumerate().take(active_cascades) {
                if !sun_due[ci] {
                    continue;
                }
                let sp = frustum(cascade.caster_view_proj());
                let mut shadow_ranges: Vec<((usize, usize), u32, u32)> = Vec::new();
                for b in self.st.batches.iter().filter(|b| b.pass == Pass::Opaque) {
                    if !box_visible(&sp, b.lo, b.hi) {
                        continue;
                    }
                    let r_min = lod_k * box_distance(cam, b.lo, b.hi) * crate::mech::SHADOW_LOD_SCALE;
                    let radii = &self.st.radii[b.start as usize..(b.start + b.count) as usize];
                    let n = radii.partition_point(|r| *r >= r_min) as u32;
                    if n == 0 {
                        continue;
                    }
                    match shadow_ranges.last_mut() {
                        Some(last) if last.0 == b.key && last.2 == b.start => last.2 = b.start + n,
                        _ => shadow_ranges.push((b.key, b.start, b.start + n)),
                    }
                }

                unbind_rtv_srvs(ctx);
                let sc = sun_res.sun.scale_cascade(cascade);
                let side = sc.tile.min(sun_res.side);
                unsafe {
                    ctx.ClearDepthStencilView(&sun_res.depth_dsv, D3D11_CLEAR_DEPTH.0 as u32, 1.0, 0);
                    ctx.OMSetRenderTargets(None, Some(&sun_res.depth_dsv));
                }
                crate::d3d11::set_viewport(ctx, 0.0, 0.0, side as f32, side as f32);
                crate::d3d11::set_scissor(ctx, 0, 0, side as i32, side as i32);
                unsafe {
                    ctx.IASetInputLayout(&self.il_part);
                    ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
                    ctx.VSSetShader(&self.vs_shadow, None);
                    ctx.PSSetShader(None, None);
                    ctx.VSSetConstantBuffers(0, Some(&[Some(self.globals_cb.clone())]));
                    ctx.PSSetConstantBuffers(0, Some(&[Some(self.globals_cb.clone())]));
                    ctx.RSSetState(&self.rs_scissor_none);
                    ctx.OMSetDepthStencilState(&self.ds_less_eq_rw, 0);
                    ctx.OMSetBlendState(&self.bs_no_color, Some(&[0.0; 4]), 0xFFFFFFFF);
                }
                sun_res.sun.bind(ctx, ci);

                let cast = |key: (usize, usize), range: std::ops::Range<u32>| {
                    if let Some(mesh) = self.meshes.get(key.0) {
                        crate::runtime::domains::services::perf::draw(
                            crate::runtime::domains::services::perf::SHADOW,
                            1,
                            (mesh.count / 3) as u64 * (range.end - range.start) as u64,
                        );
                        unsafe {
                            ctx.IASetVertexBuffers(0, 1, Some(&Some(mesh.vbuf.clone())), Some(&part_stride), Some(&0));
                            ctx.DrawInstanced(mesh.count, range.end - range.start, 0, range.start);
                        }
                    }
                };
                if !shadow_ranges.is_empty() {
                    Self::bind_instance_buffer(ctx, &self.static_buf);
                    for r in &shadow_ranges {
                        cast(r.0, r.1..r.2);
                    }
                }
                if !opaque_runs.is_empty() {
                    Self::bind_instance_buffer(ctx, &self.instance_buf);
                    for r in &opaque_runs {
                        cast(r.0, r.1..r.2);
                    }
                }
                if !self.terrain_chunks.is_empty() || !self.terrain_grass.is_empty() {
                    unsafe {
                        ctx.IASetInputLayout(&self.il_terrain);
                        ctx.VSSetShader(&self.vs_terrain_shadow, None);
                    }
                    if let Some(dir) = std::env::var_os("RUSTUDIO_CULL_EXPORT") {
                        let dir = std::path::PathBuf::from(dir);
                        std::fs::create_dir_all(&dir).unwrap();
                        let vp = cascade.view_proj();
                        let mut clip = Vec::<[f32; 4]>::new();
                        let mut chunks = Vec::new();
                        for (key, c) in &self.terrain_chunks {
                            if box_visible(&sp, c.lo, c.hi) {
                                chunks.push((*key, c.count));
                                clip.extend(c.diagnostic_positions.iter().map(|p| (vp * Vec3::from(*p).extend(1.0)).to_array()));
                            }
                        }
                        std::fs::write(dir.join(format!("cascade{ci}-clip.bin")), bytemuck::cast_slice(&clip)).unwrap();
                        std::fs::write(
                            dir.join(format!("cascade{ci}.json")),
                            serde_json::to_vec_pretty(&serde_json::json!({
                                "vp": vp.to_cols_array(),
                                "tile": cascade.tile,
                                "chunks": chunks,
                                "vertices": clip.len()
                            }))
                            .unwrap(),
                        )
                        .unwrap();
                    }
                    for (solid, c) in self
                        .terrain_chunks
                        .values()
                        .map(|c| (true, c))
                        .chain(self.terrain_grass.values().filter(|_| grass_shadows).map(|c| (false, c)))
                    {
                        unsafe {
                            ctx.RSSetState(if solid && diagnostic_back {
                                &self.rs_scissor_back
                            } else {
                                &self.rs_scissor_none
                            });
                        }
                        if box_visible(&sp, c.lo, c.hi) {
                            crate::runtime::domains::services::perf::draw(
                                crate::runtime::domains::services::perf::SHADOW,
                                1,
                                (c.count / 3) as u64,
                            );
                            unsafe {
                                ctx.IASetVertexBuffers(0, 1, Some(&Some(c.vbuf.clone())), Some(&terrain_stride), Some(&0));
                                if c.grass_ranges.is_empty() {
                                    ctx.Draw(c.count, 0);
                                } else {
                                    let camera = Vec3::new(
                                        frame.globals.cam_pos[0],
                                        frame.globals.cam_pos[1],
                                        frame.globals.cam_pos[2],
                                    );
                                    for range in &c.grass_ranges {
                                        ctx.Draw(range.count(camera), range.start);
                                    }
                                }
                            }
                        }
                    }
                }
                unsafe { ctx.OMSetRenderTargets(None, None) };
                sun_res.sun.blur(ctx, cascade, ci);
            }
        }
        crate::gpu_timing::mark(ctx, "gpu: sun shadows");

        // Local light shadow maps (Studio's Future path).
        if let Some(lshadow) = self.lshadow.as_mut() {
            let slots: Vec<(usize, Vec3, f32)> = lights_upload
                .iter()
                .filter(|l| l.extra[3] >= 0.0)
                .map(|l| (l.extra[3] as usize, Vec3::new(l.pos_range[0], l.pos_range[1], l.pos_range[2]), l.pos_range[3]))
                .filter(|s| s.0 < max_local_shadows)
                .collect();
            let key = {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                self.static_gen.hash(&mut h);
                for (i, p, r) in &slots {
                    i.hash(&mut h);
                    p.to_array().map(f32::to_bits).hash(&mut h);
                    r.to_bits().hash(&mut h);
                }
                h.finish()
            };
            if !slots.is_empty() && lshadow.key != Some(key) {
                lshadow.key = Some(key);
                let n = crate::mech::LSHADOW_MAX_LIGHTS;
                let mut slot_data = vec![0f32; n * 100];
                for (slot, pos, range) in &slots {
                    let inv = 1.0 / range.max(1e-3);
                    for (f, (vp, _)) in LightShadows::faces(*pos, *range).iter().enumerate() {
                        slot_data[slot * 100 + f * 16..slot * 100 + f * 16 + 16].copy_from_slice(&vp.to_cols_array());
                    }
                    slot_data[slot * 100 + 96..slot * 100 + 100].copy_from_slice(&[pos.x, pos.y, pos.z, inv]);
                }
                crate::d3d11::update_dynamic(ctx, &lshadow.slots_buf, bytemuck::cast_slice(&slot_data));

                let far_p = crate::mech::EVSM_POS_EXP.exp();
                let far_n = -(-crate::mech::EVSM_NEG_EXP).exp();
                let clear_col = [far_p, far_p * far_p, far_n, far_n * far_n];
                unbind_rtv_srvs(ctx);
                unsafe {
                    ctx.ClearRenderTargetView(&lshadow.moments_rtv, &clear_col);
                    ctx.ClearDepthStencilView(&lshadow.depth_dsv, D3D11_CLEAR_DEPTH.0 as u32, 1.0, 0);
                    ctx.OMSetRenderTargets(
                        Some(&[Some(lshadow.moments_rtv.clone())]),
                        Some(&lshadow.depth_dsv),
                    );
                    ctx.IASetInputLayout(&self.il_part);
                    ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
                    ctx.VSSetShader(&self.vs_light_shadow, None);
                    ctx.PSSetShader(&self.ps_light_shadow, None);
                    ctx.VSSetConstantBuffers(0, Some(&[Some(self.globals_cb.clone())]));
                    ctx.PSSetConstantBuffers(0, Some(&[Some(self.globals_cb.clone())]));
                    ctx.VSSetConstantBuffers(2, Some(&[Some(lshadow.face_cb.clone())]));
                    ctx.PSSetConstantBuffers(2, Some(&[Some(lshadow.face_cb.clone())]));
                    ctx.RSSetState(&self.rs_scissor_none);
                    ctx.OMSetDepthStencilState(&self.ds_less_eq_rw, 0);
                    ctx.OMSetBlendState(&self.bs_opaque, Some(&[0.0; 4]), 0xFFFFFFFF);
                }
                Self::bind_instance_buffer(ctx, &self.static_buf);

                let cols = crate::mech::LSHADOW_ATLAS / crate::mech::LSHADOW_FACE;
                let face_px = (crate::mech::LSHADOW_FACE * lshadow.side / crate::mech::LSHADOW_ATLAS).max(1);
                for (slot, pos, range) in &slots {
                    let near: Vec<&Batch> = self
                        .st
                        .batches
                        .iter()
                        .filter(|b| b.pass == Pass::Opaque)
                        .filter(|b| (pos.clamp(b.lo, b.hi) - *pos).length_squared() <= range * range)
                        .collect();
                    let inv = 1.0 / range.max(1e-3);
                    for (f, (vp, axis)) in LightShadows::faces(*pos, *range).iter().enumerate() {
                        let mut u = [0f32; 24];
                        u[..16].copy_from_slice(&vp.to_cols_array());
                        u[16..20].copy_from_slice(&[pos.x, pos.y, pos.z, inv]);
                        u[20..24].copy_from_slice(&[axis.x, axis.y, axis.z, 0.0]);
                        crate::d3d11::upload_cb(ctx, &lshadow.face_cb, &u);
                        let k = *slot as u32 * 6 + f as u32;
                        let (x, y) = ((k % cols) * face_px, (k / cols) * face_px);
                        crate::d3d11::set_viewport(ctx, x as f32, y as f32, face_px as f32, face_px as f32);
                        crate::d3d11::set_scissor(ctx, x as i32, y as i32, face_px as i32, face_px as i32);
                        for b in &near {
                            if let Some(mesh) = self.meshes.get(b.key.0) {
                                crate::runtime::domains::services::perf::draw(
                                    crate::runtime::domains::services::perf::SHADOW,
                                    1,
                                    (mesh.count / 3) as u64 * b.count as u64,
                                );
                                unsafe {
                                    ctx.IASetVertexBuffers(0, 1, Some(&Some(mesh.vbuf.clone())), Some(&part_stride), Some(&0));
                                    ctx.DrawInstanced(mesh.count, b.count, 0, b.start);
                                }
                            }
                        }
                    }
                }
                unsafe { ctx.OMSetRenderTargets(None, None) };
            }
        }
        crate::gpu_timing::mark(ctx, "gpu: light shadows");

        let has_glass = blended.iter().any(|e| (e.2.extra[0] as u32 & 7) == 3);
        let has_neon = blended.iter().any(|e| (e.2.extra[0] as u32 & 7) == 1);
        let water_refract = !water_runs.is_empty() && self.quality.level >= crate::mech::QUALITY_WATER_REFRACT_MIN_LEVEL;
        let split = water_refract || has_glass || has_neon;
        let mut calls = 0usize;

        // Scene environment probe work (env_probe.rs).
        if let Some(pf) = frame.probe.as_ref() {
            for task in &pf.tasks {
                if let crate::env_probe::Task::Face(f) = *task {
                    let (fwd, up) = crate::env_probe::face_basis(f);
                    let centre = pf.centre;
                    let fov = std::f32::consts::FRAC_PI_2;
                    let reverse = Mat4::perspective_rh(fov, 1.0, crate::env_probe::FAR, crate::mech::NEAR);
                    let ordinary = Mat4::perspective_rh(fov, 1.0, crate::mech::NEAR, crate::env_probe::FAR);
                    let view = Mat4::look_to_rh(centre, fwd, up);
                    let view_rel = Mat4::look_to_rh(Vec3::ZERO, fwd, up);
                    let mut pg = globals;
                    pg.view_proj = (reverse * view).to_cols_array_2d();
                    pg.inv_view_proj = (ordinary.as_dmat4() * view_rel.as_dmat4()).inverse().as_mat4().to_cols_array_2d();
                    pg.cam_pos = [centre.x, centre.y, centre.z, globals.cam_pos[3]];
                    pg.env_spec[3] = 0.0;
                    crate::d3d11::update_dynamic(ctx, &self.probe.globals_cb, bytemuck::bytes_of(&pg));
                    let face_planes = frustum(ordinary * view);

                    unbind_rtv_srvs(ctx);
                    unsafe {
                        ctx.ClearRenderTargetView(&self.probe.face_rtv, &[0.0; 4]);
                        ctx.ClearDepthStencilView(&self.probe.face_dsv, D3D11_CLEAR_DEPTH.0 as u32, 0.0, 0);
                        ctx.OMSetRenderTargets(
                            Some(&[Some(self.probe.face_rtv.clone())]),
                            Some(&self.probe.face_dsv),
                        );
                    }
                    let ps = crate::env_probe::SIZE as f32;
                    crate::d3d11::set_viewport(ctx, 0.0, 0.0, ps, ps);
                    self.bind_scene_globals_with(ctx, &self.probe.globals_cb);
                    unsafe {
                        ctx.IASetInputLayout(&self.il_part);
                        ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
                        ctx.VSSetShader(&self.vs_part, None);
                        ctx.PSSetShader(if low_shaders { &self.ps_part_low } else { &self.ps_part_opaque }, None);
                        ctx.PSSetSamplers(3, Some(&[Some(self.active_sampler().clone())]));
                        ctx.OMSetDepthStencilState(&self.ds_greater_rw, 0);
                        ctx.OMSetBlendState(&self.bs_opaque, Some(&[0.0; 4]), 0xFFFFFFFF);
                    }
                    let mut last_tex = None;
                    Self::bind_instance_buffer(ctx, &self.static_buf);
                    for b in self.st.batches.iter().filter(|b| b.pass == Pass::Opaque) {
                        if !box_visible(&face_planes, b.lo, b.hi) {
                            continue;
                        }
                        unsafe {
                            ctx.RSSetState(if self.culls_back(b.key.0) { &self.rs_cull_back } else { &self.rs_cull_none });
                        }
                        self.draw_part_run(ctx, b.key, b.start..b.start + b.count, &mut last_tex);
                        if self.meshes.get(b.key.0).is_some() {
                            calls += 1;
                        }
                    }
                    if !opaque_runs.is_empty() {
                        Self::bind_instance_buffer(ctx, &self.instance_buf);
                        for r in &opaque_runs {
                            unsafe {
                                ctx.RSSetState(if self.culls_back(r.0 .0) { &self.rs_cull_back } else { &self.rs_cull_none });
                            }
                            self.draw_part_run(ctx, r.0, r.1..r.2, &mut last_tex);
                            if self.meshes.get(r.0 .0).is_some() {
                                calls += 1;
                            }
                        }
                    }
                    if !self.terrain_chunks.is_empty() {
                        unsafe {
                            ctx.IASetInputLayout(&self.il_terrain);
                            ctx.VSSetShader(&self.vs_terrain, None);
                            ctx.PSSetShader(if low_shaders { &self.ps_terrain_low } else { &self.ps_terrain }, None);
                            ctx.VSSetConstantBuffers(2, Some(&[Some(self.terrain_params_cb.clone())]));
                            ctx.PSSetConstantBuffers(2, Some(&[Some(self.terrain_params_cb.clone())]));
                            ctx.RSSetState(&self.rs_cull_back);
                        }
                        for c in self.terrain_chunks.values() {
                            if box_visible(&face_planes, c.lo, c.hi) {
                                unsafe {
                                    ctx.IASetVertexBuffers(0, 1, Some(&Some(c.vbuf.clone())), Some(&terrain_stride), Some(&0));
                                    ctx.Draw(c.count, 0);
                                }
                            }
                        }
                    }
                    unsafe { ctx.OMSetRenderTargets(None, None) };
                    unbind_rtv_srvs(ctx);
                    self.probe.store_face(ctx, f);
                } else {
                    unbind_rtv_srvs(ctx);
                    unsafe { ctx.OMSetRenderTargets(None, None) };
                    self.probe.encode(ctx, *task);
                }
            }
        }
        crate::gpu_timing::mark(ctx, "gpu: env probe");

        let draw_part = |key: (usize, usize), range: std::ops::Range<u32>, last_tex: &mut Option<usize>, calls: &mut usize| {
            self.draw_part_run(ctx, key, range, last_tex);
            if self.meshes.get(key.0).is_some() {
                *calls += 1;
            }
        };

        let encoded = !self.see.linear
            && (!surface_decal_runs.is_empty()
                || !decal_runs.is_empty()
                || !blend_runs.is_empty()
                || !overlay_runs.is_empty()
                || static_ranges.iter().any(|r| r.0 == Pass::Decal));

        let draw_surface_decals = |calls: &mut usize| {
            if !surface_decal_runs.is_empty() {
                self.bind_see_pipe(ctx, SeePipeKind::Transparent, encoded);
                Self::bind_instance_buffer(ctx, &self.instance_buf);
                let mut last_tex = None;
                for r in &surface_decal_runs {
                    draw_part(r.0, r.1..r.2, &mut last_tex, calls);
                }
            }
        };
        let draw_see_through = |target: &Target, glass_ready: bool, calls: &mut usize| {
            let glass_srvs = if glass_ready {
                if let (Some(sc), Some(gd)) = (target.scene_copy.as_ref(), target.glass_depth.as_ref()) {
                    [Some(sc.1.clone()), Some(gd.1.clone())]
                } else {
                    [Some(self.black_srv.clone()), Some(self.black_srv.clone())]
                }
            } else {
                [Some(self.black_srv.clone()), Some(self.black_srv.clone())]
            };
            unsafe { ctx.PSSetShaderResources(19, Some(&glass_srvs)) };
            self.bind_see_pipe(ctx, SeePipeKind::Transparent, encoded);
            let mut last_tex = None;
            Self::bind_instance_buffer(ctx, &self.static_buf);
            for r in static_ranges.iter().filter(|r| r.0 == Pass::Decal) {
                draw_part(r.1, r.2..r.3, &mut last_tex, calls);
            }
            Self::bind_instance_buffer(ctx, &self.instance_buf);
            for r in &decal_runs {
                draw_part(r.0, r.1..r.2, &mut last_tex, calls);
            }
            for r in &blend_runs {
                let kind = if r.1 < glass_end {
                    if self.culls_back(r.0 .0) { SeePipeKind::GlassCulled } else { SeePipeKind::Glass }
                } else if self.culls_back(r.0 .0) {
                    SeePipeKind::TransparentCulled
                } else {
                    SeePipeKind::Transparent
                };
                self.bind_see_pipe(ctx, kind, encoded);
                draw_part(r.0, r.1..r.2, &mut last_tex, calls);
            }
            if !overlay_runs.is_empty() {
                self.bind_see_pipe(ctx, SeePipeKind::Overlay, encoded);
                for r in &overlay_runs {
                    draw_part(r.0, r.1..r.2, &mut last_tex, calls);
                }
            }
        };
        let target_size = target.size;
        let draw_lines = || {
            if !frame.lines.is_empty() {
                crate::d3d11::set_scissor(ctx, 0, 0, target_size[0] as i32, target_size[1] as i32);
                unsafe {
                    ctx.IASetInputLayout(&self.il_line);
                    ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_LINELIST);
                    ctx.VSSetShader(&self.vs_line, None);
                    ctx.PSSetShader(&self.ps_line, None);
                    ctx.RSSetState(&self.rs_cull_none);
                    ctx.OMSetDepthStencilState(&self.ds_gequal_ro, 0);
                    ctx.OMSetBlendState(&self.bs_alpha, Some(&[0.0; 4]), 0xFFFFFFFF);
                    ctx.IASetVertexBuffers(0, 1, Some(&Some(self.line_buf.clone())), Some(&line_stride), Some(&0));
                    ctx.Draw(frame.lines.len() as u32, 0);
                }
            }
        };
        let run_see_encode = |target: &Target| {
            crate::d3d11::unbind_ps_srvs(ctx, 15, 17);
            unsafe { ctx.OMSetRenderTargets(None, None) };
            self.bind_scene_globals_with(ctx, &self.globals_cb);
            unsafe { ctx.OMSetRenderTargets(Some(&[Some(target.see_rtv.clone())]), None) };
            crate::d3d11::set_viewport(ctx, 0.0, 0.0, target.size[0] as f32, target.size[1] as f32);
            unsafe {
                ctx.IASetInputLayout(None);
                ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
                ctx.VSSetShader(&self.vs_post, None);
                ctx.PSSetShader(self.see.encode.get(msaa), None);
                ctx.PSSetShaderResources(18, Some(&[Some(target.msaa_srv.clone())]));
                ctx.RSSetState(&self.rs_scissor_none);
                ctx.OMSetDepthStencilState(&self.ds_none, 0);
                ctx.OMSetBlendState(&self.bs_opaque, Some(&[0.0; 4]), 0xFFFFFFFF);
            }
            for &[x, y, w, h] in &see_rects {
                crate::d3d11::set_scissor(ctx, x as i32, y as i32, w as i32, h as i32);
                unsafe { ctx.Draw(3, 0) };
            }
            crate::gpu_timing::mark(ctx, "gpu: see-through encode (bounded)");
            crate::d3d11::unbind_ps_srvs(ctx, 18, 1);
            unsafe { ctx.OMSetRenderTargets(Some(&[Some(target.see_rtv.clone())]), Some(&target.depth_dsv)) };
            crate::d3d11::set_viewport(ctx, 0.0, 0.0, target.size[0] as f32, target.size[1] as f32);
        };
        let run_see_decode = |target: &Target, with_lines: bool| {
            crate::gpu_timing::mark(ctx, "gpu: see-through draws");
            crate::d3d11::unbind_ps_srvs(ctx, 15, 17);
            unsafe { ctx.OMSetRenderTargets(None, None) };
            unsafe { ctx.OMSetRenderTargets(Some(&[Some(target.msaa_rtv.clone())]), Some(&target.depth_dsv)) };
            crate::d3d11::set_viewport(ctx, 0.0, 0.0, target.size[0] as f32, target.size[1] as f32);
            unsafe {
                ctx.IASetInputLayout(None);
                ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
                ctx.VSSetShader(&self.vs_post, None);
                ctx.PSSetShader(self.see.decode.get(msaa), None);
                ctx.PSSetShaderResources(18, Some(&[Some(target.see_srv.clone())]));
                ctx.RSSetState(&self.rs_scissor_none);
                ctx.OMSetDepthStencilState(&self.ds_always_ro, 0);
                ctx.OMSetBlendState(&self.bs_opaque, Some(&[0.0; 4]), 0xFFFFFFFF);
            }
            for &[x, y, w, h] in &see_rects {
                crate::d3d11::set_scissor(ctx, x as i32, y as i32, w as i32, h as i32);
                unsafe { ctx.Draw(3, 0) };
            }
            crate::d3d11::unbind_ps_srvs(ctx, 18, 1);
            if with_lines {
                draw_lines();
            }
            self.resolve_msaa(ctx, target);
            crate::gpu_timing::mark(ctx, "gpu: see-through decode (bounded)");
        };

        // Initial viewport pass: sky, opaque parts, decal depth, terrain & grass.
        unbind_rtv_srvs(ctx);
        self.bind_scene_globals_with(ctx, &self.globals_cb);
        unsafe {
            ctx.ClearRenderTargetView(&target.msaa_rtv, &[0.0; 4]);
            ctx.ClearDepthStencilView(&target.depth_dsv, D3D11_CLEAR_DEPTH.0 as u32, 0.0, 0);
            ctx.OMSetRenderTargets(Some(&[Some(target.msaa_rtv.clone())]), Some(&target.depth_dsv));
        }
        crate::d3d11::set_viewport(ctx, 0.0, 0.0, target.size[0] as f32, target.size[1] as f32);

        if frame.sky {
            unsafe {
                ctx.IASetInputLayout(None);
                ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
                ctx.VSSetShader(&self.vs_sky, None);
                ctx.PSSetShader(&self.ps_sky, None);
                ctx.RSSetState(&self.rs_cull_none);
                ctx.OMSetDepthStencilState(&self.ds_none, 0);
                ctx.OMSetBlendState(&self.bs_opaque, Some(&[0.0; 4]), 0xFFFFFFFF);
                ctx.Draw(3, 0);
            }
        }

        // Opaque parts (back-culled or two-sided).
        unsafe {
            ctx.IASetInputLayout(&self.il_part);
            ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            ctx.VSSetShader(&self.vs_part, None);
            ctx.PSSetShader(if low_shaders { &self.ps_part_low } else { &self.ps_part_opaque }, None);
            ctx.PSSetSamplers(3, Some(&[Some(self.active_sampler().clone())]));
            ctx.OMSetDepthStencilState(&self.ds_greater_rw, 0);
            ctx.OMSetBlendState(&self.bs_opaque, Some(&[0.0; 4]), 0xFFFFFFFF);
        }
        let mut last_tex = None;
        Self::bind_instance_buffer(ctx, &self.static_buf);
        for r in static_ranges.iter().filter(|r| r.0 == Pass::Opaque) {
            unsafe {
                ctx.RSSetState(if self.culls_back(r.1 .0) { &self.rs_cull_back } else { &self.rs_cull_none });
            }
            draw_part(r.1, r.2..r.3, &mut last_tex, &mut calls);
        }
        if !opaque_runs.is_empty() {
            Self::bind_instance_buffer(ctx, &self.instance_buf);
            for r in &opaque_runs {
                unsafe {
                    ctx.RSSetState(if self.culls_back(r.0 .0) { &self.rs_cull_back } else { &self.rs_cull_none });
                }
                draw_part(r.0, r.1..r.2, &mut last_tex, &mut calls);
            }
        }

        // Decal opaque texel depth.
        if !surface_decal_runs.is_empty() || !blend_runs.is_empty() {
            Self::bind_instance_buffer(ctx, &self.instance_buf);
            unsafe {
                ctx.PSSetShader(&self.ps_decal_depth, None);
                ctx.RSSetState(&self.rs_cull_none);
                ctx.OMSetDepthStencilState(&self.ds_greater_rw, 0);
                ctx.OMSetBlendState(&self.bs_no_color, Some(&[0.0; 4]), 0xFFFFFFFF);
            }
            for r in surface_decal_runs.iter().chain(&blend_runs) {
                draw_part(r.0, r.1..r.2, &mut last_tex, &mut calls);
            }
        }

        // Terrain chunks + decoration grass in view.
        if !self.terrain_chunks.is_empty() || !self.terrain_grass.is_empty() {
            unsafe {
                ctx.IASetInputLayout(&self.il_terrain);
                ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
                ctx.VSSetShader(&self.vs_terrain, None);
                ctx.PSSetShader(if low_shaders { &self.ps_terrain_low } else { &self.ps_terrain }, None);
                ctx.VSSetConstantBuffers(2, Some(&[Some(self.terrain_params_cb.clone())]));
                ctx.PSSetConstantBuffers(2, Some(&[Some(self.terrain_params_cb.clone())]));
                ctx.RSSetState(&self.rs_cull_back);
                ctx.OMSetDepthStencilState(&self.ds_greater_rw, 0);
                ctx.OMSetBlendState(&self.bs_opaque, Some(&[0.0; 4]), 0xFFFFFFFF);
            }
            for c in self.terrain_chunks.values().chain(self.terrain_grass.values()) {
                if box_visible(&planes, c.lo, c.hi) {
                    crate::runtime::domains::services::perf::draw(
                        crate::runtime::domains::services::perf::SCENE,
                        1,
                        (c.count / 3) as u64,
                    );
                    unsafe {
                        ctx.IASetVertexBuffers(0, 1, Some(&Some(c.vbuf.clone())), Some(&terrain_stride), Some(&0));
                        if c.grass_ranges.is_empty() {
                            ctx.Draw(c.count, 0);
                        } else {
                            let camera = Vec3::new(
                                frame.globals.cam_pos[0],
                                frame.globals.cam_pos[1],
                                frame.globals.cam_pos[2],
                            );
                            for range in &c.grass_ranges {
                                ctx.Draw(range.count(camera), range.start);
                            }
                        }
                    }
                }
            }
        }

        // Low-quality water fallback (< QUALITY_WATER_REFRACT_MIN_LEVEL): shade water parts directly
        // via scene.hlsl kind == 5.0 without splitting, copying hdr_tex, or building water_depth.
        if !water_runs.is_empty() && !water_refract {
            self.bind_part_pipeline(ctx, true, false);
            Self::bind_instance_buffer(ctx, &self.instance_buf);
            let mut water_last_tex = None;
            for r in &water_runs {
                draw_part(r.0, r.1..r.2, &mut water_last_tex, &mut calls);
            }
        }

        if !encoded {
            draw_surface_decals(&mut calls);
            draw_lines();
            if !split {
                draw_see_through(&target, false, &mut calls);
            }
        }
        unsafe { ctx.OMSetRenderTargets(None, None) };
        if split || !encoded {
            self.resolve_msaa(ctx, &target);
        }
        crate::gpu_timing::mark(ctx, "gpu: scene (opaque, terrain, unencoded blends)");

        if encoded && split && !surface_decal_runs.is_empty() {
            run_see_encode(&target);
            draw_surface_decals(&mut calls);
            run_see_decode(&target, false);
        }

        let mut glass_depth_ran = false;
        if split {
            crate::d3d11::unbind_ps_srvs(ctx, 15, 17);
            {
                let copy_tex = target.ensure_scene_copy(device).0.clone();
                unsafe {
                    ctx.OMSetRenderTargets(None, None);
                    ctx.CopyResource(&copy_tex, &target.hdr_tex);
                }
            }
            if water_refract {
                let u = self.water.uniforms(&frame.water, target.size);
                self.water.upload(ctx, &u);
                let copy_srv = target.scene_copy.as_ref().unwrap().1.clone();
                let water_dsv = target.ensure_water_depth(device).clone();
                unsafe {
                    ctx.ClearDepthStencilView(&water_dsv, D3D11_CLEAR_DEPTH.0 as u32, 0.0, 0);
                    ctx.OMSetRenderTargets(
                        Some(&[Some(target.msaa_rtv.clone())]),
                        Some(&water_dsv),
                    );
                }
                crate::d3d11::set_viewport(ctx, 0.0, 0.0, target.size[0] as f32, target.size[1] as f32);
                self.bind_scene_globals_with(ctx, &self.globals_cb);
                unsafe {
                    ctx.IASetInputLayout(&self.il_part);
                    ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
                    ctx.PSSetSamplers(3, Some(&[Some(self.active_sampler().clone())]));
                }
                self.water.bind(ctx, &copy_srv, &target.depth_srv, msaa);
                Self::bind_instance_buffer(ctx, &self.instance_buf);
                let mut water_last_tex = None;
                for r in &water_runs {
                    draw_part(r.0, r.1..r.2, &mut water_last_tex, &mut calls);
                }
                crate::d3d11::unbind_ps_srvs(ctx, 15, 17);
                self.resolve_msaa(ctx, &target);
            }
            if has_glass {
                self.resolve_glass_depth(device, ctx, &mut target);
                glass_depth_ran = true;
            }
            if !encoded {
                self.bind_scene_globals_with(ctx, &self.globals_cb);
                unsafe {
                    ctx.OMSetRenderTargets(
                        Some(&[Some(target.msaa_rtv.clone())]),
                        Some(&target.depth_dsv),
                    );
                }
                crate::d3d11::set_viewport(ctx, 0.0, 0.0, target.size[0] as f32, target.size[1] as f32);
                draw_see_through(&target, has_glass, &mut calls);
                crate::d3d11::unbind_ps_srvs(ctx, 15, 17);
                self.resolve_msaa(ctx, &target);
            }
        }
        if encoded {
            run_see_encode(&target);
            if !split {
                draw_surface_decals(&mut calls);
            }
            draw_see_through(&target, has_glass, &mut calls);
            run_see_decode(&target, true);
        }
        if split || encoded {
            crate::gpu_timing::mark(ctx, "gpu: water, glass, split see-through");
        }
        stats.draw_calls = calls;

        let ssao_needed = self.quality.enable_ssao && globals.ssao[3] > 0.0;
        let clouds_needed = clouds_active && frame.clouds.is_some() && self.clouds.is_some();

        crate::d3d11::unbind_ps_srvs(ctx, 15, 17);
        if ssao_needed {
            let copy_tex = target.ensure_scene_copy(device).0.clone();
            unsafe {
                ctx.OMSetRenderTargets(None, None);
                ctx.CopyResource(&copy_tex, &target.hdr_tex);
            }
            crate::gpu_timing::mark(ctx, "gpu: scene copy");
        } else {
            unsafe { ctx.OMSetRenderTargets(None, None) };
        }

        self.bind_scene_globals_with(ctx, &self.globals_cb);

        // SSAO passes.
        if ssao_needed {
            if msaa && !glass_depth_ran {
                self.resolve_glass_depth(device, ctx, &mut target);
                glass_depth_ran = true;
            }
            let _ = target.ensure_ssao(device);
            let ssao = target.ssao.as_ref().unwrap();
            let copy_srv = &target.scene_copy.as_ref().unwrap().1;
            let depth_1x_srv = if msaa {
                &target.glass_depth.as_ref().unwrap().1
            } else {
                &target.depth_srv
            };
            self.run_post_pass(
                ctx,
                &ssao.depth_rtv,
                target.half_size,
                Some([1.0; 4]),
                self.ps_ssao_down.get(msaa),
                [None, None, None, Some(&target.depth_srv), None, None, None, None],
                &self.bs_opaque,
            );
            self.run_post_pass(
                ctx,
                &ssao.rtvs[0],
                target.half_size,
                Some([1.0; 4]),
                &self.ps_ssao_main,
                [None, None, None, None, Some(&ssao.depth_srv), None, None, None],
                &self.bs_opaque,
            );
            self.run_post_pass(
                ctx,
                &ssao.rtvs[1],
                target.half_size,
                Some([1.0; 4]),
                &self.ps_ssao_blur_x,
                [
                    None,
                    None,
                    None,
                    None,
                    Some(&ssao.srvs[0]),
                    Some(copy_srv),
                    None,
                    None,
                ],
                &self.bs_opaque,
            );
            self.run_post_pass(
                ctx,
                &ssao.rtvs[0],
                target.half_size,
                Some([1.0; 4]),
                &self.ps_ssao_blur_y,
                [
                    None,
                    None,
                    None,
                    None,
                    Some(&ssao.srvs[1]),
                    Some(copy_srv),
                    None,
                    None,
                ],
                &self.bs_opaque,
            );
            if std::env::var_os("RUSTUDIO_TERRAIN_CAPTURE_NO_AO").is_none()
                && std::env::var_os("RUSTUDIO_TERRAIN_CAPTURE_ID").is_none()
            {
                self.run_post_pass(
                    ctx,
                    &target.hdr_rtv,
                    target.size,
                    None,
                    &self.ps_ssao_apply,
                    [
                        None,
                        None,
                        None,
                        Some(depth_1x_srv),
                        Some(&ssao.srvs[0]),
                        Some(copy_srv),
                        None,
                        None,
                    ],
                    &self.bs_ssao_apply,
                );
            }
            crate::d3d11::unbind_ps_srvs(ctx, 15, 8);
            unsafe { ctx.OMSetRenderTargets(None, None) };
        }
        crate::gpu_timing::mark(ctx, "gpu: ssao");

        // Terrain.Clouds over the sky (Studio composes them after SSAO).
        if clouds_needed {
            if msaa && !glass_depth_ran {
                self.resolve_glass_depth(device, ctx, &mut target);
            }
            let copy = target.ensure_scene_copy(device);
            let copy_tex = copy.0.clone();
            let copy_srv = copy.1.clone();
            let depth_1x_srv = if msaa {
                &target.glass_depth.as_ref().unwrap().1
            } else {
                &target.depth_srv
            };
            crate::d3d11::unbind_ps_srvs(ctx, 15, 17);
            unsafe {
                ctx.OMSetRenderTargets(None, None);
                ctx.CopyResource(&copy_tex, &target.hdr_tex);
            }
            self.bind_scene_globals_with(ctx, &self.globals_cb);
            crate::d3d11::set_viewport(ctx, 0.0, 0.0, target.size[0] as f32, target.size[1] as f32);
            if let Some(c) = self.clouds.as_mut() {
                c.composite(ctx, &target.hdr_rtv, &copy_srv, depth_1x_srv);
            }
            crate::d3d11::unbind_ps_srvs(ctx, 15, 17);
            unsafe { ctx.OMSetRenderTargets(None, None) };
        }
        crate::gpu_timing::mark(ctx, "gpu: clouds composite");

        self.post_deferred = frame.defer_post;
        if !frame.defer_post {
            self.run_bloom_dof_post(device, ctx, &mut target);
        }

        self.target = Some(target);
        self.probe.flush(ctx);
        if let Some(c) = self.clouds.as_mut() {
            c.after_submit();
        }
        self.stats = stats;
        crate::runtime::domains::services::perf::end_frame(svc_t0.elapsed().as_secs_f64(), 0.0);
        self.texture_id
    }
}

const BUILTIN_MESHES: usize = 5;

impl Renderer {
    /// The mesh index to draw loaded mesh `index` single-sided (back faces
    /// culled) or two-sided with: `index` itself, or, when another part
    /// already draws that shared mesh the other way (a DoubleSided MeshPart
    /// beside single-sided ones), a second slot over the same vertices.
    /// Skinned slots are a part's own and are set in place.
    pub fn mesh_cull_slot(&mut self, index: u32, cull_back: bool) -> u32 {
        let Some(m) = self.meshes.get_mut(index as usize + BUILTIN_MESHES) else { return index };
        if m.storage || m.cull.is_none_or(|c| c == cull_back) {
            m.cull = Some(cull_back);
            return index;
        }
        if let Some(&a) = self.cull_alias.get(&(index, cull_back)) {
            return a;
        }
        let alias = Mesh { vbuf: m.vbuf.clone(), count: m.count, cull: Some(cull_back), storage: false };
        self.meshes.push(alias);
        let a = (self.meshes.len() - 1 - BUILTIN_MESHES) as u32;
        self.cull_alias.insert((index, cull_back), a);
        a
    }

    /// Whether the mesh at this slot is drawn with back faces culled:
    /// built-in shapes always, loaded meshes when mesh_cull_slot says so.
    fn culls_back(&self, slot: usize) -> bool {
        slot < BUILTIN_MESHES || self.meshes.get(slot).is_some_and(|m| m.cull == Some(true))
    }

    /// Whether the slot's sides are known (built-in shapes, file meshes)
    /// rather than drawn two-sided for an unknown winding (CSG).
    fn culling_known(&self, slot: usize) -> bool {
        slot < BUILTIN_MESHES || self.meshes.get(slot).is_some_and(|m| m.cull.is_some())
    }
}

/// Index into `Renderer::meshes`: built-in shapes first, then loaded meshes.
fn mesh_slot(p: &PartDraw) -> usize {
    match p.custom_mesh {
        Some(i) => BUILTIN_MESHES + i as usize,
        None => p.mesh as usize,
    }
}

impl Renderer {
    /// Uploads a triangle list (positions in the unit box, normals scaled to
    /// match) and returns its index for [`PartDraw::custom_mesh`].
    pub fn add_mesh(
        &mut self,
        device: &ID3D11Device,
        positions: &[Vec3],
        normals: &[Vec3],
        uvs: &[glam::Vec2],
    ) -> u32 {
        self.add_mesh_colored(device, positions, normals, uvs, &[])
    }

    /// `add_mesh` with per-vertex colours (sRGB 0..1, alpha 1), or none.
    pub fn add_mesh_colored(
        &mut self,
        device: &ID3D11Device,
        positions: &[Vec3],
        normals: &[Vec3],
        uvs: &[glam::Vec2],
        colors: &[glam::Vec4],
    ) -> u32 {
        self.add_mesh_projected(device, positions, normals, uvs, colors, &[])
    }

    pub fn add_mesh_projected(
        &mut self,
        device: &ID3D11Device,
        positions: &[Vec3],
        normals: &[Vec3],
        uvs: &[glam::Vec2],
        colors: &[glam::Vec4],
        planes: &[glam::Vec4],
    ) -> u32 {
        self.add_mesh_with(device, positions, normals, uvs, colors, planes, false)
    }

    /// `add_mesh_projected`; `storage` also lets the skinning pass write it.
    #[allow(clippy::too_many_arguments)]
    fn add_mesh_with(
        &mut self,
        device: &ID3D11Device,
        positions: &[Vec3],
        normals: &[Vec3],
        uvs: &[glam::Vec2],
        colors: &[glam::Vec4],
        planes: &[glam::Vec4],
        storage: bool,
    ) -> u32 {
        let verts: Vec<Vertex> = positions
            .iter()
            .zip(normals)
            .enumerate()
            .map(|(i, (p, n))| Vertex {
                pos: p.to_array(),
                normal: n.to_array(),
                projection: planes.get(i).map(|p| p.to_array()).unwrap_or([0.0; 4]),
                uv: uvs.get(i).map(|u| u.to_array()).unwrap_or([0.0, 0.0]),
                color: colors.get(i).map(|c| c.to_array()).unwrap_or([0.0; 4]),
            })
            .collect();
        let vbuf = crate::d3d11::create_default_buffer(
            device,
            D3D11_BIND_VERTEX_BUFFER.0 as u32,
            std::mem::size_of_val(verts.as_slice()),
            Some(bytemuck::cast_slice(&verts)),
        );
        self.meshes.push(Mesh {
            vbuf,
            count: verts.len() as u32,
            cull: None,
            storage,
        });
        (self.meshes.len() - 1 - BUILTIN_MESHES) as u32
    }

    /// Uploads ready-made vertices (an EditableMesh's live copy,
    /// runtime/editable.rs) and returns the index for `custom_mesh`.
    pub fn add_mesh_vertices(&mut self, device: &ID3D11Device, verts: &[Vertex]) -> u32 {
        let vbuf = crate::d3d11::create_default_buffer(
            device,
            D3D11_BIND_VERTEX_BUFFER.0 as u32,
            std::mem::size_of_val(verts),
            Some(bytemuck::cast_slice(verts)),
        );
        self.meshes.push(Mesh { vbuf, count: verts.len() as u32, cull: None, storage: false });
        (self.meshes.len() - 1 - BUILTIN_MESHES) as u32
    }

    /// Overwrites vertices `first..` of loaded mesh `index` in place (no new
    /// buffer, only these bytes go up). False when they don't fit.
    pub fn write_mesh_vertices(
        &mut self,
        ctx: &ID3D11DeviceContext,
        index: u32,
        first: u32,
        verts: &[Vertex],
    ) -> bool {
        let Some(m) = self.meshes.get(index as usize + BUILTIN_MESHES) else { return false };
        if m.storage || first as usize + verts.len() > m.count as usize {
            return false;
        }
        if !verts.is_empty() {
            let stride = std::mem::size_of::<Vertex>() as u32;
            let byte_off = first * stride;
            let bytes = std::mem::size_of_val(verts) as u32;
            let dst_box = D3D11_BOX {
                left: byte_off,
                top: 0,
                front: 0,
                right: byte_off + bytes,
                bottom: 1,
                back: 1,
            };
            unsafe { ctx.UpdateSubresource(&m.vbuf, 0, Some(&dst_box), verts.as_ptr().cast(), 0, 0) };
            crate::bench::add("live_mesh_bytes", 0.0, bytes as u64);
        }
        true
    }

    /// Rewrites loaded mesh `index` whole from these arrays (the same
    /// layout `add_mesh_projected` builds), in place. A skinning slot gets
    /// its rest pose here and is queued for the compute pass again with the
    /// palette it already holds. False when the sizes differ.
    #[allow(clippy::too_many_arguments)]
    pub fn rewrite_mesh(
        &mut self,
        ctx: &ID3D11DeviceContext,
        index: u32,
        positions: &[Vec3],
        normals: &[Vec3],
        uvs: &[glam::Vec2],
        colors: &[glam::Vec4],
        projection: [f32; 4],
    ) -> bool {
        let Some(m) = self.meshes.get(index as usize + BUILTIN_MESHES) else { return false };
        if m.count as usize != positions.len() || normals.len() != positions.len() {
            return false;
        }
        let projection = if m.storage { [0.0; 4] } else { projection };
        let verts: Vec<Vertex> = positions
            .iter()
            .zip(normals)
            .enumerate()
            .map(|(i, (p, n))| Vertex {
                pos: p.to_array(),
                normal: n.to_array(),
                projection,
                uv: uvs.get(i).map(|u| u.to_array()).unwrap_or([0.0, 0.0]),
                color: colors.get(i).map(|c| c.to_array()).unwrap_or([0.0; 4]),
            })
            .collect();
        if !verts.is_empty() {
            unsafe { ctx.UpdateSubresource(&m.vbuf, 0, None, verts.as_ptr().cast(), 0, 0) };
        }
        crate::bench::add("live_mesh_bytes", 0.0, std::mem::size_of_val(verts.as_slice()) as u64);
        if m.storage {
            self.skin.again(index);
        }
        true
    }

    /// A skinned part's mesh slot deformed on the GPU (skin_gpu.rs): the
    /// slot `index` when it is one made for skinning with the same corner
    /// count, else a new one holding the mesh at rest (uvs, colours); then
    /// this frame's palette is queued for the compute pass. Returns the
    /// index to draw with.
    pub fn skin_mesh(
        &mut self,
        device: &ID3D11Device,
        ctx: &ID3D11DeviceContext,
        index: Option<u32>,
        req: &crate::skin_gpu::Request,
    ) -> u32 {
        let base = req.base;
        let ok = index
            .and_then(|i| self.meshes.get(i as usize + BUILTIN_MESHES))
            .is_some_and(|m| m.storage && m.count as usize == base.positions.len());
        let idx = match index {
            Some(i) if ok => i,
            _ => {
                if let Some(i) = index {
                    self.skin.drop_slot(i);
                }
                let i = self.add_mesh_with(device, &base.positions, &base.normals, &base.uvs, &base.colors, &[], true);
                let b = base.positions.len() as u64 * std::mem::size_of::<Vertex>() as u64;
                self.skin.bytes += b;
                crate::bench::add("skin_upload_bytes", 0.0, b);
                i
            }
        };
        let m = &self.meshes[idx as usize + BUILTIN_MESHES];
        self.skin.queue(device, ctx, idx, &m.vbuf, req);
        idx
    }

    /// Rewrites a loaded mesh's vertices in place (skinned parts re-upload
    /// their deformed copy, skin.rs), or adds a new mesh when `index` is
    /// None or its size differs. Returns the index to draw with.
    pub fn set_mesh(
        &mut self,
        device: &ID3D11Device,
        ctx: &ID3D11DeviceContext,
        index: Option<u32>,
        positions: &[Vec3],
        normals: &[Vec3],
        uvs: &[glam::Vec2],
        colors: &[glam::Vec4],
    ) -> u32 {
        let slot = index.map(|i| i as usize + BUILTIN_MESHES);
        match slot.and_then(|s| self.meshes.get(s)) {
            Some(m) if m.count as usize == positions.len() => {
                let verts: Vec<Vertex> = positions
                    .iter()
                    .zip(normals)
                    .enumerate()
                    .map(|(i, (p, n))| Vertex {
                        pos: p.to_array(),
                        normal: n.to_array(),
                        projection: [0.0; 4],
                        uv: uvs.get(i).map(|u| u.to_array()).unwrap_or([0.0, 0.0]),
                        color: colors.get(i).map(|c| c.to_array()).unwrap_or([0.0; 4]),
                    })
                    .collect();
                if !verts.is_empty() {
                    unsafe { ctx.UpdateSubresource(&m.vbuf, 0, None, verts.as_ptr().cast(), 0, 0) };
                }
                index.unwrap_or(0)
            }
            _ => self.add_mesh_colored(device, positions, normals, uvs, colors),
        }
    }
}

/// Sort / batch key: mesh slot, then texture.
fn draw_key(p: &PartDraw) -> (usize, usize) {
    (
        mesh_slot(p),
        if p.tex_mode == TexMode::None {
            0
        } else {
            p.texture.unwrap_or(0) as usize
        },
    )
}

fn tex_mode(p: &PartDraw) -> f32 {
    match (p.tex_mode, p.texture) {
        (_, None) | (TexMode::None, _) => 0.0,
        (TexMode::Over, Some(_)) => 1.0,
        (TexMode::Decal, Some(_)) => 2.0,
        (TexMode::Opaque, Some(_)) => 3.0,
        (TexMode::Cutout, Some(_)) => 4.0,
        (TexMode::OpaqueTinted, Some(_)) => 5.0,
        (TexMode::TintMask, Some(_)) => 6.0,
    }
}

impl Renderer {
    /// Adds or updates one part's draws. Parts that keep changing place move
    /// to the per-frame list; colour-only changes update the static slot.
    pub fn set_item(&mut self, r: Ref, mut draws: Vec<PartDraw>) {
        // Studio seeds material offsets from an allocation address. A DOM
        // referent supplies the equivalent stable per-instance identity here.
        let identity = u64::from_str_radix(&r.to_string()[16..], 16).unwrap_or(0);
        for d in &mut draws {
            if d.mat_layer >= 0 && d.custom_mesh.is_none() && d.tex_mode == TexMode::None {
                // Studio returns no offset for a MaterialVariant.
                let variant = self.materials.cpu.get(d.mat_layer as usize)
                    .is_some_and(|m| crate::matpack::is_variant_pack(m.pack));
                let off = if variant { [0.0; 2] } else { crate::matpack::uv_offset(d.size, d.material_id, identity) };
                d.uvt[2] = off[0];
                d.uvt[3] = off[1];
            }
        }
        if let Some(layer) = self.vf.layer_of.get(&r).copied() {
            // A ViewportFrame's part: drawn by its frame's pass only.
            let bias = bias_of(&draws);
            self.vf.touch(layer);
            self.vf.prepare(layer, r, &draws, bias);
            self.items.insert(r, Item { draws, bias, state: ItemState::Dynamic, moved: Instant::now() });
            return;
        }
        let Renderer {
            items,
            st,
            per_frame,
            pending,
            pending_at,
            ..
        } = self;
        let static_ok = draws.iter().all(|d| !shows(d) || pass_of(d).is_some());
        let Some(item) = items.get_mut(&r) else {
            let bias = bias_of(&draws);
            items.insert(
                r,
                Item {
                    draws,
                    bias,
                    state: ItemState::Pending,
                    moved: Instant::now(),
                },
            );
            per_frame.insert(r);
            *pending += 1;
            // Settle time counts from the first waiting part: a game that
            // spawns something every few frames would otherwise never settle.
            pending_at.get_or_insert_with(Instant::now);
            return;
        };
        let same_place = item.draws.len() == draws.len()
            && item.draws.iter().zip(&draws).all(|(a, b)| {
                near_xf(&a.xf, &b.xf)
                    && a.size == b.size
                    && pass_of(a) == pass_of(b)
                    && draw_key(a) == draw_key(b)
                    && shows(a) == shows(b)
            });
        match &item.state {
            ItemState::Static(slots) if same_place && static_ok => {
                for (d, &s) in draws.iter().zip(slots) {
                    if s != u32::MAX {
                        st.raw[s as usize] = to_raw(d, item.bias);
                        st.writes.push(s);
                    }
                }
            }
            ItemState::Static(slots) => {
                for &s in slots {
                    if s != u32::MAX {
                        st.raw[s as usize] = HIDDEN;
                        st.writes.push(s);
                        st.hidden += 1;
                    }
                }
                item.state = ItemState::Dynamic;
                per_frame.insert(r);
            }
            ItemState::Pending if !same_place => {
                item.state = ItemState::Dynamic;
                *pending = pending.saturating_sub(1);
            }
            _ => {}
        }
        if !same_place {
            item.moved = Instant::now();
        }
        item.draws = draws;
    }

    /// Dynamic parts that stopped changing go back to Pending, so the next
    /// static rebuild batches them again (a part moved once by a script, or
    /// whose mesh finished loading, is not per-frame for the session).
    fn requeue_quiet(&mut self) {
        if self.last_quiet_scan.elapsed().as_secs_f32() < 1.0 {
            return;
        }
        self.last_quiet_scan = Instant::now();
        let mut n = 0;
        for r in &self.per_frame {
            let Some(item) = self.items.get_mut(r) else { continue };
            if item.state == ItemState::Dynamic
                && item.moved.elapsed().as_secs_f32() >= crate::mech::DYNAMIC_SETTLE_S
                && item.draws.iter().all(|d| !shows(d) || pass_of(d).is_some())
            {
                item.state = ItemState::Pending;
                n += 1;
            }
        }
        if n > 0 {
            self.pending += n;
            self.pending_at.get_or_insert_with(Instant::now);
        }
    }

    pub fn remove_item(&mut self, r: Ref) {
        if self.vf.layer_of.contains_key(&r) {
            self.set_layer(r, None);
            return;
        }
        let Some(item) = self.items.remove(&r) else {
            return;
        };
        match item.state {
            ItemState::Static(slots) => {
                for s in slots {
                    if s != u32::MAX {
                        self.st.raw[s as usize] = HIDDEN;
                        self.st.writes.push(s);
                        self.st.hidden += 1;
                    }
                }
            }
            ItemState::Pending => self.pending = self.pending.saturating_sub(1),
            ItemState::Dynamic => {}
        }
        self.per_frame.remove(&r);
    }

    pub fn clear_items(&mut self) {
        let layered = &self.vf.layer_of;
        self.items.retain(|r, _| layered.contains_key(r));
        self.per_frame.clear();
        self.st = StaticLayout::default();
        self.static_gen += 1;
        self.pending = 0;
        self.pending_at = None;
    }

    pub fn item_draws(&self, r: Ref) -> Option<&[PartDraw]> {
        self.items.get(&r).map(|i| i.draws.as_slice())
    }

    /// Re-sorts every settled part into the static buffer.
    fn rebuild_static(&mut self, device: &ID3D11Device, ctx: &ID3D11DeviceContext) {
        struct Row {
            pass: Pass,
            key: (usize, usize),
            cell: u64,
            radius: f32,
            raw: InstanceRaw,
            lo: Vec3,
            hi: Vec3,
            owner: Ref,
            index: u32,
        }
        let mut rows: Vec<Row> = Vec::with_capacity(self.items.len());
        for (r, item) in self.items.iter_mut() {
            if item.state == ItemState::Dynamic || self.vf.layer_of.contains_key(r) {
                continue;
            }
            if !item.draws.iter().all(|d| !shows(d) || pass_of(d).is_some()) {
                item.state = ItemState::Dynamic;
                continue;
            }
            item.state = ItemState::Static(vec![u32::MAX; item.draws.len()]);
            for (i, d) in item.draws.iter().enumerate() {
                let Some(pass) = pass_of(d).filter(|_| shows(d)) else {
                    continue;
                };
                let c = (d.xf.pos / CELL).floor();
                let (lo, hi) = aabb_of(d);
                rows.push(Row {
                    pass,
                    key: draw_key(d),
                    cell: morton([c.x as i32, c.y as i32, c.z as i32]),
                    radius: radius_of(d),
                    raw: to_raw(d, item.bias),
                    lo,
                    hi,
                    owner: *r,
                    index: i as u32,
                });
            }
        }
        rows.sort_by(|a, b| {
            (a.pass, a.key, a.cell)
                .cmp(&(b.pass, b.key, b.cell))
                .then(b.radius.total_cmp(&a.radius))
        });
        let mut st = StaticLayout {
            raw: Vec::with_capacity(rows.len()),
            radii: Vec::with_capacity(rows.len()),
            ..Default::default()
        };
        let mut cell_of_batch = u64::MAX;
        for (slot, row) in rows.iter().enumerate() {
            let slot = slot as u32;
            st.raw.push(row.raw);
            st.radii.push(row.radius);
            if let Some(ItemState::Static(slots)) =
                self.items.get_mut(&row.owner).map(|i| &mut i.state)
            {
                slots[row.index as usize] = slot;
            }
            match st.batches.last_mut() {
                Some(b) if b.pass == row.pass && b.key == row.key && cell_of_batch == row.cell => {
                    b.count += 1;
                    b.lo = b.lo.min(row.lo);
                    b.hi = b.hi.max(row.hi);
                }
                _ => {
                    cell_of_batch = row.cell;
                    st.batches.push(Batch {
                        pass: row.pass,
                        key: row.key,
                        lo: row.lo,
                        hi: row.hi,
                        start: slot,
                        count: 1,
                    });
                }
            }
        }
        if st.raw.len() > self.static_cap {
            self.static_cap = st.raw.len().next_power_of_two();
            self.static_buf = Self::make_static_buf(
                device,
                self.static_cap * std::mem::size_of::<InstanceRaw>(),
            );
        }
        if !st.raw.is_empty() {
            let bytes = std::mem::size_of_val(st.raw.as_slice()) as u32;
            let dst_box = D3D11_BOX {
                left: 0,
                top: 0,
                front: 0,
                right: bytes,
                bottom: 1,
                back: 1,
            };
            unsafe {
                ctx.UpdateSubresource(&self.static_buf, 0, Some(&dst_box), st.raw.as_ptr().cast(), 0, 0);
            }
        }
        self.st = st;
        self.static_gen += 1;
        let items = &self.items;
        self.per_frame
            .retain(|r| items.get(r).is_some_and(|i| i.state == ItemState::Dynamic));
        self.pending = 0;
        self.pending_at = None;
    }

    /// Uploads static slots changed in place (merged into contiguous runs).
    fn flush_static_writes(&mut self, ctx: &ID3D11DeviceContext) {
        if self.st.writes.is_empty() {
            return;
        }
        self.static_gen += 1;
        let mut w = std::mem::take(&mut self.st.writes);
        w.sort_unstable();
        w.dedup();
        let size = std::mem::size_of::<InstanceRaw>() as u32;
        let mut i = 0;
        while i < w.len() {
            let start = w[i];
            let mut end = start + 1;
            i += 1;
            while i < w.len() && w[i] == end {
                end += 1;
                i += 1;
            }
            if (end as usize) <= self.st.raw.len() {
                let dst_box = D3D11_BOX {
                    left: start * size,
                    top: 0,
                    front: 0,
                    right: end * size,
                    bottom: 1,
                    back: 1,
                };
                let slice = &self.st.raw[start as usize..end as usize];
                unsafe {
                    ctx.UpdateSubresource(&self.static_buf, 0, Some(&dst_box), slice.as_ptr().cast(), 0, 0);
                }
            }
        }
    }
}

/// InstanceRaw.extra.y bit for CastShadow false (vs_shadow drops the part).
const NO_SHADOW_BIT: u32 = 1 << 19;

// selects PlasticReflection only for 256/272; tiled
// materials (including Metal) keep their texture-driven metalness/roughness.
pub(crate) fn material_reflectance(material: u32, reflectance: f32) -> f32 {
    match material {
        256 | 272 | 1568 => reflectance.clamp(0.0, 1.0),
        _ => 0.0,
    }
}

fn to_raw(p: &PartDraw, bias: f32) -> InstanceRaw {
    let r = p.xf.rot;
    let pos = p.xf.pos;
    InstanceRaw {
        r0: [r.x_axis.x, r.x_axis.y, r.x_axis.z, pos.x],
        r1: [r.y_axis.x, r.y_axis.y, r.y_axis.z, pos.y],
        r2: [r.z_axis.x, r.z_axis.y, r.z_axis.z, pos.z],
        size: [p.size.x, p.size.y, p.size.z, bias],
        color: [
            p.color[0], p.color[1], p.color[2],
            // scene::part_draw currently halves ForceField opacity.
            if p.material_id == 1584 { (p.alpha * 2.0).min(1.0) } else { p.alpha },
        ],
        // y: placeholder (bit 0), the six surfaces (3 bits each above it)
        // and NO_SHADOW_BIT (CastShadow false).
        // x: the shading kind (0..7), 8 = map materials by the mesh UVs,
        // 16 × (material layer + 1).
        extra: [
            (if p.material_id == 1584 { 6.0 } else { p.material })
                + if p.custom_mesh.is_some() { 8.0 } else { 0.0 }
                + 16.0 * (p.mat_layer + 1) as f32,
            (p.placeholder as u32 | p.surfaces << 1 | if p.cast_shadow { 0 } else { NO_SHADOW_BIT })
                as f32,
            // z: Reflectance, + 2 when hovered.
            material_reflectance(p.material_id, p.reflectance) + if p.hover { 2.0 } else { 0.0 },
            tex_mode(p),
        ],
        uvt: p.uvt,
        emissive: [p.emissive[0], p.emissive[1], p.emissive[2], p.tex_albedo],
        tint: match p.surface_tint {
            Some(c) => [c[0], c[1], c[2], 1.0],
            None => [1.0, 1.0, 1.0, 0.0],
        },
    }
}

fn mip_levels(w: u32, h: u32) -> u32 {
    32 - w.max(h).leading_zeros()
}

/// An RGBA8 sRGB image followed by its mip levels (each half the size,
/// 2×2 texels averaged in linear light; alpha averaged as is).
/// PBR maps contain linear data; gamma-correct colour mips distort normals
/// and roughness. Keep the channels in their encoded data space.
fn data_mip_chain(mut w: u32, mut h: u32, rgba: &[u8]) -> Vec<u8> {
    let mut out = rgba.to_vec();
    let mut prev = rgba.to_vec();
    while w > 1 || h > 1 {
        let (nw, nh) = ((w / 2).max(1), (h / 2).max(1));
        let mut next = vec![0; (nw * nh * 4) as usize];
        for y in 0..nh { for x in 0..nw { for c in 0..4 {
            let mut sum = 0u16;
            for (dx, dy) in [(0,0), (1,0), (0,1), (1,1)] {
                sum += prev[(((y*2+dy).min(h-1)*w + (x*2+dx).min(w-1))*4) as usize + c] as u16;
            }
            next[((y*nw+x)*4) as usize+c] = ((sum+2)/4) as u8;
        }}}
        out.extend_from_slice(&next);
        prev = next;
        w = nw; h = nh;
    }
    out
}

#[test]
fn pbr_mips_preserve_linear_channels() {
    let px = [0,0,0,0, 255,255,255,255, 0,0,0,0, 255,255,255,255];
    assert_eq!(&data_mip_chain(2,2,&px)[16..], &[128;4]);
}

/// Six square faces, each with its mip chain, layer-major (a cube upload).
pub fn cube_mips(size: u32, faces: &[Vec<u8>; 6]) -> Vec<u8> {
    let mut data: Vec<u8> = faces.iter().flat_map(|f| mip_chain(size, size, f)).collect();
    ggx_prefilter_cube(size, &mut data);
    data
}

/// Studio's PrefilteredEnvTexture: the sky at ENV_PREFILTER_SIZE px, each
/// mip below it GGX-filtered for roughness mip / ENV_SPEC_MIP_PER_ROUGHNESS
/// (FastConvolutionPS, Manson & Sloan's fast filtering, run per face and
/// level; the part FS reads it at lod roughness × 5). The
/// shader reads those levels of the skybox's own chain (lod offset by the
/// levels above ENV_PREFILTER_SIZE), so they're replaced in place here:
/// box-filtered levels left rough parts with sharp, glossy sky reflections.
/// Importance-sampled GGX (α = roughness², as the FS's sun term), N = V,
/// reading the box chain at the sample's solid angle.
fn ggx_prefilter_cube(size: u32, data: &mut [u8]) {
    use crate::mech as m;
    let levels = mip_levels(size, size) as usize;
    let base = (size.max(1) / m::ENV_PREFILTER_SIZE.min(size.max(1))).trailing_zeros() as usize;
    if base + 1 >= levels {
        return;
    }
    let to_lin: [f32; 256] = std::array::from_fn(|i| (i as f32 / 255.0).powf(2.2));
    let th: [f32; 255] = std::array::from_fn(|i| ((i as f32 + 0.5) / 255.0).powf(2.2));
    let to_srgb = |v: f32| th.partition_point(|&t| t <= v) as u8;
    let face_len: usize = (0..levels).map(|l| ((size >> l).max(1) as usize).pow(2) * 4).sum();
    let level_off = |l: usize| -> usize { (0..l).map(|k| ((size >> k).max(1) as usize).pow(2) * 4).sum() };
    // The source: the box chain from the base level down, linear RGB.
    let src: Vec<(usize, Vec<[f32; 3]>)> = (base..levels)
        .map(|l| {
            let w = (size >> l).max(1) as usize;
            let mut v = Vec::with_capacity(6 * w * w);
            for f in 0..6 {
                let o = f * face_len + level_off(l);
                for t in 0..w * w {
                    let p = &data[o + t * 4..o + t * 4 + 3];
                    v.push([to_lin[p[0] as usize], to_lin[p[1] as usize], to_lin[p[2] as usize]]);
                }
            }
            (w, v)
        })
        .collect();
    let fetch = |level: usize, d: Vec3| -> [f32; 3] {
        let (w, px) = &src[level.min(src.len() - 1)];
        let w = *w;
        let (f, sc, tc) = cube_face_st(d);
        let x = ((sc * w as f32 - 0.5).max(0.0)).min(w as f32 - 1.0);
        let y = ((tc * w as f32 - 0.5).max(0.0)).min(w as f32 - 1.0);
        let (x0, y0) = (x.floor() as usize, y.floor() as usize);
        let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(w - 1));
        let (fx, fy) = (x - x0 as f32, y - y0 as f32);
        let at = |xx: usize, yy: usize| px[f * w * w + yy * w + xx];
        let mut c = [0f32; 3];
        for (xx, yy, wt) in [(x0, y0, (1.0 - fx) * (1.0 - fy)), (x1, y0, fx * (1.0 - fy)), (x0, y1, (1.0 - fx) * fy), (x1, y1, fx * fy)] {
            let s = at(xx, yy);
            for ch in 0..3 {
                c[ch] += s[ch] * wt;
            }
        }
        c
    };
    let n_samples = m::ENV_PREFILTER_SAMPLES;
    let base_w = src[0].0 as f32;
    let texel_sa = 4.0 * std::f32::consts::PI / (6.0 * base_w * base_w);
    // One thread per face: each writes only its own face's levels.
    std::thread::scope(|scope| {
        for (f, face) in data.chunks_mut(face_len).enumerate() {
            let fetch = &fetch;
            let to_srgb = &to_srgb;
            let level_off = &level_off;
            scope.spawn(move || {
                for l in base + 1..levels {
                    let w = (size >> l).max(1) as usize;
                    let rough = ((l - base) as f32 / m::ENV_SPEC_MIP_PER_ROUGHNESS).min(1.0);
                    let a = rough * rough;
                    let a2 = a * a;
                    let o = level_off(l);
                    for ty in 0..w {
                        for tx in 0..w {
                            let n = cube_face_dir(f, (tx as f32 + 0.5) / w as f32, (ty as f32 + 0.5) / w as f32);
                            let up = if n.z.abs() < 0.999 { Vec3::Z } else { Vec3::X };
                            let tx_ = up.cross(n).normalize();
                            let ty_ = n.cross(tx_);
                            let mut acc = [0f32; 3];
                            let mut wsum = 0f32;
                            for i in 0..n_samples {
                                // Hammersley point → GGX half vector.
                                let u = (i as f32 + 0.5) / n_samples as f32;
                                let v = (i as u32).reverse_bits() as f32 / 4294967296.0;
                                let phi = 2.0 * std::f32::consts::PI * v;
                                let cos_t = ((1.0 - u) / (1.0 + (a2 - 1.0) * u)).sqrt();
                                let sin_t = (1.0 - cos_t * cos_t).max(0.0).sqrt();
                                let h = tx_ * (sin_t * phi.cos()) + ty_ * (sin_t * phi.sin()) + n * cos_t;
                                let lv = h * (2.0 * cos_t) - n;
                                let nl = n.dot(lv);
                                if nl <= 0.0 {
                                    continue;
                                }
                                // pdf (N = V) = D / 4; the source mip whose
                                // texel covers this sample's solid angle.
                                let dd = cos_t * cos_t * (a2 - 1.0) + 1.0;
                                let d = a2 / (std::f32::consts::PI * dd * dd);
                                let sample_sa = 1.0 / (n_samples as f32 * d * 0.25 + 1e-4);
                                let mip = (0.5 * (sample_sa / texel_sa).log2() + 1.0).max(0.0);
                                let m0 = mip.floor() as usize;
                                let t = mip - m0 as f32;
                                let c0 = fetch(m0, lv);
                                let c1 = fetch(m0 + 1, lv);
                                for ch in 0..3 {
                                    acc[ch] += (c0[ch] * (1.0 - t) + c1[ch] * t) * nl;
                                }
                                wsum += nl;
                            }
                            let p = o + (ty * w + tx) * 4;
                            for ch in 0..3 {
                                face[p + ch] = to_srgb(acc[ch] / wsum.max(1e-6));
                            }
                        }
                    }
                }
            });
        }
    });
}

/// A unit direction's cube face (wgpu order +X, −X, +Y, −Y, +Z, −Z) and its
/// texture coordinates (s, t) in 0..1 (the Vulkan cube map selection).
fn cube_face_st(d: Vec3) -> (usize, f32, f32) {
    let a = d.abs();
    let (f, sc, tc, ma) = if a.x >= a.y && a.x >= a.z {
        if d.x > 0.0 { (0, -d.z, -d.y, a.x) } else { (1, d.z, -d.y, a.x) }
    } else if a.y >= a.z {
        if d.y > 0.0 { (2, d.x, d.z, a.y) } else { (3, d.x, -d.z, a.y) }
    } else if d.z > 0.0 {
        (4, d.x, -d.y, a.z)
    } else {
        (5, -d.x, -d.y, a.z)
    };
    (f, (sc / ma + 1.0) * 0.5, (tc / ma + 1.0) * 0.5)
}

/// The direction through (s, t) of cube face `f` (inverse of cube_face_st).
fn cube_face_dir(f: usize, s: f32, t: f32) -> Vec3 {
    let (a, b) = (2.0 * s - 1.0, 2.0 * t - 1.0);
    match f {
        0 => Vec3::new(1.0, -b, -a),
        1 => Vec3::new(-1.0, -b, a),
        2 => Vec3::new(a, 1.0, b),
        3 => Vec3::new(a, -1.0, -b),
        4 => Vec3::new(a, -b, 1.0),
        _ => Vec3::new(-a, -b, -1.0),
    }
    .normalize()
}

#[test]
fn cube_face_mapping_round_trips() {
    for f in 0..6 {
        for (s, t) in [(0.2, 0.7), (0.5, 0.5), (0.9, 0.1)] {
            let (g, s2, t2) = cube_face_st(cube_face_dir(f, s, t));
            assert_eq!(g, f);
            assert!((s - s2).abs() < 1e-5 && (t - t2).abs() < 1e-5);
        }
    }
}

thread_local! {
    /// Mips made off the main thread for the image being uploaded (its
    /// pixels' address and the chain): see [`with_mips`].
    static READY_MIPS: std::cell::RefCell<Option<(usize, Vec<u8>)>> = const { std::cell::RefCell::new(None) };
}

/// The mip chain add_texture would make for `img` (None when it makes none).
pub fn mip_chain_for(img: &crate::texture::Rgba) -> Option<Vec<u8>> {
    let (w, h) = (img.width.max(1), img.height.max(1));
    (img.pixels.len() == (w * h * 4) as usize).then(|| mip_chain(w, h, &img.pixels))
}

/// Runs `upload` (which adds `pixels` as a texture) with `mips` already made
/// for it: add_texture takes them instead of building the chain itself.
pub fn with_mips<R>(pixels: &[u8], mips: Option<Vec<u8>>, upload: impl FnOnce() -> R) -> R {
    let Some(m) = mips else { return upload() };
    READY_MIPS.with(|c| *c.borrow_mut() = Some((pixels.as_ptr() as usize, m)));
    let r = upload();
    READY_MIPS.with(|c| c.borrow_mut().take());
    r
}

fn mip_chain(w: u32, h: u32, rgba: &[u8]) -> Vec<u8> {
    use std::sync::OnceLock;
    static TO_LIN: OnceLock<[f32; 256]> = OnceLock::new();
    let to_lin = TO_LIN.get_or_init(|| std::array::from_fn(|i| (i as f32 / 255.0).powf(2.2)));
    // round(v^(1/2.2) * 255) without powf: the byte whose rounding interval
    // holds v (thresholds ((s - 0.5) / 255)^2.2). powf per channel per texel
    // made mip chains a large share of a map load's texture time.
    static TH: OnceLock<[f32; 255]> = OnceLock::new();
    let th = TH.get_or_init(|| std::array::from_fn(|i| ((i as f32 + 0.5) / 255.0).powf(2.2)));
    let to_srgb = |v: f32| th.partition_point(|&t| t <= v) as u8;
    // (exactly the chain's size: growing a Vec of level 0 by doubling held
    // half as much again as the chain needs)
    let mut total = rgba.len();
    let (mut cw, mut ch) = (w, h);
    while cw > 1 || ch > 1 {
        (cw, ch) = ((cw / 2).max(1), (ch / 2).max(1));
        total += (cw * ch * 4) as usize;
    }
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(rgba);
    let (mut cw, mut ch) = (w, h);
    let mut prev = rgba.to_vec();
    while cw > 1 || ch > 1 {
        let (nw, nh) = ((cw / 2).max(1), (ch / 2).max(1));
        let mut next = vec![0u8; (nw * nh * 4) as usize];
        for y in 0..nh {
            for x in 0..nw {
                let mut acc = [0f32; 4];
                for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                    let sx = (x * 2 + dx).min(cw - 1);
                    let sy = (y * 2 + dy).min(ch - 1);
                    let o = ((sy * cw + sx) * 4) as usize;
                    for c in 0..3 {
                        acc[c] += to_lin[prev[o + c] as usize];
                    }
                    acc[3] += prev[o + 3] as f32;
                }
                let o = ((y * nw + x) * 4) as usize;
                for c in 0..3 {
                    next[o + c] = to_srgb(acc[c] * 0.25);
                }
                next[o + 3] = (acc[3] * 0.25 + 0.5) as u8;
            }
        }
        out.extend_from_slice(&next);
        prev = next;
        cw = nw;
        ch = nh;
    }
    out
}

pub fn mat_cols(m: Mat4) -> [[f32; 4]; 4] {
    m.to_cols_array_2d()
}

// ----------------------------------------------------------------- meshes

/// Collects triangles for a convex shape centred on the origin, flipping any
/// triangle whose winding faces inward so back-face culling is correct.
#[derive(Default)]
struct MeshBuilder {
    verts: Vec<Vertex>,
    /// A point strictly inside the shape.
    center: Vec3,
}

impl MeshBuilder {
    fn tri(&mut self, a: (Vec3, Vec3), b: (Vec3, Vec3), c: (Vec3, Vec3)) {
        let face_n = (b.0 - a.0).cross(c.0 - a.0);
        let centroid = (a.0 + b.0 + c.0) / 3.0;
        let (b, c) = if face_n.dot(centroid - self.center) < 0.0 {
            (c, b)
        } else {
            (b, c)
        };
        let fnorm = face_n.normalize_or(Vec3::Y)
            * if face_n.dot(centroid - self.center) < 0.0 {
                -1.0
            } else {
                1.0
            };
        for (p, n) in [a, b, c] {
            self.verts.push(Vertex {
                pos: p.to_array(),
                normal: n.to_array(),
                uv: box_uv(p, fnorm),
                projection: [0.0; 4],
                color: [0.0; 4],
            });
        }
    }

    /// A flat polygon (convex, given in order).
    fn flat(&mut self, pts: &[Vec3]) {
        let n = (pts[1] - pts[0]).cross(pts[2] - pts[0]).normalize();
        let centroid = pts.iter().copied().sum::<Vec3>() / pts.len() as f32;
        let n = if n.dot(centroid - self.center) < 0.0 {
            -n
        } else {
            n
        };
        for i in 1..pts.len() - 1 {
            self.tri((pts[0], n), (pts[i], n), (pts[i + 1], n));
        }
    }
}

/// Selection uses the same tessellation and size transform as the visible ball.
pub(crate) fn ball_outline_points() -> impl Iterator<Item = Vec3> {
    (0..=18).flat_map(|i| (0..32).map(move |j| {
        let th = std::f32::consts::PI * i as f32 / 18.0;
        let ph = std::f32::consts::TAU * j as f32 / 32.0;
        Vec3::new(th.sin() * ph.cos(), th.cos(), th.sin() * ph.sin()) * 0.5
    }))
}

pub(crate) fn outline_triangles(kind: MeshKind) -> &'static [[Vec3; 3]] {
    static TRIANGLES: std::sync::OnceLock<Vec<Vec<[Vec3; 3]>>> = std::sync::OnceLock::new();
    &TRIANGLES.get_or_init(|| build_meshes().iter().map(|vertices|
        vertices.chunks_exact(3).map(|t| [Vec3::from(t[0].pos), Vec3::from(t[1].pos), Vec3::from(t[2].pos)]).collect()
    ).collect())[kind as usize]
}

fn build_meshes() -> Vec<Vec<Vertex>> {
    let h = 0.5;
    let v = |x: f32, y: f32, z: f32| Vec3::new(x, y, z);

    // Block
    let mut block = MeshBuilder::default();
    for axis in 0..3 {
        for s in [-h, h] {
            let corner = |a: f32, b: f32| {
                let mut p = Vec3::ZERO;
                p[axis] = s;
                p[(axis + 1) % 3] = a;
                p[(axis + 2) % 3] = b;
                p
            };
            block.flat(&[corner(-h, -h), corner(h, -h), corner(h, h), corner(-h, h)]);
        }
    }

    // Ball
    let mut ball = MeshBuilder::default();
    let (rings, segs) = (18, 32);
    let sp = |i: usize, j: usize| {
        let th = std::f32::consts::PI * i as f32 / rings as f32;
        let ph = std::f32::consts::TAU * j as f32 / segs as f32;
        let n = Vec3::new(th.sin() * ph.cos(), th.cos(), th.sin() * ph.sin());
        (n * h, n)
    };
    for i in 0..rings {
        for j in 0..segs {
            let (a, b, c, d) = (sp(i, j), sp(i + 1, j), sp(i + 1, j + 1), sp(i, j + 1));
            if i != 0 {
                ball.tri(a, b, d);
            }
            if i != rings - 1 {
                ball.tri(b, c, d);
            }
        }
    }

    // Cylinder (axis along X, like Roblox cylinders)
    let mut cyl = MeshBuilder::default();
    let segs = 32;
    let ring = |j: usize| {
        let a = std::f32::consts::TAU * j as f32 / segs as f32;
        Vec3::new(0.0, a.cos(), a.sin())
    };
    for j in 0..segs {
        let (n0, n1) = (ring(j), ring(j + 1));
        let (p0, p1) = (n0 * h, n1 * h);
        cyl.tri(
            (p0 + v(-h, 0.0, 0.0), n0),
            (p0 + v(h, 0.0, 0.0), n0),
            (p1 + v(h, 0.0, 0.0), n1),
        );
        cyl.tri(
            (p0 + v(-h, 0.0, 0.0), n0),
            (p1 + v(h, 0.0, 0.0), n1),
            (p1 + v(-h, 0.0, 0.0), n1),
        );
        for s in [-h, h] {
            let n = v(s.signum(), 0.0, 0.0);
            cyl.tri(
                (v(s, 0.0, 0.0), n),
                (p0 + v(s, 0.0, 0.0), n),
                (p1 + v(s, 0.0, 0.0), n),
            );
        }
    }

    // Wedge: full height at the back (+Z), sloping down to the front (-Z).
    let mut wedge = MeshBuilder {
        center: v(0.0, -0.2, 0.2),
        ..Default::default()
    };
    wedge.flat(&[v(-h, -h, -h), v(h, -h, -h), v(h, -h, h), v(-h, -h, h)]); // bottom
    wedge.flat(&[v(-h, -h, h), v(h, -h, h), v(h, h, h), v(-h, h, h)]); // back
    wedge.flat(&[v(-h, -h, -h), v(h, -h, -h), v(h, h, h), v(-h, h, h)]); // slope
    wedge.flat(&[v(-h, -h, -h), v(-h, -h, h), v(-h, h, h)]); // left
    wedge.flat(&[v(h, -h, -h), v(h, -h, h), v(h, h, h)]); // right

    // Corner wedge: apex above the right-front corner.
    let mut corner = MeshBuilder {
        center: v(0.25, -0.3, -0.25),
        ..Default::default()
    };
    let apex = v(h, h, -h);
    let (b0, b1, b2, b3) = (v(-h, -h, -h), v(h, -h, -h), v(h, -h, h), v(-h, -h, h));
    corner.flat(&[b0, b1, b2, b3]);
    corner.flat(&[apex, b1, b2]);
    corner.flat(&[apex, b0, b1]);
    corner.flat(&[apex, b2, b3]);
    corner.flat(&[apex, b3, b0]);

    // Explicitly mark only flat oblique primitive faces. Curved normals and
    // authored union projection tags retain their existing material mapping.
    for mesh in [&mut wedge, &mut corner] {
        for vertex in &mut mesh.verts {
            let n = Vec3::from_array(vertex.normal).abs();
            if n.y > 0.0001 && (n.x > 0.0001 || n.z > 0.0001) {
                vertex.projection[0] = -99.0;
            }
        }
    }
    vec![
        block.verts,
        ball.verts,
        cyl.verts,
        wedge.verts,
        corner.verts,
    ]
}

/// The triangles of a built-in Wedge / CornerWedge in unit-part space (the
/// same shapes the meshes above draw); None for other kinds. Decals and
/// Textures are projected onto them (viewport.rs), as Studio clips a
/// Texture on a wedge's Front or Top to its slope.
pub fn shape_triangles(kind: MeshKind) -> Option<Vec<[Vec3; 3]>> {
    let h = 0.5;
    let v = Vec3::new;
    let quad = |q: [Vec3; 4]| [[q[0], q[1], q[2]], [q[0], q[2], q[3]]];
    let mut out = Vec::new();
    match kind {
        MeshKind::Wedge => {
            out.extend(quad([v(-h, -h, -h), v(h, -h, -h), v(h, -h, h), v(-h, -h, h)]));
            out.extend(quad([v(-h, -h, h), v(h, -h, h), v(h, h, h), v(-h, h, h)]));
            out.extend(quad([v(-h, -h, -h), v(h, -h, -h), v(h, h, h), v(-h, h, h)]));
            out.push([v(-h, -h, -h), v(-h, -h, h), v(-h, h, h)]);
            out.push([v(h, -h, -h), v(h, -h, h), v(h, h, h)]);
        }
        MeshKind::CornerWedge => {
            let apex = v(h, h, -h);
            let (b0, b1, b2, b3) = (v(-h, -h, -h), v(h, -h, -h), v(h, -h, h), v(-h, -h, h));
            out.extend(quad([b0, b1, b2, b3]));
            out.push([apex, b1, b2]);
            out.push([apex, b0, b1]);
            out.push([apex, b2, b3]);
            out.push([apex, b3, b0]);
        }
        _ => return None,
    }
    // Winding doesn't matter here: the projection keeps triangles by the
    // normal's side, so orient each one away from the shape's centre.
    let centre = if kind == MeshKind::Wedge { v(0.0, -0.2, 0.2) } else { v(0.25, -0.3, -0.25) };
    for tri in &mut out {
        let n = (tri[1] - tri[0]).cross(tri[2] - tri[0]);
        if n.dot((tri[0] + tri[1] + tri[2]) / 3.0 - centre) < 0.0 {
            tri.swap(1, 2);
        }
    }
    Some(out)
}

thread_local! {
    static REUSE_TEXTURE_SLOT: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
}

/// The next part texture upload on this thread replaces slot `index`
/// instead of taking a new one (assets.rs, for EditableImage updates).
pub fn reuse_texture_slot(index: u32) {
    REUSE_TEXTURE_SLOT.with(|c| c.set(Some(index)));
}
