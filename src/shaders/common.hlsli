#ifndef RUSTUDIO_COMMON_HLSLI
#define RUSTUDIO_COMMON_HLSLI

#pragma pack_matrix(column_major)
#include "mech_constants.hlsli"

#ifndef MSAA_SAMPLES
#define MSAA_SAMPLES 1
#endif

// Standard fullscreen triangle helper (CCW in NDC, UV (0,0) top-left).
void fullscreen_tri(uint id, out float4 pos, out float2 uv) {
    float x = (float)((int)(id & 1u) * 4 - 1);
    float y = (float)((int)(id >> 1u) * 4 - 1);
    pos = float4(x, y, 0.0f, 1.0f);
    uv = float2(x * 0.5f + 0.5f, 0.5f - y * 0.5f);
}

// Explicit matrix column extraction (m[r][c] in HLSL is row-first).
float4 mat_col(float4x4 m, uint col) {
    return float4(m[0][col], m[1][col], m[2][col], m[3][col]);
}

float lane4(float4 v, uint idx) {
    return (idx == 0u) ? v.x : ((idx == 1u) ? v.y : ((idx == 2u) ? v.z : v.w));
}

float3 sq(float3 c) {
    return c * c;
}

float3 srgb_to_linear(float3 c) {
    float3 lo = c / 12.92f;
    float3 hi = pow(abs(max((c + 0.055f) / 1.055f, 0.0f)), 2.4f);
    return (c <= 0.04045f) ? lo : hi;
}

float3 linear_to_srgb(float3 c) {
    float3 lo = 12.92f * c;
    float3 hi = 1.055f * pow(abs(max(c, 0.0f)), 1.0f / 2.4f) - 0.055f;
    return (c <= 0.0031308f) ? lo : hi;
}

float3 oct_decode(float2 e) {
    float3 v = float3(e.x, e.y, 1.0f - abs(e.x) - abs(e.y));
    if (v.z < 0.0f) {
        float2 sgn = float2(v.x >= 0.0f ? 1.0f : -1.0f, v.y >= 0.0f ? 1.0f : -1.0f);
        v.xy = (1.0f - abs(v.yx)) * sgn;
    }
    return normalize(v);
}

float evsm_visibility(float4 m, float z) {
    float ep = exp(EVSM_POS_EXP * z);
    float en = -exp(-EVSM_NEG_EXP * z);
    float2 mv = float2(EVSM_POS_EXP, EVSM_NEG_EXP) * EVSM_SHADOW_BIAS * float2(ep, en);
    float2 mv2 = mv * mv;
    float vp = max(m.y - m.x * m.x, mv2.x);
    float dp = ep - m.x;
    float vn = max(m.w - m.z * m.z, mv2.y);
    float dn = en - m.z;
    float pp = (ep <= m.x) ? 1.0f : clamp((vp / (vp + dp * dp) - EVSM_BLEED_CUT) / (1.0f - EVSM_BLEED_CUT), 0.0f, 1.0f);
    float pn = (en <= m.z) ? 1.0f : clamp((vn / (vn + dn * dn) - EVSM_BLEED_CUT) / (1.0f - EVSM_BLEED_CUT), 0.0f, 1.0f);
    return min(pp, pn);
}

// MSAA == 1 vs MSAA > 1 depth / color sampling helpers.
#if MSAA_SAMPLES > 1
#define TEX_DEPTH_MS Texture2DMS<float>
#define TEX_COLOR_MS Texture2DMS<float4>
int2 ms_dims(Texture2DMS<float> t) { uint w, h, s; t.GetDimensions(w, h, s); return int2(w, h); }
int2 ms_dims4(Texture2DMS<float4> t) { uint w, h, s; t.GetDimensions(w, h, s); return int2(w, h); }
float ms_load(Texture2DMS<float> t, int2 p, uint s) { return t.Load(p, s); }
float4 ms_load4(Texture2DMS<float4> t, int2 p, uint s) { return t.Load(p, s); }
#else
#define TEX_DEPTH_MS Texture2D<float>
#define TEX_COLOR_MS Texture2D<float4>
int2 ms_dims(Texture2D<float> t) { uint w, h; t.GetDimensions(w, h); return int2(w, h); }
int2 ms_dims4(Texture2D<float4> t) { uint w, h; t.GetDimensions(w, h); return int2(w, h); }
float ms_load(Texture2D<float> t, int2 p, uint s) { return t.Load(int3(p, 0)); }
float4 ms_load4(Texture2D<float4> t, int2 p, uint s) { return t.Load(int3(p, 0)); }
#endif

