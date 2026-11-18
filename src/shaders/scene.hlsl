#define SCENE_GROUP0
#include "common.hlsli"

// Pass-specific constant buffers at b2 (mutually exclusive entry points).
cbuffer LFaceCB : register(b2) {
    float4x4 lface_vp;
    float4 lface_pos;
    float4 lface_axis;
};

cbuffer SunPassCB : register(b2) {
    float4x4 sunp_vp;
    float4 sunp_rect;
    float4 sunp_hw[3];
    float4 sunp_vw[2];
    float4 sunp_vo;
    float4 sunp_centre;
};

cbuffer TerrainParamsCB : register(b2) {
    float4 tp_p[276];
};

// Material / texture bindings (@group(1)/@group(2) in WGSL).
Texture2D<float4> tex : register(t15);
Texture2D<float4> emissive_tex : register(t16);
Texture2D<float4> surface_tex : register(t17);
SamplerState tex_sampler : register(s3);

TEX_COLOR_MS see_scene : register(t18);
Texture2D<float4> glass_scene : register(t19);
Texture2D<float> glass_depth : register(t20);
TEX_DEPTH_MS glass_depth_ms : register(t21);

// ------------------------------------------------------------------ sky

struct SkyOut {
    float4 pos : SV_Position;
    float2 ndc : TEXCOORD0;
};

SkyOut vs_sky(uint id : SV_VertexID) {
    float x = (float)((int)(id & 1u) * 4 - 1);
    float y = (float)((int)(id >> 1u) * 4 - 1);
    SkyOut o;
    o.pos = float4(x, y, 1.0f, 1.0f);
    o.ndc = float2(x, y);
    return o;
}

float hash3(float3 p) {
    return frac(sin(dot(p, float3(12.9898f, 78.233f, 37.719f))) * 43758.5453f);
}

float4 fs_sky(SkyOut input) : SV_Target {
    float4 far_p = mul(g.inv_view_proj, float4(input.ndc, 1.0f, 1.0f));
    float3 dir = normalize(far_p.xyz / far_p.w);
    float3 c = sky_color_lod_k(dir, 0.0f, sky_level(dir));
    if (under_water()) {
        return float4(fog_rgb(), 1.0f);
    }
    if (g.sky_rot.x > 0.5f && dir.y > 0.0f) {
        float3 cell = floor(dir * 300.0f);
        float s = step(0.9985f, hash3(cell)) * pow(abs(max(hash3(cell + float3(7.31f, 1.7f, 3.9f)), 0.0f)), 4.0f);
        c += (float3)(s * 0.6f) * (1.0f - g.fog.w) * atmos_t(dir);
    }
    return float4(c, 1.0f);
}

// ---------------------------------------------------------------- parts

struct PartIn {
    float3 pos        : TEXCOORD0;
    float3 normal     : TEXCOORD1;
    float4 r0         : TEXCOORD2;
    float4 r1         : TEXCOORD3;
    float4 r2         : TEXCOORD4;
    float4 size       : TEXCOORD5;
    float4 color      : TEXCOORD6;
    float4 extra      : TEXCOORD7;
    float2 uv         : TEXCOORD8;
    float4 uvt        : TEXCOORD9;
    float4 emissive   : TEXCOORD10;
    float4 vcol       : TEXCOORD11;
    float4 projection : TEXCOORD12;
    float4 tint       : TEXCOORD13;
};

struct PartOut {
    float4 clip                   : SV_Position;
    float3 world                  : TEXCOORD0;
    float3 normal                 : TEXCOORD1;
    float4 color                  : TEXCOORD2;
    float4 extra                  : TEXCOORD3;
    float3 local                  : TEXCOORD4;
    float2 uv                     : TEXCOORD5;
    float3 emissive               : TEXCOORD6;
    float emissive_albedo         : TEXCOORD7;
    float3 half_ext               : TEXCOORD8;
    float3 objn                   : TEXCOORD9;
    float4 mscale                 : TEXCOORD10;
    nointerpolation uint oblique  : TEXCOORD11;
    float4 vtint                  : TEXCOORD12;
    nointerpolation float4 tint   : TEXCOORD13;
};

float detile_noise(float2 uv) {
    float2 p = uv * 32.0f - 0.5f;
    float2 f = frac(p);
    int2 i = (int2)floor(p);
    float v0 = 0.0f, v1 = 0.0f, v2 = 0.0f, v3 = 0.0f;
    [unroll]
    for (int k = 0; k < 4; k++) {
        int2 c = (i + int2(k & 1, k >> 1)) & 31;
        uint n = (uint)(c.y * 32 + c.x);
        float val = lane4(mat_params[5u * MAT_PARAM_LAYERS + n / 4u], n % 4u);
        if (k == 0) v0 = val;
        else if (k == 1) v1 = val;
        else if (k == 2) v2 = val;
        else v3 = val;
    }
    return lerp(lerp(v0, v1, f.x), lerp(v2, v3, f.x), f.y);
}

float2 wang_uv(float2 uv) {
    float2 tile = uv * WANG_CELLS;
    int2 cell = ((int2)floor(tile)) & 127;
    uint n = (uint)(cell.y * 128 + cell.x);
    uint packed = (uint)lane4(mat_params[5u * MAT_PARAM_LAYERS + 256u + n / 16u], (n / 4u) % 4u);
    uint index = (packed >> ((n % 4u) * 4u)) & 15u;
    return (float2((float)(index & 3u), (float)(index >> 2u)) + frac(tile)) / WANG_CELLS;
}

float3 mat_normal(float4 t) {
    float2 xy = t.rg * 2.0f - 1.0f;
    return float3(xy, sqrt(saturate(1.0f - dot(xy, xy))));
}

float3 part_world(PartIn v) {
    float3 l = v.pos * v.size.xyz;
    float3 origin = float3(v.r0.w, v.r1.w, v.r2.w);
    return v.r0.xyz * l.x + v.r1.xyz * l.y + v.r2.xyz * l.z + origin;
}

PartOut part_out(PartIn v) {
    float3 world = part_world(v);
    float3 s = max(v.size.xyz, float3(1e-4f, 1e-4f, 1e-4f));
    float3 n = v.r0.xyz * (v.normal.x / s.x) + v.r1.xyz * (v.normal.y / s.y) + v.r2.xyz * (v.normal.z / s.z);
    PartOut o;
    o.clip = mul(g.view_proj, float4(world, 1.0f));
    o.clip.z = o.clip.z * (1.0f + v.size.w);
    o.world = world;
    o.normal = normalize(n);
    o.color = v.color;
    o.vtint = float4(1.0f, 1.0f, 1.0f, 1.0f);
    if (v.vcol.a > 0.5f) {
        o.color = float4(sq(v.vcol.rgb), v.color.a);
    } else if (v.vcol.a < -0.5f) {
        o.vtint = float4(v.vcol.rgb, -v.vcol.a - 1.0f);
    }
    o.extra = v.extra;
    o.local = v.pos * v.size.xyz;
    o.uv = v.uv * v.uvt.xy + ((v.uvt.w < 0.0f) ? float2(0.0f, 0.0f) : v.uvt.zw);
    if (v.uvt.w < 0.0f && v.uvt.w > -2.5f) {
        float3 p = o.local;
        bool modern = v.projection.w >= 9.0f;
        uint tag = (uint)v.projection.w - (modern ? 8u : 0u);
        float2 uv = v.uv;
        if (tag == 1u) { uv = float2(-p.z, -p.y); }
        else if (tag == 2u) { uv = float2(-p.x, -p.z); }
        else if (tag == 3u) { uv = float2(p.x, -p.y); }
        else if (tag == 4u) { uv = float2(p.z, -p.y); }
        else if (tag == 5u) { uv = float2(p.x, -p.z); }
        else if (tag == 6u) { uv = float2(-p.x, -p.y); }
        if (v.uvt.w < -1.5f || all(v.projection.xyz == float3(0.0f, 0.0f, 0.0f))) {
            float3 source_normal = (v.uvt.w < -1.5f)
                ? (v.normal / max(v.projection.xyz, float3(1e-4f, 1e-4f, 1e-4f)))
                : v.normal;
            float3 a = abs(source_normal);
            uv = float2(p.x, -p.y);
            if (a.x >= a.y && a.x >= a.z) {
                uv = float2((v.normal.x > 0.0f) ? -p.z : p.z, -p.y);
            } else if (a.y >= a.z) {
                uv = float2((v.normal.y > 0.0f) ? -p.x : p.x, -p.z);
            } else if (v.normal.z < 0.0f) {
                uv = float2(-p.x, -p.y);
            }
        }
        o.uv = uv * v.uvt.xy;
    }
    o.emissive = v.emissive.rgb;
    o.emissive_albedo = v.emissive.w;
    o.tint = v.tint;
    o.half_ext = v.size.xyz * 0.5f;
    o.objn = v.normal;
    o.oblique = (v.projection.x == -99.0f) ? 1u : 0u;
    o.mscale = v.uvt;
    return o;
}

