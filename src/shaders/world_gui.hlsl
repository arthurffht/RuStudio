#include "common.hlsli"

static const float SOFT_DEPTH_MAX = 500.0f;
static const float FX_VOXEL = 4.0f;
static const float FX_GRID_BORDER = LIGHTGRID_BORDER;
static const float2 FX_WHITE_UV = float2(0.0f, 0.0f);

struct FxGlass {
    float4 x;
    float4 y;
    float4 z;
    float4 pos;
};

cbuffer ToneCB : register(b0) {
    float4 tone_curve;
    float4 tone_e_on;
    float4 tone_fog;
    float4 tone_fog_rgb;
    float4x4 tone_casc[4];
    float4 tone_sphere[4];
    float4 tone_rect[4];
    float4 tone_grid;
    float4 tone_sun;
    float4x4 tone_vp;
    float4 tone_cam;
    float4 tone_amb;
    float4 tone_outdoor;
    float4 tone_lamp;
    float4 tone_env;
    float4 tone_right;
};

TEX_DEPTH_MS scene_depth              : register(t0);
Texture2D<float4> sun_atlas           : register(t1);
Texture3D<float4> sun_grid            : register(t2);
StructuredBuffer<Light> lights        : register(t3);
StructuredBuffer<FxGlass> glass       : register(t4);
StructuredBuffer<uint> glass_idx      : register(t5);
Texture2D<float4> tex                 : register(t6);
Texture2D<float4> src                 : register(t7);

SamplerState sun_sampler              : register(s0);
SamplerState samp                     : register(s1);

struct VIn {
    float2 ndc       : TEXCOORD0;
    float depth      : TEXCOORD1;
    float2 uv        : TEXCOORD2;
    float4 color     : TEXCOORD3;
    float3 light     : TEXCOORD4;
    float dist       : TEXCOORD5;
    float clip_w     : TEXCOORD6;
    float alpha_mul  : TEXCOORD7;
};

struct StripIn {
    float3 pos   : TEXCOORD0;
    float zoff   : TEXCOORD1;
    float3 at    : TEXCOORD2;
    float4 color : TEXCOORD3;
    float2 uv    : TEXCOORD4;
    float4 look  : TEXCOORD5;
    float3 check : TEXCOORD6;
    uint2 glass  : TEXCOORD7;
    float4 face  : TEXCOORD8;
    float scroll : TEXCOORD9;
};

struct ParticleIn {
    float3 centre : TEXCOORD0;
    float zoffset : TEXCOORD1;
    float3 e1     : TEXCOORD2;
    float soft    : TEXCOORD3;
    float3 e2     : TEXCOORD4;
    float blend   : TEXCOORD5;
    float2 uv0    : TEXCOORD6;
    float2 uv1    : TEXCOORD7;
    float2 cell   : TEXCOORD8;
    float4 color  : TEXCOORD9;
    float4 look   : TEXCOORD11;
    float3 check  : TEXCOORD12;
    uint2 glass   : TEXCOORD13;
};

struct VOut {
    float4 pos                     : SV_Position;
    float2 uv                      : TEXCOORD0;
    float4 color                   : TEXCOORD1;
    float depth                    : TEXCOORD2;
    float3 light                   : TEXCOORD3;
    float dist                     : TEXCOORD4;
    nointerpolation float fx       : TEXCOORD5;
    nointerpolation float alpha_mul : TEXCOORD6;
    float2 uv2                     : TEXCOORD7;
    nointerpolation float blend    : TEXCOORD8;
    nointerpolation float2 soft    : TEXCOORD9;
    float vw                       : TEXCOORD10;
    float3 beam_sun                : TEXCOORD11;
    float3 world                   : TEXCOORD12;
    float beam_grid                : TEXCOORD13;
};