// Shared #[repr(C)] Rust GPU structs.
struct Globals {
    float4x4 view_proj;
    float4x4 inv_view_proj;
    float4 cam_pos;
    float4 sun_dir;
    float4 sun_color;
    float4 ambient;
    float4 outdoor;
    float4 sky_top;
    float4 sky_horizon;
    float4 params;
    float4 fog;
    float4 fog_color;
    float4 cc;
    float4 cc_tint;
    float4 bloom;
    float4x4 sun_casc[4];
    float4 sun_sphere[4];
    float4 sun_rect[4];
    float4 sky_rot;
    float4 env[6];
    float4 sky_tint;
    float4 lightgrid;
    float4 atmos;
    float4 env_spec;
    float4 lightocc;
    float4 atmos_cube[6];
    float4 atmos_sky;
    float4 atmos_glare;
    float4 env_indoor[6];
    float4 sky_grad[2];
    float4 lamp1;
    float4 ssao;
    float4 dof;
};

struct Light {
    float4 pos_range;
    float4 color;
    float4 dir;
    float4 extra;
    float4 surface_u;
    float4 surface_v;
};

struct ShadowSlot {
    float4x4 vp[6];
    float4 pos;
};

static const uint MAT_PARAM_LAYERS = 256u;

#ifdef SCENE_GROUP0

cbuffer GlobalsCB : register(b0) {
    Globals g;
};

cbuffer MatParamsCB : register(b1) {
    float4 mat_params[2560];
};

StructuredBuffer<Light> lights : register(t0);
TextureCube<float4> sky_tex : register(t1);
Texture2D<float4> sun_evsm_tex : register(t2);
Texture2D<float4> studs_tex : register(t3);
Texture2DArray<float4> mat_albedo : register(t4);
Texture2DArray<float4> mat_surface : register(t5);
Texture3D<float4> lightgrid_tex : register(t6);
Texture3D<float4> lightocc_tex : register(t7);
TextureCube<float4> indoor_tex : register(t8);
Texture2D<float4> brdf_lut : register(t9);
Texture2D<float4> lshadow_tex : register(t10);
StructuredBuffer<ShadowSlot> lshadow_slots : register(t11);
Texture2DArray<float4> mat_emission : register(t12);
Texture3D<float4> localgrid_tex : register(t13);
TextureCube<float4> indoor_next : register(t14);

SamplerState sky_sampler : register(s0);
SamplerState studs_sampler : register(s1);
SamplerState lshadow_sampler : register(s2);

int2 tex_dims2d(Texture2D<float4> t) { uint w = 1u, h = 1u; t.GetDimensions(w, h); return int2(w, h); }
int3 tex_dims3d(Texture3D<float4> t) { uint w = 1u, h = 1u, d = 1u; t.GetDimensions(w, h, d); return int3(w, h, d); }
uint cube_levels(TextureCube<float4> t) { uint w = 1u, h = 1u, lv = 1u; t.GetDimensions(0, w, h, lv); return lv; }