PartOut vs_part(PartIn v) {
    return part_out(v);
}

float4 glass_scene_at(float3 world, float3 normal, float alpha) {
    float3 delta = g.cam_pos.xyz - world;
    float eta = lerp(0.66f, 1.0f, (1.0f - alpha) * (1.0f - alpha));
    float4 projected = mul(g.view_proj, float4(world, 1.0f));
    float4 displaced = mul(g.view_proj, float4(g.cam_pos.xyz -
        (delta * (1.0f + 3.0f * eta / max(dot(delta, normal), 0.01f)) + normal * (3.0f * (1.0f - eta))), 1.0f));
    float2 uv = projected.xy / projected.w * float2(0.5f, -0.5f) + 0.5f;
    float2 refracted = displaced.xy / displaced.w * float2(0.5f, -0.5f) + 0.5f;
    float2 edge = refracted - 0.5f;
    float2 offset = (refracted - uv) * saturate(1.0f - 4.0f * edge * edge);
    float2 size = (float2)tex_dims2d(glass_scene);
    float2 bias = offset / max(length(offset), 1e-8f) / size;
    float behind = 1e9f;
    [unroll]
    for (int side = -1; side <= 1; side += 2) {
        float2 test_uv = saturate(refracted + (float)side * bias);
        int2 pixel = clamp((int2)(test_uv * size), int2(0, 0), (int2)size - 1);
        float z = glass_depth.Load(int3(pixel, 0));
        float4 pos = mul(g.inv_view_proj, float4(test_uv * float2(2.0f, -2.0f) + float2(-1.0f, 1.0f), z, 1.0f));
        float4 clip = mul(g.view_proj, float4(pos.xyz / pos.w + g.cam_pos.xyz, 1.0f));
        behind = min(behind, (z == 0.0f) ? 1e9f : clip.w);
    }
    return glass_scene.SampleLevel(sky_sampler, uv + offset * saturate(behind - projected.w), 0.0f);
}

static const float NEON_GLOW = 13.286025f;

float4 neon_over_scene(float3 glow, float a, float2 frag) {
    float k = 0.25f * max(g.params.y, 1e-4f);
    int2 size = tex_dims2d(glass_scene);
    float3 behind = max(glass_scene.Load(int3(clamp((int2)frag, int2(0, 0), size - 1), 0)).rgb, float3(0.0f, 0.0f, 0.0f));
    float3 e = a * sqrt(saturate(glow * k))
        + (1.0f - a) * sqrt(saturate(behind * k));
    float3 blended = e * e / k;
    return float4(max(blended - (1.0f - a) * behind, float3(0.0f, 0.0f, 0.0f)), a);
}

static const float WATER_SUN = 1.0f;
static const float WATER_BELOW_ALPHA = 0.9f;
static const float WATER_INSIDE_MIN_UP = 0.5f;

float3 surface_shade(uint packed_faces, float3 objn, float3 local_p, float3 half_ext, float3 ddx_l, float3 ddy_l) {
    float3 a = abs(objn);
    uint face = 0u;
    float2 uv = float2(0.0f, 0.0f);
    float2 gx = float2(0.0f, 0.0f);
    float2 gy = float2(0.0f, 0.0f);
    float3 c = local_p + half_ext;
    if (a.x >= a.y && a.x >= a.z) {
        face = (objn.x > 0.0f) ? 0u : 3u;
        uv = c.zy;
        gx = ddx_l.zy;
        gy = ddy_l.zy;
    } else if (a.y >= a.z) {
        face = (objn.y > 0.0f) ? 1u : 4u;
        uv = c.xz;
        gx = ddx_l.xz;
        gy = ddy_l.xz;
    } else {
        face = (objn.z > 0.0f) ? 2u : 5u;
        uv = c.xy;
        gx = ddx_l.xy;
        gy = ddy_l.xy;
    }
    uint band = (packed_faces >> (3u * face)) & 7u;
    float3 res = float3(1.0f, 1.0f, 1.0f);
    [branch]
    if (band != 0u) {
        float2 k = float2(0.5f, 1.0f / 32.0f);
        float2 at = float2(uv.x * 0.5f, ((float)(band - 1u) + frac(uv.y / 8.0f)) * 0.25f);
        float t = studs_tex.SampleGrad(studs_sampler, at, gx * k, gy * k).r;
        res = sq((float3)(t * 2.0f));
    }
    return res;
}

float2 face_studs(float3 objn, float3 local_p, float3 half_ext, uint oblique) {
    float3 a = abs(objn);
    float3 c = local_p + half_ext;
    if (oblique != 0u) {
        float3 n = normalize(objn / max(half_ext, float3(0.0001f, 0.0001f, 0.0001f)));
        float3 u = normalize(cross(n, float3(0.0f, 1.0f, 0.0f)));
        float3 v = cross(u, n);
        return float2(dot(c, u), dot(c, v));
    }
    if (a.x >= a.y && a.x >= a.z) {
        return float2(c.z, 2.0f * half_ext.y - c.y);
    } else if (a.y >= a.z) {
        return c.xz;
    }
    return float2(c.x, 2.0f * half_ext.y - c.y);
}

float4 fs_decal_depth(PartOut input) : SV_Target {
    bool decal_clamp = all(abs(input.mscale - float4(1.0f, 1.0f, 0.0f, 0.0f)) < 1e-4f);
    float2 edge_px = 0.5f / (float2)tex_dims2d(tex);
    float2 suv = decal_clamp ? clamp(input.uv, edge_px, 1.0f - edge_px) : input.uv;
    float alpha = tex.Sample(tex_sampler, suv).a * input.color.a;
    if ((uint)(input.extra.w + 0.5f) != 2u || alpha < 1.0f) {
        discard;
    }
    return float4(0.0f, 0.0f, 0.0f, 0.0f);
}

static const float SEE_Q = 0.525f;

float see_k() {
    return 0.25f * max(g.params.y, 1e-4f);
}

float3 see_enc(float3 c) {
    return pow(abs(saturate(c * see_k())), SEE_Q);
}

float3 see_dec(float3 e) {
    return pow(abs(saturate(e)), 1.0f / SEE_Q) / see_k();
}

float4 see_out(float3 c, float w, float a, bool enc_out) {
    if (enc_out) {
        return float4(see_enc(c) * w, max(a, 1.0f / 1024.0f));
    }
    return float4(c * w, a);
}