VOut vs_gui(VIn v) {
    VOut o;
    o.pos = float4(v.ndc, 0.5f, 1.0f);
    o.fx = 0.0f;
    o.alpha_mul = v.alpha_mul;
    o.uv2 = v.uv;
    o.blend = 0.0f;
    o.soft = float2(0.0f, 0.0f);
    o.vw = 0.0f;
    o.uv = v.uv;
    o.color = v.color;
    o.depth = v.depth;
    o.light = v.light;
    o.beam_sun = float3(0.0f, 0.0f, 0.0f);
    o.beam_grid = 0.0f;
    o.world = float3(0.0f, 0.0f, 0.0f);
    o.dist = v.dist;
    return o;
}

float2 fx_grid(float3 p) {
    if (tone_grid.w <= 0.0f) {
        return float2(1.0f, 1.0f);
    }
    uint gw, gh, gd;
    sun_grid.GetDimensions(gw, gh, gd);
    float3 q = (p / FX_VOXEL - tone_grid.xyz).xzy / float3((float)gw, (float)gh, (float)gd);
    if (any(abs(q - 0.5f) >= FX_GRID_BORDER)) {
        return float2(1.0f, 1.0f);
    }
    return sun_grid.SampleLevel(sun_sampler, q, 0.0f).rg;
}

float3 fx_local(Light l, float3 p) {
    float range = l.pos_range.w;
    float3 to = l.pos_range.xyz - p;
    float d = length(to);
    float reach = range + FX_LOCAL_REACH;
    if (range <= 0.0f || d >= reach) {
        return float3(0.0f, 0.0f, 0.0f);
    }
    float3 dir = to / max(d, 1e-4f);
    float k = FX_LOCAL_SLOPE * (reach - d);
    if (l.color.w > 0.5f) {
        float c = dot(-dir, l.dir.xyz);
        float cone = saturate((c - l.dir.w) / max(1.0f - l.dir.w, 1e-3f));
        k *= cone * cone;
        if (l.color.w > 1.5f && c < 0.0f) {
            k = 0.0f;
        }
    }
    return l.color.rgb / LIGHT_INTENSITY * k;
}

float3 fx_scene(float3 p, float2 g_val) {
    float3 c = tone_amb.rgb + tone_outdoor.rgb * g_val.x + tone_env.rgb * g_val.x;
    uint n = (uint)tone_amb.w;
    for (uint i = 0u; i < n; i++) {
        c += fx_local(lights[i], p);
    }
    return min(c, (float3)FX_LIGHT_CAP) + tone_lamp.rgb * g_val.y;
}

float3 fx_look(float4 look, float3 lit) {
    return (float3)look.x + lit * look.y;
}

void fx_checked(inout VOut o, float3 gpu, float3 cpu) {
    if (tone_cam.w > 0.5f && any(abs(gpu - cpu) > (2e-3f + abs(cpu) * 2e-3f))) {
        o.color = float4(1.0f, 0.0f, 1.0f, 1.0f);
        o.light = float3(1.0f, 1.0f, 1.0f);
        o.beam_sun = float3(0.0f, 0.0f, 0.0f);
        o.uv = FX_WHITE_UV;
        o.uv2 = FX_WHITE_UV;
    }
}

bool fx_hit(FxGlass g_box, float3 o, float3 d, float len) {
    float3 rel = o - g_box.pos.xyz;
    float3 lo = float3(dot(g_box.x.xyz, rel), dot(g_box.y.xyz, rel), dot(g_box.z.xyz, rel));
    float3 ld = float3(dot(g_box.x.xyz, d), dot(g_box.y.xyz, d), dot(g_box.z.xyz, d));
    float3 half_ext = float3(g_box.x.w, g_box.y.w, g_box.z.w);
    float t0 = 0.0f;
    float t1 = len;
    [unroll]
    for (int i = 0; i < 3; i++) {
        float ldi = (i == 0) ? ld.x : ((i == 1) ? ld.y : ld.z);
        float loi = (i == 0) ? lo.x : ((i == 1) ? lo.y : lo.z);
        float hi = (i == 0) ? half_ext.x : ((i == 1) ? half_ext.y : half_ext.z);
        if (abs(ldi) < 1e-6f) {
            if (abs(loi) > hi) {
                return false;
            }
        } else {
            float a = (-hi - loi) / ldi;
            float b = (hi - loi) / ldi;
            t0 = max(t0, min(a, b));
            t1 = min(t1, max(a, b));
            if (t0 > t1) {
                return false;
            }
        }
    }
    return true;
}