float3 tonemap(float3 x) {
    float3 y = max(x, float3(0.0f, 0.0f, 0.0f));
    if (g.params.z >= 0.5f) {
        return sqrt(clamp(y, float3(0.0f, 0.0f, 0.0f), float3(1.0f, 1.0f, 1.0f)));
    }
    float3 mapped = y * (0.3f * y + float3(0.4f, 0.4f, 0.4f))
        / (y * (0.3f * y + float3(0.38f, 0.38f, 0.38f)) + float3(0.12f, 0.12f, 0.12f));
    return clamp(mapped, float3(0.0f, 0.0f, 0.0f), float3(1.0f, 1.0f, 1.0f));
}

float2 env_brdf(float rough, float nv) {
    float4 q = rough * float4(-1.0f, -0.0275f, -0.572f, 0.022f) + float4(1.0f, 0.0425f, 1.04f, -0.04f);
    float a004 = min(q.x * q.x, exp2(-9.28f * nv)) * q.x + q.y;
    return float2(-1.04f, 1.04f) * a004 + q.zw;
}

float light_falloff(float d, float r) {
    static const float LIGHT_FADE_START = 0.71f;
    static const float LIGHT_FALLOFF_K = 110.0f;
    float start = LIGHT_FADE_START * r;
    float x = clamp(d, start, r) - start;
    float t = x / max(r - start, 1e-4f);
    float window = 2.0f * t * t * t - 3.0f * t * t + 1.0f;
    return window * LIGHT_FALLOFF_K / (LIGHT_FALLOFF_K + x * x);
}

float atmos_t(float3 dir) {
    if (g.atmos.w < 0.5f) {
        return 1.0f;
    }
    float y = max(dir.y, 1e-5f);
    return clamp(exp2(g.atmos_sky.x / (ATMOS_T_EPS + pow(abs(y), g.atmos_sky.y))), 0.0f, 1.0f);
}

float3 atmos_haze(float3 dir) {
    float3 p = saturate(dir);
    float3 n = saturate(-dir);
    float3 p2 = p * p;
    float3 n2 = n * n;
    return g.atmos_cube[0].rgb * p2.x + g.atmos_cube[1].rgb * n2.x
        + g.atmos_cube[2].rgb * p2.y + g.atmos_cube[3].rgb * n2.y
        + g.atmos_cube[4].rgb * p2.z + g.atmos_cube[5].rgb * n2.z;
}

static const float SKY_LEVEL_Q0 = 0.27f;
float3 sky_level(float3 dir) {
    if (g.sky_rot.w <= 0.0f) {
        return float3(1.0f, 1.0f, 1.0f);
    }
    float q = clamp(SKY_LEVEL_Q0 + dir.y, 0.0f, 1.0f);
    float3 l = lerp(sqrt(max(g.sky_grad[1].rgb, float3(0.0f, 0.0f, 0.0f))),
                    sqrt(max(g.sky_grad[0].rgb, float3(0.0f, 0.0f, 0.0f))), q);
    return l * l;
}

float3 sky_color_lod_k(float3 dir, float lod, float3 k) {
    float3 c;
    if (g.params.w > 0.5f) {
        float3 t = sky_tex.SampleLevel(sky_sampler, dir, lod).rgb;
        float3 tint = (g.sky_tint.w > 0.5f) ? g.sky_tint.rgb : float3(1.0f, 1.0f, 1.0f);
        c = sq(t) * k * tint;
    } else {
        float h = clamp(dir.y, -1.0f, 1.0f);
        c = lerp(g.sky_horizon.rgb, g.sky_top.rgb, pow(abs(saturate(h)), 0.55f));
        if (h < 0.0f) {
            c = lerp(g.sky_horizon.rgb, g.sky_horizon.rgb * 0.55f, saturate(-h * 3.0f));
        }
        c = c * g.sky_tint.rgb;
    }
    if (g.atmos.w < 0.5f) {
        return c;
    }
    float t = atmos_t(dir);
    float gbase = max(0.5f + 0.5f * dot(dir, normalize(g.sun_dir.xyz)), 1e-6f);
    float3 gl = saturate(pow(abs(gbase), g.atmos_glare.xyz) * (1.0f - t)) * g.atmos_sky.z;
    float3 haze = atmos_haze(dir) + g.sun_color.rgb * g.sun_dir.w * gl;
    return lerp(haze, c, t);
}