float4 part_shade_impl(PartOut input, bool front, bool enc_out, bool opaque_pass) {
    uint code = (uint)(input.extra.x + 0.5f);
    float kind = (float)(code & 7u);
    int layer = (int)(code >> 4u) - 1;
    bool material_lq = (layer >= 0) && (g.env_spec.z <= 0.5f) && (g.sun_color.w > 0.5f);

    float2 muv = face_studs(input.objn, input.local, input.half_ext, input.oblique) * input.mscale.xy + input.mscale.zw;
    if ((code & 8u) != 0u) {
        muv = input.uv;
    }
    float2 mdx = ddx(muv);
    float2 mdy = ddy(muv);
    float3 wdx = ddx(input.world);
    float3 wdy = ddy(input.world);
    float2 udx = ddx(input.uv);
    float2 udy = ddy(input.uv);
    float3 ddx_l = ddx(input.local);
    float3 ddy_l = ddy(input.local);

    uint mode = (uint)(input.extra.w + 0.5f);
    float3 n = normalize(input.normal);
    if (dot(n, g.cam_pos.xyz - input.world) < 0.0f) {
        n = -n;
    }
    float3 vtint = sq(input.vtint.rgb);
    float3 base = input.color.rgb * vtint;
    float alpha = opaque_pass ? 1.0f : input.color.a;
    bool translucent = !opaque_pass && alpha < 0.999f;
    if (translucent) {
        alpha = alpha * input.vtint.a;
    }

    float3 t_raw = float3(0.0f, 0.0f, 0.0f);
    float4 t = float4(0.0f, 0.0f, 0.0f, 1.0f);
    float emask = 0.0f;
    float mat_rough = -1.0f;
    float mat_metal = 0.0f;

    [branch]
    if (mode != 0u || (!opaque_pass && kind == 6.0f)) {
        bool decal_clamp = (mode == 2u) && all(abs(input.mscale - float4(1.0f, 1.0f, 0.0f, 0.0f)) < 1e-4f);
        float2 edge_px = 0.5f / (float2)tex_dims2d(tex);
        float2 suv = decal_clamp ? clamp(input.uv, edge_px, 1.0f - edge_px) : input.uv;
        float4 t_s = tex.SampleGrad(tex_sampler, suv, udx, udy);
        t_raw = linear_to_srgb(t_s.rgb);
        t = float4(t_raw * t_raw, t_s.a);

        if (mode == 1u || mode >= 3u) {
            emask = emissive_tex.SampleGrad(tex_sampler, input.uv, udx, udy).r;
            if (tex_dims2d(surface_tex).x > 1) {
                float4 sa_map = surface_tex.SampleGrad(tex_sampler, input.uv, udx, udy);
                mat_rough = sa_map.b;
                mat_metal = sa_map.a;
                float det = udx.x * udy.y - udx.y * udy.x;
                if (abs(det) > 1e-10f) {
                    float3 tangent = (wdx * udy.y - wdy * udx.y) / det;
                    float3 bitangent = (wdy * udx.x - wdx * udy.x) / det;
                    float3 tn = mat_normal(sa_map);
                    float3 tt = normalize(tangent - n * dot(n, tangent));
                    float3 bb = normalize(bitangent - n * dot(n, bitangent));
                    n = normalize(tt * tn.x - bb * tn.y + n * tn.z);
                }
            }
        }
    }

    float4 out_res = float4(0.0f, 0.0f, 0.0f, 0.0f);
    if (!opaque_pass && kind == 6.0f) {
        float3 view = normalize(g.cam_pos.xyz - input.world);
        float rim = 1.0f - saturate(2.0f * abs(dot(n, view)));
        float phase = abs(cos(0.3f * g.cam_pos.w) * cos(g.cam_pos.w));
        float red = t_raw.r;
        float pulse = (mode != 0u && red > 0.0f) ? ((1.0f - saturate(13.0f * abs(red - phase))) * t.a) : 0.0f;
        float edge = max(rim, pulse);
        float cover = alpha * max(lerp(pulse, 1.0f, edge) * edge, 0.05f);
        float fog = fog_amount(length(input.world - g.cam_pos.xyz));
        out_res = see_out(lerp(base, fog_rgb_at(input.world), fog), cover, cover * (1.0f - fog), enc_out);
    } else {

    [branch]
    if (mode != 0u) {
        bool surface = input.tint.w > 0.5f;
        float3 sa_raw = t_raw * input.tint.rgb * sqrt(max(input.emissive_albedo, 0.0f));
        if (mode == 1u && surface) {
            float3 under = sqrt(max(input.color.rgb, float3(0.0f, 0.0f, 0.0f))) * input.vtint.rgb;
            float3 raw = lerp(under, sa_raw, t.a);
            base = raw * raw;
        } else if (mode == 1u) {
            base = lerp(base, t.rgb * input.emissive_albedo * vtint, t.a);
        } else if (mode == 6u) {
            float3 raw = t_raw * lerp(float3(1.0f, 1.0f, 1.0f), input.tint.rgb, t.a) * sqrt(max(input.emissive_albedo, 0.0f));
            base = raw * raw;
        } else if (mode == 2u) {
            base = t.rgb * base;
            if (!opaque_pass) { alpha = alpha * t.a; }
        } else if (mode == 3u) {
            base = t.rgb * input.emissive_albedo * vtint;
            if (translucent) { alpha = alpha * t.a; }
        } else if (mode == 5u) {
            base = t.rgb * input.color.rgb * vtint;
            if (translucent) { alpha = alpha * t.a; }
        } else if (mode == 4u) {
            base = surface ? (sa_raw * sa_raw) : (t.rgb * input.emissive_albedo * vtint);
            if (!opaque_pass) { alpha = input.color.a * input.vtint.a * t.a; }
        }
    }

    float3 material_emission = float3(0.0f, 0.0f, 0.0f);
    float glass_alpha = alpha;
    float glass_scene_alpha = -1.0f;
    [branch]
    if (mode == 0u && layer >= 0) {
        float4 mp0 = float4(-1.0f, 0.0f, 0.0f, 1.0f);
        float4 mp1 = float4(0.0f, 0.0f, 0.0f, 0.0f);
        float4 mf = float4(1.0f, 1.0f, 1.0f, 0.0f);
        float4 mc = float4(0.0f, 0.0f, 0.0f, 0.0f);
        float4 ms = float4(1.0f, 1.0f, 1.0f, 0.0f);
        if ((uint)layer < MAT_PARAM_LAYERS) {
            mp0 = mat_params[5u * (uint)layer];
            mp1 = mat_params[5u * (uint)layer + 1u];
            mf = mat_params[5u * (uint)layer + 2u];
            mc = mat_params[5u * (uint)layer + 3u];
            ms = mat_params[5u * (uint)layer + 4u];
        }
        uint shader_type = (uint)(mp1.z + 0.5f);
        bool lq = material_lq;
        // D3D11 SV_Position.w is clip.w directly (studs), unlike WGSL's 1/clip.w.
        float depth = input.clip.w;
        float2 fades = saturate(1.0f - depth * float2(1.0f / MATERIAL_DIFFUSE_FADE_DEPTH, ENV_SPEC_FADE));
        float4 a;
        float4 su;
        float3 nm;
        float2 duv = muv;
        float dk = 1.0f;
        [branch]
        if (mp1.y > 0.5f) {
            float k = detile_noise(muv * mp1.x) * DETILE_LEVELS;
            float f = frac(k);
            float i = floor(k);
            float2 ua = muv + sin(float2(3.0f, 7.0f) * i);
            float2 ub = muv + sin(float2(3.0f, 7.0f) * (i + 1.0f));
            a = lerp(mat_albedo.SampleGrad(studs_sampler, float3(ua, (float)layer), mdx, mdy),
                     mat_albedo.SampleGrad(studs_sampler, float3(ub, (float)layer), mdx, mdy), f);
            float4 sa = mat_surface.SampleGrad(studs_sampler, float3(ua, (float)layer), mdx, mdy);
            float4 sb = mat_surface.SampleGrad(studs_sampler, float3(ub, (float)layer), mdx, mdy);
            su = lerp(sa, sb, f);
            nm = lerp(mat_normal(sa), mat_normal(sb), f);
        } else if (shader_type == 6u) {
            duv = muv * mp0.w;
            dk = mp0.w;
            float2 atlas = wang_uv(duv);
            a = mat_albedo.SampleGrad(studs_sampler, float3(atlas, (float)layer), mdx * dk, mdy * dk);
            su = mat_surface.SampleGrad(studs_sampler, float3(atlas, (float)layer), mdx * dk, mdy * dk);
            nm = mat_normal(su);
        } else {
            duv = muv * mp0.w;
            dk = mp0.w;
            a = mat_albedo.SampleGrad(studs_sampler, float3(duv, (float)layer), mdx * dk, mdy * dk);
            su = mat_surface.SampleGrad(studs_sampler, float3(duv, (float)layer), mdx * dk, mdy * dk);
            [branch]
            if (lq || shader_type == 7u || shader_type == 8u) {
                float cutoff = lq ? MATERIAL_LQ_DIFFUSE_CUTOFF : mc.x;
                float2 uv = lq ? muv : duv;
                float2 dx = lq ? mdx : (mdx * dk);
                float2 dy = lq ? mdy : (mdy * dk);
                float4 far = mat_albedo.SampleGrad(studs_sampler, float3(uv * mf.x, (float)layer), dx * mf.x, dy * mf.x);
                float4 near = mat_albedo.SampleGrad(studs_sampler, float3(uv, (float)layer), dx, dy);
                a = lerp(far, near, saturate((fades.x - cutoff) * ms.x));
            }
            [branch]
            if ((!lq && shader_type == 8u) || shader_type == 10u || shader_type == 11u) {
                float4 sn = mat_surface.SampleGrad(studs_sampler, float3(duv * mf.y, (float)layer), mdx * dk * mf.y, mdy * dk * mf.y);
                float4 ss = mat_surface.SampleGrad(studs_sampler, float3(duv * mf.z, (float)layer), mdx * dk * mf.z, mdy * dk * mf.z);
                su = float4(lerp(sn.rg, su.rg, saturate((fades.y - mc.y) * ms.y)),
                            lerp(ss.ba, su.ba, saturate((fades.y - mc.z) * ms.z)));
            }
            nm = mat_normal(su);
        }
        mat_rough = su.b;
        mat_metal = su.a;
        if (!opaque_pass && (shader_type == 10u || shader_type == 11u)) {
            glass_alpha = saturate(2.0f * alpha - 1.0f + a.a);
        }
        float tint_k = (mp1.w > 0.5f && shader_type != 10u && shader_type != 11u) ? a.a : 1.0f;
        base = sq(lerp(float3(1.0f, 1.0f, 1.0f), sqrt(max(base, float3(0.0f, 0.0f, 0.0f))), tint_k) * a.rgb);
        [branch]
        if (shader_type == 9u) {
            float mask = mat_emission.SampleGrad(studs_sampler, float3(duv, (float)layer), mdx * dk, mdy * dk).r;
            material_emission = float3(mf.w, mc.w, ms.w) * mask;
        }
        float2 xy = nm.xy;
        float z = nm.z * ((shader_type == 10u || shader_type == 11u) ? 10.0f : 1.0f);
        [branch]
        if (mp0.x >= 0.0f && mp0.z != 0.0f) {
            float t_scale = mp0.y * dk;
            float4 dn = mat_surface.SampleGrad(studs_sampler, float3(duv * mp0.y, (float)(int)mp0.x), mdx * t_scale, mdy * t_scale);
            xy += (dn.rg * 2.0f - 1.0f) * mp0.z;
        }
        xy *= fades.y;
        float det = mdx.x * mdy.y - mdx.y * mdy.x;
        if (!lq && abs(det) > 1e-20f) {
            float3 tg = (wdx * mdy.y - wdy * mdx.y) / det;
            float3 bt = (wdy * mdx.x - wdx * mdy.x) / det;
            float3 tt = normalize(tg - n * dot(n, tg));
            float3 bb = normalize(bt - n * dot(n, bt));
            n = normalize(tt * xy.x - bb * xy.y + n * z);
        }
    }

    uint packed_flags = (uint)(input.extra.y + 0.5f);
    uint faces = (packed_flags >> 1u) & 0x3FFFFu;
    [branch]
    if (faces != 0u) {
        base *= surface_shade(faces, input.objn, input.local, input.half_ext, ddx_l, ddy_l);
    }
    if ((packed_flags & 1u) == 1u) {
        float k = frac((input.world.x + input.world.y + input.world.z) * 0.25f);
        base = lerp(base, base * 0.6f + float3(0.18f, 0.12f, 0.3f), step(0.5f, k) * 0.6f);
    }
    if (g.lamp1.w > 1.5f) {
        float3 lit = min(g.ambient.rgb + g.sun_color.rgb * max(dot(n, normalize(g.sun_dir.xyz)), 0.0f), float3(1.0f, 1.0f, 1.0f));
        out_res = see_out(lit * base, alpha, alpha, enc_out);
    } else if (!opaque_pass && kind > 4.5f && kind < 5.5f) {
        float3 up = normalize(input.normal);
        bool below = dot(up, g.cam_pos.xyz - input.world) < 0.0f;
        if (under_water() && (!below || up.y < WATER_INSIDE_MIN_UP)) {
            discard;
        }
        float3 vd = normalize(input.world - g.cam_pos.xyz);
        float shadow = sun_shadow(input.world, up);
        float3 lit = base * (g.ambient.rgb + g.outdoor.rgb * light_grid(input.world, up) * sky_gain()
            + g.sun_color.rgb * g.sun_dir.w * shadow * WATER_SUN);
        float fres = saturate((saturate(0.78f - 2.5f * abs(dot(n, vd))) + 0.3f) * input.emissive.x);
        float3 c_w;
        if (below) {
            c_w = lit;
            alpha = WATER_BELOW_ALPHA;
        } else {
            c_w = lerp(lit, sky_color(reflect(vd, n)), fres);
            alpha = saturate(1.0f - input.emissive.y * (1.0f - fres));
        }
        [branch]
        if (g.fog.z > 0.5f) {
            float d = length(input.world - g.cam_pos.xyz);
            float f = fog_amount(d);
            c_w = lerp(c_w, fog_rgb_at(input.world), f);
        }
        out_res = see_out(c_w, alpha, alpha, enc_out);
    } else {
        float3 c = float3(0.0f, 0.0f, 0.0f);
        bool debug_lit_exit = false;
        [branch]
        if (kind > 0.5f && kind < 1.5f) {
            c = base * base * NEON_GLOW;
        } else {
            float3 l = normalize(g.sun_dir.xyz);
            float ndl = max(dot(n, l), 0.0f);
            float shadow = (ndl > 0.0f) ? sun_shadow(input.world, normalize(input.normal)) : 1.0f;
            float sv = light_grid(input.world, normalize(input.normal));
            bool is_glass = !opaque_pass && (kind > 2.5f) && (kind < 3.5f);
            float3 v = normalize(g.cam_pos.xyz - input.world);
            float nv = max(dot(n, v), 1e-4f);
            float fade = saturate(1.0f - length(g.cam_pos.xyz - input.world) * ENV_SPEC_FADE);
            float sS = (g.env_spec.z > 0.5f) ? (g.sky_grad[1].w * fade) : 0.0f;
            float rough = (mat_rough >= 0.0f) ? (ENV_ROUGH_BASE + ENV_ROUGH_SCALE * mat_rough) : g.env_spec.y;
            float refl = input.extra.z - 2.0f * step(1.5f, input.extra.z);
            bool reflective = (refl > 0.0f) && !is_glass;
            float metal = (reflective || is_glass) ? 0.0f : (mat_metal * sS);
            float3 f0 = lerp(float3(0.04f, 0.04f, 0.04f), base, metal);
            float2 ab = brdf_ab(rough, nv);
            float3 fe = (f0 * ab.x + ab.y) / max(ab.x + ab.y, 1e-4f);
            float kd = 1.0f - metal;

            float3 sun_spec = float3(0.0f, 0.0f, 0.0f);
            float3 fs = float3(0.0f, 0.0f, 0.0f);
            [branch]
            if (g.env_spec.z > 0.5f) {
                float3 h = normalize(v + l);
                float nh = max(dot(n, h), 0.001f);
                float lh = dot(l, h);
                float e = pow(abs(max(1.0f - lh, 0.0f)), 5.0f);
                fs = float3(e, e, e) + f0 * (1.0f - e);
                float a = pow(abs(max(rough, 0.0f)), 4.0f);
                float d = (nh * a - nh) * nh + 1.0f;
                float dterm = (a + a * a) / (d * d * (lh * 3.0f + 0.5f) * (nh * 0.75f + 0.25f));
                sun_spec = fs * dterm * ndl * shadow * g.sun_color.rgb * g.sun_dir.w * sun_cover();
            }
            float3 sun_light = g.sun_color.rgb * g.sun_dir.w * ndl * shadow;
            float nl = dot(n, l);
            bool voxel_sun = (g.sun_color.w > 0.5f) && (g.env_spec.z <= 0.5f);
            float back = (nl < 0.0f && g.bloom.z < 0.5f) ? (-nl) : 0.0f;
            if (voxel_sun && nl < 0.0f) {
                back = -nl * sun_shadow(input.world, normalize(input.normal));
            }
            float3 env_part = material_lq ? float3(0.0f, 0.0f, 0.0f) : ((kd - fe * sS * kd) * env_cube(n, sv));
            float3 light = env_part
                + g.ambient.rgb + g.outdoor.rgb * sv * sky_gain()
                + (kd - fs * sS * kd) * sun_light
                + g.lamp1.rgb * kd * back;
            LocalPbr local = local_pbr(input.world, (g.sun_color.w > 0.5f) ? normalize(input.normal) : n, v, rough, f0, kd, sS);
            light += local.diffuse;
            sun_spec += local.specular;

            if (g.sky_rot.z > 4.5f) {
                float dm = g.sky_rot.z;
                float3 dl = light;
                if (dm > 5.5f && dm < 6.5f) { dl = local.diffuse; }
                else if (dm > 6.5f && dm < 7.5f) { dl = (kd - fs * sS * kd) * sun_light; }
                else if (dm > 10.5f) { dl = (float3)sv; }
                else if (dm > 9.5f) { dl = (float3)light_grid_sun(input.world, n); }
                else if (dm > 8.5f) { dl = (float3)max(sun_shadow_map(input.world), 0.0f); }
                else if (dm > 7.5f) { dl = light - local.diffuse - (kd - fs * sS * kd) * sun_light; }
                out_res = float4(dl * 0.25f, 1.0f);
                debug_lit_exit = true;
            } else {
                float3 albedo = base;
                [branch]
                if (reflective) {
                    albedo = lerp(base, env_reflect(reflect(-v, n), rough, 1.0f, true), refl);
                }
                c = light * albedo;
                if (!is_glass) {
                    c += env_reflect(reflect(-v, n), rough, sv, reflective) * fe * sS + sun_spec;
                } else {
                    float a = glass_alpha;
                    float3 reflection = env_reflect(reflect(-v, n), rough, sv, false)
                        * lerp(float3(1.0f, 1.0f, 1.0f), base, 0.5f);
                    float grazing = 0.8f * (1.0f - nv) * (1.0f - nv) * saturate(2.0f * alpha);
                    float4 scene = float4(0.0f, 0.0f, 0.0f, 0.0f);
                    [branch]
                    if (alpha < 0.999f) {
                        scene = glass_scene_at(input.world, n, alpha);
                    }
                    float3 tinted = scene.rgb * lerp(float3(1.0f, 1.0f, 1.0f), sqrt(max(input.color.rgb, float3(0.0f, 0.0f, 0.0f))), saturate(2.0f * alpha));
                    float3 lit = lerp(light * base * a, reflection, refl);
                    c = lerp(lerp(tinted, lit, a), reflection, grazing)
                        + env_reflect(reflect(-v, n), rough, sv, false) * fe * sS * a + sun_spec;
                    float energy = dot(tinted, float3(1.0f, 1.0f, 1.0f)) / (dot(scene.rgb, float3(1.0f, 1.0f, 1.0f)) + 0.01f);
                    glass_scene_alpha = lerp(lerp(lerp(1.0f, scene.a, energy), 1.0f, a), 1.0f, grazing);
                    alpha = 1.0f;
                }
            }
        }

        if (!debug_lit_exit) {
            c += material_emission * base;
            if ((mode == 1u || mode >= 3u) && any(input.emissive > 0.0f)) {
                c += t.rgb * emask * input.emissive;
            }
            if (g.sky_rot.z > 0.5f && g.sky_rot.z < 1.5f) {
                c = sq(n * 0.5f + 0.5f);
            } else if (g.sky_rot.z > 1.5f && g.sky_rot.z < 2.5f) {
                c = base;
            } else if (g.sky_rot.z > 2.5f && g.sky_rot.z < 3.5f) {
                c = sq(float3(frac(input.uv), 0.0f));
            } else if (g.sky_rot.z > 3.5f && g.sky_rot.z < 4.5f) {
                c = (float3)light_grid(input.world, n);
            }
            c = c + float3(0.004f, 0.008f, 0.014f) * step(1.5f, input.extra.z);

            bool see_through_neon = !opaque_pass && (kind > 0.5f) && (kind < 1.5f) && (alpha < 0.999f);
            float visibility = 1.0f;
            [branch]
            if (g.fog.z > 0.5f) {
                float d = length(input.world - g.cam_pos.xyz);
                float f = fog_amount(d);
                visibility = 1.0f - f;
                c = lerp(c, fog_rgb_at(input.world), f);
            }
            if (opaque_pass) {
                out_res = float4(c, 1.0f);
            } else if (see_through_neon) {
                float a = 1.0f - visibility * (1.0f - alpha);
                out_res = enc_out ? see_out(c, a, a, true) : neon_over_scene(c, a, input.clip.xy);
            } else if (glass_scene_alpha >= 0.0f) {
                float vis = (g.fog.z > 0.5f) ? (1.0f - fog_amount(length(input.world - g.cam_pos.xyz))) : 1.0f;
                out_res = see_out(c, 1.0f, glass_scene_alpha * vis, enc_out);
            } else {
                out_res = see_out(c, alpha, alpha, enc_out);
            }
        }
    }
    }
    return out_res;
}