float fx_transmit(float3 p, uint2 list) {
    float t = 1.0f;
    if (list.y == 0u) {
        return t;
    }
    float3 seg = p - tone_cam.xyz;
    float len = length(seg);
    float3 d = seg / max(len, 1e-6f);
    for (uint i = 0u; i < list.y; i++) {
        FxGlass g_box = glass[glass_idx[list.x + i]];
        if (fx_hit(g_box, tone_cam.xyz, d, len)) {
            t *= 1.0f - g_box.pos.w;
        }
    }
    return t;
}

float4 fx_clip(float3 p) {
    float4 c = mul(tone_vp, float4(p, 1.0f));
    if (c.w == 0.0f) {
        c.w = -1e-6f;
    }
    return c;
}

float3 toward_cam(float3 p) {
    float3 d = tone_cam.xyz - p;
    float l = length(d);
    return (l > 0.0f) ? (d / l) : float3(0.0f, 0.0f, 0.0f);
}

VOut vs_strip(StripIn v) {
    VOut o;
    float3 p = v.pos;
    float3 at = v.at;
    if (v.face.w != 0.0f) {
        float3 c = cross(v.face.xyz, tone_cam.xyz - p);
        float rcp_l = 1.0f / max(length(c), 1e-38f);
        bool ok = (length(c) > 0.0f) && (rcp_l < 3.0e38f);
        float3 side = ok ? (c * rcp_l) : tone_right.xyz;
        p += side * v.face.w;
    }
    if (v.zoff != 0.0f) {
        float3 s = toward_cam(at) * v.zoff;
        p += s;
        at += s;
    }
    float4 c = fx_clip(p);
    o.pos = c;
    o.depth = c.z;
    o.vw = c.w;
    o.fx = 1.0f;
    o.alpha_mul = v.look.w;
    o.uv = float2(v.uv.x, v.uv.y - v.scroll);
    o.uv2 = o.uv;
    o.blend = 0.0f;
    o.soft = float2(0.0f, 0.0f);
    float t = fx_transmit(at, v.glass);
    o.color = v.color * t;
    o.dist = length(p - tone_cam.xyz);
    o.world = p;
    o.beam_sun = float3(0.0f, 0.0f, 0.0f);
    o.beam_grid = 0.0f;
    if (v.look.y > 0.0f) {
        float2 g_val = fx_grid(at);
        o.light = fx_look(v.look, fx_scene(at, g_val));
        if (v.look.z > 1.5f) {
            o.beam_sun = tone_lamp.rgb * v.look.y;
            o.beam_grid = g_val.y;
        }
    } else {
        o.light = (float3)v.look.x;
    }
    fx_checked(o, o.light * t, v.check);
    return o;
}

VOut vs_particle(ParticleIn v, uint vi : SV_VertexID) {
    VOut o;
    static const uint corners[6] = { 0u, 1u, 2u, 0u, 2u, 3u };
    uint k = corners[vi];
    float cx = (k == 1u || k == 2u) ? 1.0f : -1.0f;
    float cy = (k >= 2u) ? 1.0f : -1.0f;
    float3 p = v.centre + (v.e1 * cx + v.e2 * cy);
    float4 c = fx_clip(p);
    float z = c.z;
    if (v.zoffset != 0.0f) {
        float3 q = p + toward_cam(p) * v.zoffset;
        float4 cz = mul(tone_vp, float4(q, 1.0f));
        if (abs(cz.w) > 1e-6f) {
            z = cz.z * c.w / cz.w;
        }
    }
    o.pos = float4(c.xy, z, c.w);
    o.depth = z;
    o.vw = c.w;
    o.fx = 1.0f;
    o.alpha_mul = v.look.w;
    float2 corner = float2((cx + 1.0f) * 0.5f, 1.0f - (cy + 1.0f) * 0.5f) * v.cell;
    o.uv = v.uv0 + corner;
    o.uv2 = v.uv1 + corner;
    o.blend = v.blend;
    o.soft = float2(v.soft, (v.soft > 0.0f) ? v.zoffset : 0.0f);
    float t = fx_transmit(v.centre, v.glass);
    o.color = v.color * t;
    o.dist = length(p - tone_cam.xyz);
    o.world = p;
    o.beam_sun = float3(0.0f, 0.0f, 0.0f);
    o.beam_grid = 0.0f;
    if (v.look.y > 0.0f) {
        float3 at = v.centre + toward_cam(v.centre) * FX_LIGHT_TOWARD_CAMERA;
        o.light = fx_look(v.look, fx_scene(at, fx_grid(at)));
    } else {
        o.light = (float3)v.look.x;
    }
    fx_checked(o, o.light * t, v.check);
    return o;
}