float3 sky_color_lod(float3 dir, float lod) {
    return sky_color_lod_k(dir, lod, float3(1.0f, 1.0f, 1.0f));
}

float3 sky_color(float3 dir) {
    return sky_color_lod(dir, 0.0f);
}

float3 sky_rough(float3 dir, float rough) {
    float top = (float)cube_levels(sky_tex) - 1.0f;
    return sky_color_lod(dir, max(top - ENV_SPEC_LOD_SPAN * (1.0f - rough), 0.0f));
}

bool under_water() {
    return g.fog.z > 1.5f && g.fog.z < 2.5f;
}

float fog_amount(float d) {
    if (g.fog.z > 2.5f) {
        return 1.0f - saturate(exp2(g.fog.x - g.fog.y * d));
    }
    if (g.fog.z > 1.5f) {
        return 1.0f - saturate(exp2(g.fog.x - g.fog.y * d) - 1.0f);
    }
    return 1.0f - saturate(exp2((g.fog.y - d) / max(g.fog.y - g.fog.x, 1e-3f)) - 1.0f);
}

float3 fog_rgb() {
    if (under_water()) {
        return g.fog_color.rgb * (g.ambient.rgb + g.outdoor.rgb + g.sun_color.rgb * g.sun_dir.w * max(g.sun_dir.y, 0.0f));
    }
    return g.fog_color.rgb;
}

float3 fog_rgb_at(float3 world) {
    float3 res = fog_rgb();
    if (g.fog.z > 2.5f) {
        float3 rel = world - g.cam_pos.xyz;
        float vis = 1.0f - fog_amount(length(rel));
        float top = (float)cube_levels(sky_tex) - 1.0f;
        float lod = max(top - (ATMOS_ENV_CUBE_LEVELS - 1.0f), 0.0f) + max(g.atmos_sky.w, vis) * ATMOS_FOG_MIP;
        res = sky_color_lod(normalize(rel), min(lod, top));
    }
    return res;
}

float sun_cover() {
    return g.outdoor.w;
}

float sky_gain() {
    return 2.0f - g.outdoor.w;
}

float light_grid(float3 world, float3 n) {
    if (g.lightgrid.w <= 0.0f) {
        return 1.0f;
    }
    float3 p = (world + n * g.lightgrid.w) * 0.25f - g.lightgrid.xyz;
    float3 t = float3(p.x, p.z, p.y) / (float3)tex_dims3d(lightgrid_tex);
    if (any(abs(t - 0.5f) >= LIGHTGRID_BORDER)) {
        return 1.0f;
    }
    return lightgrid_tex.SampleLevel(sky_sampler, t, 0.0f).r;
}

float light_grid_sun(float3 world, float3 n) {
    if (g.lightgrid.w <= 0.0f) {
        return 1.0f;
    }
    float3 p = (world + n * g.lightgrid.w) * 0.25f - g.lightgrid.xyz;
    float3 t = float3(p.x, p.z, p.y) / (float3)tex_dims3d(lightgrid_tex);
    if (any(abs(t - 0.5f) >= LIGHTGRID_BORDER)) {
        return 1.0f;
    }
    return lightgrid_tex.SampleLevel(sky_sampler, t, 0.0f).g;
}