float4 fs_part_opaque(PartOut input, bool front : SV_IsFrontFace) : SV_Target {
    return part_shade_impl(input, front, false, true);
}

float4 fs_part(PartOut input, bool front : SV_IsFrontFace) : SV_Target {
    return part_shade_impl(input, front, false, false);
}

float4 fs_part_enc(PartOut input, bool front : SV_IsFrontFace) : SV_Target {
    return part_shade_impl(input, front, true, false);
}

#if MSAA_SAMPLES > 1
float4 fs_see_encode(float4 p : SV_Position, uint s : SV_SampleIndex) : SV_Target {
    float3 l = ms_load4(see_scene, (int2)p.xy, s).rgb;
    return float4(see_enc(max(l, float3(0.0f, 0.0f, 0.0f))), 0.0f);
}

float4 fs_see_decode(float4 p : SV_Position, uint s : SV_SampleIndex) : SV_Target {
    float4 e = ms_load4(see_scene, (int2)p.xy, s);
    if (e.a <= 0.0f) {
        discard;
    }
    return float4(see_dec(e.rgb), 1.0f);
}
#else
float4 fs_see_encode(float4 p : SV_Position) : SV_Target {
    float3 l = ms_load4(see_scene, (int2)p.xy, 0u).rgb;
    return float4(see_enc(max(l, float3(0.0f, 0.0f, 0.0f))), 0.0f);
}