float fog_amount_gui(float d) {
    if (tone_fog.z > 2.5f) {
        return 1.0f - saturate(exp2(tone_fog.x - tone_fog.y * d));
    }
    if (tone_fog.z > 1.5f) {
        return 1.0f - saturate(exp2(tone_fog.x - tone_fog.y * d) - 1.0f);
    }
    return 1.0f - saturate(exp2((tone_fog.y - d) / max(tone_fog.y - tone_fog.x, 1e-3f)) - 1.0f);
}

float3 tonemap_gui(float3 x) {
    float3 y = max(x, float3(0.0f, 0.0f, 0.0f));
    if (tone_e_on.y > 0.5f) {
        float a = tone_curve.x;
        float b = tone_curve.y;
        float c = tone_curve.z;
        float d = tone_curve.w;
        y = y * (a * y + b) / (y * (a * y + c) + d) * tone_e_on.x;
    }
    return sqrt(saturate(y));
}

float4 fs_gui(VOut input) : SV_Target {
    float4 t = lerp(tex.Sample(samp, input.uv), tex.Sample(samp, input.uv2), input.blend);
    float4 g_col = input.color * t;
    float s = ms_load(scene_depth, (int2)floor(input.pos.xy), 0u);
    float4 c = g_col;
    if (g_col.a > 0.0f) {
        float3 lin = (g_col.rgb / g_col.a) * (g_col.rgb / g_col.a) * input.light;
        if (input.fx > 0.5f) {
            float a = g_col.a * (1.0f - fog_amount_gui(input.dist));
            if (input.soft.x > 0.0f) {
                float sd = (s > 0.0f) ? min(tone_e_on.z / max(s, 1e-9f), SOFT_DEPTH_MAX) : SOFT_DEPTH_MAX;
                float pd = min(input.vw - input.soft.y, SOFT_DEPTH_MAX - 1.0f / input.soft.x);
                a *= saturate(abs(sd - pd) * input.soft.x);
            }
            c = float4(tonemap_gui(lin * a * a), a * input.alpha_mul);
        } else {
            lin = lerp(lin, tone_fog_rgb.rgb, fog_amount_gui(input.dist));
            c = float4(tonemap_gui(lin) * g_col.a, g_col.a);
        }
    }
    float depth = (input.fx > 0.5f) ? input.pos.z : input.depth;
    if (depth < s * (1.0f - 0.0015f)) {
        discard;
    }
    return c;
}