float sun_shadow_map(float3 world) {
    float3 d0 = world - g.sun_sphere[0].xyz;
    float3 d1 = world - g.sun_sphere[1].xyz;
    float3 d2 = world - g.sun_sphere[2].xyz;
    float4x4 casc = g.sun_casc[3];
    float4 r = g.sun_rect[3];
    if (dot(d0, d0) < g.sun_sphere[0].w) {
        casc = g.sun_casc[0];
        r = g.sun_rect[0];
    } else if (dot(d1, d1) < g.sun_sphere[1].w) {
        casc = g.sun_casc[1];
        r = g.sun_rect[1];
    } else if (dot(d2, d2) < g.sun_sphere[2].w) {
        casc = g.sun_casc[2];
        r = g.sun_rect[2];
    }
    float4 p = mul(casc, float4(world, 1.0f));
    if (any(p.xy <= 0.0f) || any(p.xy >= 1.0f)) {
        return -1.0f;
    }
    float4 m = sun_evsm_tex.SampleLevel(lshadow_sampler, r.xy + p.xy * r.zw, 0.0f);
    return evsm_visibility(m, 2.0f * p.z - 1.0f);
}

float sun_visibility_at(float3 world, float3 n, float3 receiver) {
    if (g.bloom.z < 0.5f) {
        return 1.0f;
    }
    float voxel = light_grid_sun(world, n);
    if (g.sun_color.w > 0.5f && g.env_spec.z <= 0.5f) {
        return voxel;
    }
    float mapped = sun_shadow_map(receiver);
    if (mapped < 0.0f) {
        return voxel;
    }
    return lerp(mapped, voxel, saturate((length(world - g.cam_pos.xyz) - SUN_DIST) * HYBRID_LERP_SLOPE));
}

float sun_visibility(float3 world, float3 n) {
    return sun_visibility_at(world, n, world);
}

float sun_shadow(float3 world, float3 n) {
    return sun_visibility(world, n) * sun_cover();
}

float3 env_cube(float3 n, float sky) {
    float3 w = n * n;
    float3 outside = (n.x > 0.0f ? g.env[0].rgb : g.env[1].rgb) * w.x
        + (n.y > 0.0f ? g.env[2].rgb : g.env[3].rgb) * w.y
        + (n.z > 0.0f ? g.env[4].rgb : g.env[5].rgb) * w.z;
    float3 inside = (n.x > 0.0f ? g.env_indoor[0].rgb : g.env_indoor[1].rgb) * w.x
        + (n.y > 0.0f ? g.env_indoor[2].rgb : g.env_indoor[3].rgb) * w.y
        + (n.z > 0.0f ? g.env_indoor[4].rgb : g.env_indoor[5].rgb) * w.z;
    return lerp(inside, outside, sky);
}

float2 brdf_ab(float rough, float nv) {
    return brdf_lut.SampleLevel(sky_sampler, float2(rough, max(nv, 1e-4f)), 0.0f).rg;
}

float3 env_reflect(float3 r, float rough, float sky, bool sharp) {
    float m = sharp ? 0.0f : (rough * ENV_SPEC_MIP_PER_ROUGHNESS);
    float top_s = (float)cube_levels(sky_tex) - 1.0f;
    float top_i = (float)cube_levels(indoor_tex) - 1.0f;
    float lod_s = min(max(top_s - (ATMOS_ENV_CUBE_LEVELS - 1.0f), 0.0f) + m, top_s);
    float lod_i = min(max(top_i - (ATMOS_ENV_CUBE_LEVELS - 1.0f), 0.0f) + m, top_i);
    float3 tint = lerp(g.sky_grad[1].rgb, g.sky_grad[0].rgb, saturate(r.y * ENV_REFLECT_UP_GAIN));
    float3 outside = sky_color_lod(r, lod_s) * tint;
    float3 inside = indoor_tex.SampleLevel(sky_sampler, r, lod_i).rgb;
    if (g.env_spec.w != 0.0f) {
        inside = lerp(inside, indoor_next.SampleLevel(sky_sampler, r, lod_i).rgb, g.env_spec.w);
    }
    return lerp(inside, outside, sky);
}

float3 light_point(Light l, float3 world) {
    float3 source = l.pos_range.xyz;
    if (l.color.w > 1.5f) {
        float3 rel = world - source;
        source += l.surface_u.xyz * clamp(dot(rel, l.surface_u.xyz), -l.surface_u.w, l.surface_u.w)
            + l.surface_v.xyz * clamp(dot(rel, l.surface_v.xyz), -l.surface_v.w, l.surface_v.w);
    }
    return source;
}