float4 fs_see_decode(float4 p : SV_Position) : SV_Target {
    float4 e = ms_load4(see_scene, (int2)p.xy, 0u);
    if (e.a <= 0.0f) {
        discard;
    }
    return float4(see_dec(e.rgb), 1.0f);
}
#endif

// --------------------------------------------------------- light shadow

struct LShadowOut {
    float4 clip : SV_Position;
    float lin   : TEXCOORD0;
};

LShadowOut vs_light_shadow(PartIn v) {
    LShadowOut o;
    float3 world = part_world(v);
    o.clip = mul(lface_vp, float4(world, 1.0f));
    o.lin = dot(world - lface_pos.xyz, lface_axis.xyz) * lface_pos.w;
    if ((((uint)(v.extra.y + 0.5f) >> 19u) & 1u) == 1u) {
        o.clip = float4(2.0f, 2.0f, 2.0f, 1.0f);
    }
    return o;
}

float4 fs_light_shadow(LShadowOut input) : SV_Target {
    float z = 2.0f * saturate(input.lin) - 1.0f;
    float ep = exp(EVSM_POS_EXP * z);
    float en = -exp(-EVSM_NEG_EXP * z);
    return float4(ep, ep * ep, en, en * en);
}

// ------------------------------------------------------------- terrain