float3 beam_light(VOut input) {
    if (!any(input.beam_sun > 0.0f)) {
        return input.light;
    }
    float visible = -1.0f;
    if (tone_sun.w > 0.5f) {
        float3 d0 = input.world - tone_sphere[0].xyz;
        float3 d1 = input.world - tone_sphere[1].xyz;
        float3 d2 = input.world - tone_sphere[2].xyz;
        int i = 3;
        if (dot(d0, d0) < tone_sphere[0].w) {
            i = 0;
        } else if (dot(d1, d1) < tone_sphere[1].w) {
            i = 1;
        } else if (dot(d2, d2) < tone_sphere[2].w) {
            i = 2;
        }
        float4 p = mul(tone_casc[i], float4(input.world, 1.0f));
        if (all(p.xy > 0.0f) && all(p.xy < 1.0f)) {
            float4 r = tone_rect[i];
            float4 m = sun_atlas.SampleLevel(sun_sampler, r.xy + p.xy * r.zw, 0.0f);
            visible = evsm_visibility(m, 2.0f * p.z - 1.0f);
        }
    }
    float voxel = 1.0f;
    uint gw, gh, gd;
    sun_grid.GetDimensions(gw, gh, gd);
    float3 q = (input.world * 0.25f - tone_grid.xyz).xzy / float3((float)gw, (float)gh, (float)gd);
    if (tone_grid.w > 0.0f && all(abs(q - 0.5f) < 0.4921875f)) {
        voxel = sun_grid.SampleLevel(sun_sampler, q, 0.0f).g;
    }
    float visibility = voxel;
    if (visible >= 0.0f) {
        visibility = (tone_sun.x > 0.5f)
            ? lerp(visible, voxel, saturate((input.dist - tone_sun.y) * tone_sun.z))
            : (voxel * visible);
    }
    return max(input.light + input.beam_sun * (visibility - input.beam_grid), float3(0.0f, 0.0f, 0.0f));
}

float4 fs_fx(VOut input) : SV_Target {
    float4 t = lerp(tex.Sample(samp, input.uv), tex.Sample(samp, input.uv2), input.blend);
    float4 g_col = input.color * t;
    float s = ms_load(scene_depth, (int2)floor(input.pos.xy), 0u);
    float depth = (input.fx > 0.5f) ? input.pos.z : input.depth;
    if (g_col.a <= 0.0f || depth < s * (1.0f - 0.0015f)) {
        discard;
    }
    float3 lin = (g_col.rgb / g_col.a) * (g_col.rgb / g_col.a) * beam_light(input);
    if (input.fx < 0.5f) {
        lin = lerp(lin, tone_fog_rgb.rgb, fog_amount_gui(input.dist));
        return float4(sqrt(max(lin, float3(0.0f, 0.0f, 0.0f))) * g_col.a, g_col.a);
    }
    lin = min(lin, (float3)tone_e_on.w);
    float a = g_col.a * (1.0f - fog_amount_gui(input.dist));
    if (input.soft.x > 0.0f) {
        float sd = (s > 0.0f) ? min(tone_e_on.z / max(s, 1e-9f), SOFT_DEPTH_MAX) : SOFT_DEPTH_MAX;
        float pd = min(input.vw - input.soft.y, SOFT_DEPTH_MAX - 1.0f / input.soft.x);
        a *= saturate(abs(sd - pd) * input.soft.x);
    }
    return float4(sqrt(lin) * a, a * input.alpha_mul);
}

// ------------------------------------------------------------------ mip

struct MipOut {
    float4 pos : SV_Position;
    float2 uv  : TEXCOORD0;
};

MipOut vs_mip(uint id : SV_VertexID) {
    MipOut o;
    fullscreen_tri(id, o.pos, o.uv);
    return o;
}

float4 fs_mip(MipOut input) : SV_Target {
    return src.SampleLevel(samp, input.uv, 0.0f);
}

// --------------------------------------------------------- FX sqrt buffer

float4 vs_glass_mask(uint id : SV_VertexID) : SV_Position {
    float4 pos;
    float2 uv;
    fullscreen_tri(id, pos, uv);
    return pos;
}

float4 fs_in(float4 pos : SV_Position) : SV_Target {
    float4 c = src.Load(int3((int2)pos.xy, 0));
    return float4(sqrt(max(c.rgb, float3(0.0f, 0.0f, 0.0f))), c.a);
}

float4 fs_out(float4 pos : SV_Position) : SV_Target {
    float4 c = src.Load(int3((int2)pos.xy, 0));
    return float4(c.rgb * c.rgb, c.a);
}