float3 local_light(Light l, float3 world, float3 n) {
    float3 to = light_point(l, world) - world;
    float d = length(to);
    float range = l.pos_range.w;
    if (d >= range || range <= 0.0f) {
        return float3(0.0f, 0.0f, 0.0f);
    }
    float3 dir = to / max(d, 1e-4f);
    float k = light_falloff(d, range);
    float kind = l.color.w;
    float ndl = max(dot(n, dir), 0.0f);
    if (kind > 0.5f) {
        float c = dot(-dir, l.dir.xyz);
        float cone = saturate((c - l.dir.w) / max(1.0f - l.dir.w, 1e-3f));
        k *= cone * cone;
        if (kind > 1.5f) {
            k *= step(0.0f, c);
            ndl = saturate(dot(n, dir) + sqrt(l.extra.x * l.extra.y) / max(d, 1e-3f));
        }
    }
    return l.color.rgb * k * ndl;
}

float occ_at(float3 p) {
    int3 v = (int3)floor(p / g.lightocc.w - g.lightocc.xyz);
    int3 dims = tex_dims3d(lightocc_tex);
    if (any(v < 0) || any(v >= dims)) {
        return 0.0f;
    }
    return lightocc_tex.Load(int4(v, 0)).r;
}

float occ_transmit(float3 a, float3 b) {
    float3 d = b - a;
    float len = length(d);
    if (len < 1e-3f) {
        return 1.0f;
    }
    float steps = clamp(ceil(len / g.lightocc.w), 1.0f, LIGHTOCC_MAX_STEPS);
    float tau = 0.0f;
    [loop]
    for (float i = 0.0f; i < steps; i += 1.0f) {
        tau += occ_at(a + d * ((i + 0.5f) / steps));
    }
    return exp(-tau * (len / steps) * LIGHTOCC_DENSITY);
}

float light_occlusion(float3 world, float3 n, float3 lpos) {
    float v = g.lightocc.w;
    if (v <= 0.0f) {
        return 1.0f;
    }
    float3 start = world + n * (v * LIGHTOCC_START_VOXELS);
    float3 to = lpos - start;
    float d = length(to);
    float skip = v * LIGHTOCC_LIGHT_SKIP_VOXELS;
    if (d <= skip) {
        return 1.0f;
    }
    return occ_transmit(start, lpos - to / d * skip);
}

float light_shadow(int slot, float3 world) {
    float4 pos = lshadow_slots[slot].pos;
    float3 v = world - pos.xyz;
    float3 a = abs(v);
    uint f = 0u;
    if (a.x >= a.y && a.x >= a.z) {
        f = (v.x > 0.0f) ? 0u : 1u;
    } else if (a.y >= a.z) {
        f = (v.y > 0.0f) ? 2u : 3u;
    } else {
        f = (v.z > 0.0f) ? 4u : 5u;
    }
    float4 clip;
    [branch]
    switch (f) {
        case 0u: clip = mul(lshadow_slots[slot].vp[0], float4(world, 1.0f)); break;
        case 1u: clip = mul(lshadow_slots[slot].vp[1], float4(world, 1.0f)); break;
        case 2u: clip = mul(lshadow_slots[slot].vp[2], float4(world, 1.0f)); break;
        case 3u: clip = mul(lshadow_slots[slot].vp[3], float4(world, 1.0f)); break;
        case 4u: clip = mul(lshadow_slots[slot].vp[4], float4(world, 1.0f)); break;
        default: clip = mul(lshadow_slots[slot].vp[5], float4(world, 1.0f)); break;
    }
    float w = max(clip.w, 1e-4f);
    float2 ndc = clamp(clip.xy / w, float2(-1.0f, -1.0f), float2(1.0f, 1.0f));
    uint k = (uint)slot * 6u + f;
    float2 cell = float2((float)(k % LSHADOW_COLS), (float)(k / LSHADOW_COLS));
    float2 uv = (cell + ndc * float2(0.5f, -0.5f) + 0.5f) * LSHADOW_CELL;
    float4 m = lshadow_tex.SampleLevel(lshadow_sampler, uv, 0.0f);
    return evsm_visibility(m, 2.0f * (w * pos.w) - 1.0f);
}