struct TerrainIn {
    float3 pos        : TEXCOORD0;
    float4 normal     : TEXCOORD1;
    uint4 palette     : TEXCOORD2;
    uint4 axis        : TEXCOORD3;
    uint4 rnd         : TEXCOORD4;
    float4 wind_local : TEXCOORD5;
};

struct TerrainOut {
    float4 clip                          : SV_Position;
    centroid float3 world                : TEXCOORD0;
    centroid float3 normal               : TEXCOORD1;
    centroid float3 bary                 : TEXCOORD2;
    centroid float4 uv01                 : TEXCOORD3;
    centroid float2 uv2                  : TEXCOORD4;
    nointerpolation uint4 palette        : TEXCOORD5;
    nointerpolation uint4 axis           : TEXCOORD6;
    nointerpolation uint4 rnd            : TEXCOORD7;
    centroid float3 grass_direct         : TEXCOORD8;
    centroid float4 grass_view           : TEXCOORD9;
};

static const float3 TERRAIN_U[18] = {
    float3(0.0f, 0.0f, -1.0f), float3(0.0f, 0.0f, 1.0f), float3(1.0f, 0.0f, 0.0f), float3(-1.0f, 0.0f, 0.0f),
    float3(0.7f, 0.0f, -0.7f), float3(-0.7f, 0.0f, -0.7f), float3(0.7f, 0.0f, 0.7f), float3(-0.7f, 0.0f, 0.7f),
    float3(-1.0f, 0.0f, 0.0f), float3(-0.7f, 0.7f, 0.0f), float3(-0.7f, -0.7f, 0.0f), float3(-1.0f, 0.0f, 0.0f), float3(-1.0f, 0.0f, 0.0f),
    float3(-1.0f, 0.0f, 0.0f), float3(-0.7f, -0.7f, 0.0f), float3(-0.7f, 0.7f, 0.0f), float3(-1.0f, 0.0f, 0.0f), float3(-1.0f, 0.0f, 0.0f)
};

static const float3 TERRAIN_V[18] = {
    float3(0.0f, -1.0f, 0.0f), float3(0.0f, -1.0f, 0.0f), float3(0.0f, -1.0f, 0.0f), float3(0.0f, -1.0f, 0.0f),
    float3(0.0f, -1.0f, 0.0f), float3(0.0f, -1.0f, 0.0f), float3(0.0f, -1.0f, 0.0f), float3(0.0f, -1.0f, 0.0f),
    float3(0.0f, 0.0f, -1.0f), float3(0.0f, 0.0f, -1.0f), float3(0.0f, 0.0f, -1.0f), float3(0.0f, 0.7f, -0.7f), float3(0.0f, -0.7f, -0.7f),
    float3(0.0f, 0.0f, 1.0f), float3(0.0f, 0.0f, 1.0f), float3(0.0f, 0.0f, 1.0f), float3(0.0f, 0.7f, 0.7f), float3(0.0f, -0.7f, 0.7f)
};

float2 terrain_uv(float3 p, uint pal, uint axis, uint rnd) {
    float4 a = tp_p[pal * 4u];
    uint i = min(axis, 17u);
    float2 uv = float2(dot(p, TERRAIN_U[i]), dot(p, TERRAIN_V[i])) * a.x;
    float r = ((float)rnd / 128.0f - 1.0f) * a.z;
    float2 turned = uv * sqrt(max(1.0f - r * r, 0.0f)) + uv.yx * float2(r, -r);
    float k = (float)rnd;
    float2 offset = float2(k, floor(k * 2.6651442f)) * a.y;
    return turned + offset;
}

static const float3 GRASS_BLADE_COLOR = float3(106.0f, 127.0f, 63.0f) / 255.0f;

float grass_hash(float2 cell) {
    float2 scaled = clamp(cell * 0.3183099031448364f, float2(-1e20f, -1e20f), float2(1e20f, 1e20f));
    float2 a = frac(scaled + float2(0.7099999785423279f, 0.11299999803304672f)) * 50.0f;
    return -1.0f + 2.0f * frac((a.x * a.y) * (a.x + a.y));
}

float grass_noise(float2 q) {
    float2 cell = floor(q);
    float2 f = frac(q);
    float2 u = (f * f) * (3.0f - 2.0f * f);
    float a = grass_hash(cell) * (1.0f - u.x) + grass_hash(cell + float2(1.0f, 0.0f)) * u.x;
    float b = grass_hash(cell + float2(0.0f, 1.0f)) * (1.0f - u.x) + grass_hash(cell + float2(1.0f, 1.0f)) * u.x;
    return a * (1.0f - u.y) + b * u.y;
}

float2 grass_wind(float2 local_p, float phase, float2 direction, float2 offset, float bias, float amplitude) {
    return (direction * 0.4f * amplitude) * (grass_noise(local_p + phase + 2.0f * offset) + bias);
}

float3 grass_position_from(TerrainIn v, bool wind, float3 eye) {
    float3 p = v.pos;
    if (v.palette.w > 0u) {
        float phase = g.cam_pos.w * 1.0f;
        if (wind && v.palette.w == 2u) {
            float2 w = grass_wind(v.wind_local.xy, phase, float2(0.5f, 0.5f), float2(0.0f, 0.0f), 0.0f, v.wind_local.z);
            p.x += w.x;
            p.z += w.y;
        }
        float d = length(eye - p);
        float height = v.wind_local.w;
        float fade_end = (v.axis.w == 1u) ? GRASS_NEAR_RADIUS : GRASS_RADIUS;
        float fade_len = (v.axis.w == 1u) ? GRASS_NEAR_FADE_LEN : GRASS_FADE_LEN;
        float visibility = 1.0f - smoothstep(fade_end - fade_len, fade_end, d);
        p.y -= (1.0f - visibility) * height;
    }
    return p;
}

float3 grass_position(TerrainIn v, bool wind) {
    return grass_position_from(v, wind, g.cam_pos.xyz);
}

float3 terrain_position(TerrainIn v) {
    return grass_position(v, true);
}

TerrainOut vs_terrain(TerrainIn v) {
    TerrainOut o;
    float3 p = terrain_position(v);
    o.clip = mul(g.view_proj, float4(p, 1.0f));
    o.world = p;
    o.normal = v.normal.xyz;
    bool grass_tint = tp_p[v.palette.x * 4u + 2u].w > 0.5f;
    float2 grass_height = grass_tint ? float2(1.2f, 0.6f) : float2(1.0f, 0.0f);
    float grass_ao = exp2((-(v.palette.w == 2u ? 0.0f : 1.0f) * grass_height.x + grass_height.y) * (2.2f - 1.7f * g.sun_dir.y));
    float3 grass_l = normalize(g.sun_dir.xyz);
    float3 grass_v = normalize(g.cam_pos.xyz - p);
    float3 grass_h = normalize(grass_l + grass_v);
    float grass_nl = abs(dot(v.normal.xyz, grass_l));
    float grass_fade = 1.0f - saturate((length(g.cam_pos.xyz - p) - GRASS_RADIUS + 40.0f) * 0.025f);
    o.grass_direct = g.sun_color.rgb * g.sun_dir.w * saturate((grass_nl + 0.9f) / 1.9f) * grass_ao;
    o.grass_view = float4(
        grass_nl * 0.5f + 0.5f,
        abs(dot(grass_h, v.normal.xyz)),
        dot(grass_v, -grass_l) * grass_fade * grass_ao,
        pow(abs(saturate(grass_h.y)), 8.0f) * grass_fade * grass_ao
    );
    uint corner = (uint)round(v.normal.w * 127.0f);
    o.bary = float3((float)(corner == 0u), (float)(corner == 1u), (float)(corner == 2u));
    o.uv01 = float4(
        terrain_uv(v.pos, v.palette.x, v.axis.x, v.rnd.x),
        terrain_uv(v.pos, v.palette.y, v.axis.y, v.rnd.y)
    );
    o.uv2 = terrain_uv(v.pos, v.palette.z, v.axis.z, v.rnd.z);
    o.palette = v.palette;
    o.axis = v.axis;
    o.rnd = v.rnd;
    return o;
}

float3 ycocg(float3 c) {
    return float3((c.r + 2.0f * c.g + c.b) * 0.25f, (c.r - c.b) * 0.5f, (2.0f * c.g - c.r - c.b) * 0.25f);
}

struct TerrainSample {
    float3 ycocg;
    float height;
    float2 nxy;
    float rough;
    float glow;
};

TerrainSample terrain_sample(float2 uv, float2 dx, float2 dy, uint pal, bool blade) {
    TerrainSample s;
    float4 a = tp_p[pal * 4u];
    int layer = (int)a.w;
    float4 t = float4(tp_p[pal * 4u + 3u].rgb, 0.5f);
    float4 su = float4(0.5f, 0.5f, 1.0f, 0.0f);
    float glow = 0.0f;
    if (blade) {
        t = float4(GRASS_BLADE_COLOR, 0.5f);
        su.a = 1.0f;
    }
    [branch]
    if (layer >= 0 && !blade) {
        float fp = max(length(dx), length(dy)) * 1024.0f;
        float k = (fp < 2.0f) ? (2.0f / max(fp, 1e-6f)) : 1.0f;
        t = mat_albedo.SampleGrad(studs_sampler, float3(uv, (float)layer), dx * k, dy * k);
        su = mat_surface.SampleGrad(studs_sampler, float3(uv, (float)layer), dx * k, dy * k);
        glow = mat_emission.SampleGrad(studs_sampler, float3(uv, (float)layer), dx * k, dy * k).r;
    }
    float4 rc = tp_p[pal * 4u + 2u];
    float3 y = ycocg(t.rgb);
    if (rc.w > 0.5f) {
        float4 mean = tp_p[pal * 4u + 3u];
        float3 blade_base = (mean.w > 0.5f) ? mean.rgb : (float3(194.0f, 196.0f, 188.0f) / 255.0f);
        float3 tint_base = blade ? blade_base : t.rgb;
        float3 ty = ycocg(tint_base * lerp(float3(1.0f, 1.0f, 1.0f), rc.rgb, su.a));
        s.ycocg = ty;
    } else {
        s.ycocg = float3(y.x * rc.x, rc.y * y.y - rc.z * y.z, rc.z * y.y + rc.y * y.z);
    }
    s.height = t.a;
    s.nxy = su.rg;
    s.rough = su.b;
    s.glow = glow;
    return s;
}