float3 local_lights(float3 world, float3 n) {
    float3 sum = float3(0.0f, 0.0f, 0.0f);
    [branch]
    if (g.sun_color.w > 0.5f) {
        if (g.lightgrid.w > 0.0f) {
            float3 p = (world + n * 6.0f) * 0.25f - g.lightgrid.xyz;
            float3 t = p.xzy / (float3)tex_dims3d(localgrid_tex);
            if (!any(abs(t - 0.5f) >= LIGHTGRID_BORDER)) {
                float4 v = localgrid_tex.SampleLevel(sky_sampler, t, 0.0f);
                sum = v.rgb * (v.a * 120.0f);
            }
        }
    } else {
        uint count = (uint)g.params.x;
        [branch]
        if (count > 0u) {
            [loop]
            for (uint i = 0u; i < count; i++) {
                Light l = lights[i];
                float3 c = local_light(l, world, n);
                [branch]
                if (any(c > 0.0f)) {
                    [branch]
                    if (l.extra.w >= 0.0f) {
                        c *= light_shadow((int)l.extra.w, world);
                    } else if (l.extra.z > 0.5f) {
                        c *= light_occlusion(world, n, l.pos_range.xyz);
                    }
                    sum += c;
                }
            }
        }
    }
    return sum;
}

struct LocalPbr {
    float3 diffuse;
    float3 specular;
};

LocalPbr local_pbr(float3 world, float3 n, float3 v, float rough, float3 f0, float kd, float ss) {
    LocalPbr result;
    result.diffuse = float3(0.0f, 0.0f, 0.0f);
    result.specular = float3(0.0f, 0.0f, 0.0f);
    [branch]
    if (g.sun_color.w > 0.5f || g.env_spec.z <= 0.5f) {
        result.diffuse = local_lights(world, n);
    } else {
        uint count = (uint)g.params.x;
        [branch]
        if (count > 0u) {
            [loop]
            for (uint i = 0u; i < count; i++) {
                Light light = lights[i];
                float3 radiance = local_light(light, world, n);
                [branch]
                if (any(radiance > 0.0f)) {
                    [branch]
                    if (light.extra.w >= 0.0f) {
                        radiance *= light_shadow((int)light.extra.w, world);
                    } else if (light.extra.z > 0.5f) {
                        radiance *= light_occlusion(world, n, light.pos_range.xyz);
                    }
                    float3 direction = normalize(light_point(light, world) - world);
                    float3 vh = v + direction;
                    float3 h = vh * rsqrt(max(dot(vh, vh), 0.0001f));
                    float nh = max(dot(n, h), 0.001f);
                    float lh = dot(direction, h);
                    float edge = pow(abs(max(1.0f - lh, 0.0f)), 5.0f);
                    float3 fresnel = float3(edge, edge, edge) + f0 * (1.0f - edge);
                    float a = pow(abs(max(rough, 0.0f)), 4.0f);
                    float denominator = (nh * a - nh) * nh + 1.0f;
                    float distribution = (a + a * a) / (denominator * denominator * (lh * 3.0f + 0.5f) * (nh * 0.75f + 0.25f));
                    result.diffuse += ((float3)kd - fresnel * ss * kd) * radiance;
                    result.specular += fresnel * distribution * radiance;
                }
            }
        }
    }
    return result;
}

#endif // SCENE_GROUP0

#endif // RUSTUDIO_COMMON_HLSLI