float4 fs_terrain(TerrainOut input) : SV_Target {
    // Hoist derivatives & texture samples before TERRAIN_CAPTURE_ID check.
    float2 d0x = ddx(input.uv01.xy);
    float2 d0y = ddy(input.uv01.xy);
    float2 d1x = ddx(input.uv01.zw);
    float2 d1y = ddy(input.uv01.zw);
    float2 d2x = ddx(input.uv2);
    float2 d2y = ddy(input.uv2);
    bool is_blade = input.palette.w > 0u;
    TerrainSample s0 = terrain_sample(input.uv01.xy, d0x, d0y, input.palette.x, is_blade);
    TerrainSample s1 = terrain_sample(input.uv01.zw, d1x, d1y, input.palette.y, is_blade);
    TerrainSample s2 = terrain_sample(input.uv2, d2x, d2y, input.palette.z, is_blade);

    if (TERRAIN_CAPTURE_ID) {
        return is_blade ? float4(0.0f, 1.0f, 0.0f, 1.0f) : float4(1.0f, 0.0f, 0.0f, 1.0f);
    }

    float3 n0 = normalize(input.normal);
    float4 b0 = tp_p[input.palette.x * 4u + 1u];
    float4 b1 = tp_p[input.palette.y * 4u + 1u];
    float4 b2 = tp_p[input.palette.z * 4u + 1u];
    float3 h = input.bary * float3(
        saturate(s0.height * b0.x + b0.y),
        saturate(s1.height * b1.x + b1.y),
        saturate(s2.height * b2.x + b2.y)
    );
    float soft = max(dot(input.bary, float3(b0.z, b1.z, b2.z)), 1e-4f);
    float3 w = saturate((h - float3(max(h.y, h.z), max(h.x, h.z), max(h.x, h.y)) + 0.5f * soft) / soft);
    w = w / max(w.x + w.y + w.z, 1e-4f);
    float far = saturate(sqrt(max(dot(d0x, d0x), dot(d0y, d0y))) * 7.0f);
    w = lerp(w, input.bary, far);

    float3 yc = s0.ycocg * w.x + s1.ycocg * w.y + s2.ycocg * w.z;
    float3 albedo = float3(yc.x - yc.z + yc.y, yc.x + yc.z, yc.x - yc.z - yc.y);
    float2 nt = (s0.nxy * w.x + s1.nxy * w.y + s2.nxy * w.z) * 2.0f - 1.0f;
    float nz = sqrt(saturate(1.0f - dot(nt, nt)));
    float3 nb = normalize((float3(nt, nz) - float3(0.0f, 0.0f, 1.0f)) * rsqrt(max(dot(w, w), 1e-4f)) + float3(0.0f, 0.0f, 1.0f));
    float edge = dot((float3)(uint3(input.axis.x, input.axis.y, input.axis.z) > 7u), w);
    float3 tan = lerp(float3(n0.z, 0.0f, -n0.x), float3(-n0.y, n0.x, 0.0f), edge);
    float3 bit = lerp(float3(0.0f, 1.0f, 0.0f), float3(0.0f, -n0.z, n0.y), edge);
    float3 n = normalize(tan * nb.x + bit * nb.y + n0 * nb.z);
    if (is_blade) {
        n = n0;
    }
    float rough = 0.089f + 0.911f * (s0.rough * w.x + s1.rough * w.y + s2.rough * w.z);
    float glow_k = dot(input.bary, float3(b0.w, b1.w, b2.w));
    float3 glow = (float3)(glow_k * (s0.glow * w.x + s1.glow * w.y + s2.glow * w.z));
    float3 base = albedo * albedo;

    float3 l = normalize(g.sun_dir.xyz);
    float ndl = max(dot(n, l), 0.0f);
    float shadow = (ndl > 0.0f) ? (sun_visibility_at(input.world, n0, input.world + g.sun_dir.xyz * asfloat(981668463u)) * sun_cover()) : 1.0f;
    float3 grid_n = is_blade ? float3(0.0f, 1.0f, 0.0f) : n0;
    float sv = light_grid(input.world, grid_n);
    float3 v = normalize(g.cam_pos.xyz - input.world);
    float nv = max(dot(n, v), 1e-4f);
    float fade = saturate(1.0f - length(g.cam_pos.xyz - input.world) * ENV_SPEC_FADE);
    float sS = (g.env_spec.z > 0.5f) ? (g.sky_grad[1].w * fade) : 0.0f;
    float2 ab = brdf_ab(rough, nv);
    float fe = (0.04f * ab.x + ab.y) / max(ab.x + ab.y, 1e-4f);
    float3 hv = normalize(v + l);
    float vh = dot(l, hv);
    float e5 = pow(abs(max(1.0f - saturate(vh), 0.0f)), 5.0f);
    float fr = e5 + 0.04f * (1.0f - e5);

    float3 light = (1.0f - fe * sS) * env_cube(n, sv) + g.ambient.rgb + g.outdoor.rgb * sv * sky_gain()
        + (1.0f - fr * sS) * g.sun_color.rgb * g.sun_dir.w * ndl * shadow + glow;
    light += g.lamp1.rgb * max(-dot(n, l), 0.0f);
    light += local_lights(input.world, (g.sun_color.w > 0.5f) ? n0 : n);

    [branch]
    if (is_blade) {
        float grass_shadow = (g.env_spec.z > 0.5f) ? sun_shadow(input.world, grid_n) : 1.0f;
        float3 grass_indoor = float3(0.0f, 0.0f, 0.0f);
        float3 grass_sky = float3(0.0f, 0.0f, 0.0f);
        [unroll]
        for (uint i = 0u; i < 6u; i++) {
            grass_indoor += g.env_indoor[i].rgb / 6.0f;
            grass_sky += g.env[i].rgb / 6.0f;
        }
        float3 grass_ambient = g.ambient.rgb + grass_indoor;
        float3 grass_outdoor = g.outdoor.rgb + grass_sky - grass_indoor;
        float3 grass_c = albedo * albedo * (grass_ambient + grass_outdoor * sv + input.grass_direct * grass_shadow);
        if (g.env_spec.z > 0.5f) {
            float transmission = saturate(input.grass_view.z * pow(abs(max(input.grass_view.x, 0.0f)), 3.0f));
            float highlight = pow(abs(saturate(input.grass_view.y)), 12.0f) * input.grass_view.w;
            grass_c += (albedo * albedo * transmission + (float3)(highlight * sun_cover())) * g.sun_color.rgb * g.sun_dir.w * grass_shadow;
        }
        [branch]
        if (g.fog.z > 0.5f) {
            grass_c = lerp(grass_c, fog_rgb_at(input.world), fog_amount(length(input.world - g.cam_pos.xyz)));
        }
        return float4(grass_c, 1.0f);
    }

    float3 c = base * light;
    float a2 = rough * rough * rough * rough;
    float nh = max(dot(n, hv), 0.001f);
    float dd = nh * nh * (a2 - 1.0f) + 1.0f;
    float spec = fr * (a2 + a2 * a2) / (dd * dd * (vh * 3.0f + 0.5f) * (nh * 0.75f + 0.25f));
    c += g.sun_color.rgb * g.sun_dir.w * spec * ndl * shadow * fade * sun_cover();
    c += env_reflect(reflect(-v, n), rough, sv, false) * fe * sS;
    [branch]
    if (g.fog.z > 0.5f) {
        float d = length(input.world - g.cam_pos.xyz);
        float f = fog_amount(d);
        c = lerp(c, fog_rgb_at(input.world), f);
    }
    if (g.sky_rot.z > 3.5f) {
        c = (float3)sv;
    }
    return float4(c, 1.0f);
}

// ------------------------------------------------------------- shadows

float4 vs_terrain_shadow(TerrainIn v) : SV_Position {
    float4 clip = mul(sunp_vp, float4(grass_position_from(v, false, sunp_centre.xyz), 1.0f));
    clip.z = max(clip.z, 0.0f);
    return clip;
}

float4 vs_shadow(PartIn v) : SV_Position {
    float4 clip = mul(sunp_vp, float4(part_world(v), 1.0f));
    clip.z = max(clip.z, 0.0f);
    if ((((uint)(v.extra.y + 0.5f) >> 19u) & 1u) == 1u) {
        clip = float4(2.0f, 2.0f, 2.0f, 1.0f);
    }
    return clip;
}

// ---------------------------------------------------------------- lines

struct LineIn {
    float3 pos   : TEXCOORD0;
    float4 color : TEXCOORD1;
};

struct LineOut {
    float4 clip  : SV_Position;
    float4 color : TEXCOORD0;
    float3 world : TEXCOORD1;
};

struct LineFrag {
    float4 color : SV_Target;
    float depth  : SV_Depth;
};

LineOut vs_line(LineIn v) {
    LineOut o;
    o.clip = mul(g.view_proj, float4(v.pos, 1.0f));
    o.color = v.color;
    o.world = v.pos;
    return o;
}

LineFrag fs_line(LineOut input) {
    LineFrag o;
    float a = input.color.a;
    o.depth = input.clip.z;
    float fade = 1.0f;
    if (a >= 3.5f) {
        a = a - 4.0f;
        o.depth = 1.0f;
    } else if (a >= 1.5f) {
        a = a - 2.0f;
    } else {
        float d = length(input.world - g.cam_pos.xyz);
        fade = 1.0f - saturate((d - 80.0f) / 260.0f);
    }
    o.color = float4(sq(input.color.rgb), a * fade);
    return o;
}

// --------------------------------------------------------- glass depth

float fs_glass_depth(float4 p : SV_Position) : SV_Target {
    int2 pixel = (int2)p.xy;
#if MSAA_SAMPLES > 1
    float z = 0.0f;
    [unroll]
    for (uint i = 0u; i < (uint)MSAA_SAMPLES; i++) {
        z = max(z, ms_load(glass_depth_ms, pixel, i));
    }
    return z;
#else
    return ms_load(glass_depth_ms, pixel, 0u);
#endif
}
