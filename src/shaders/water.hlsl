#define SCENE_GROUP0
#include "common.hlsli"

cbuffer WaterParamsCB : register(b3) {
    float4 wp_wave;   // x wave number, y phase, z amplitude, w frame blend
    float4 wp_frames; // x frame a, y frame b, z depth scale, w depth offset
    float4 wp_view;   // xy target size, z wave fade, w near plane
};

Texture2DArray<float4> water_normals : register(t21);
SamplerState water_sampler           : register(s4);
Texture2D<float4> scene_color        : register(t22);
TEX_DEPTH_MS scene_depth             : register(t23);

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

struct WaterOut {
    float4 clip     : SV_Position;
    float3 world    : TEXCOORD0;
    float3 normal   : TEXCOORD1;
    float4 color    : TEXCOORD2;
    float2 uv       : TEXCOORD3;
    float3 emissive : TEXCOORD4;
};

static const float WATER_SUN = 1.0f;

WaterOut vs_water(PartIn v) {
    float3 l = v.pos * v.size.xyz;
    float3 origin = float3(v.r0.w, v.r1.w, v.r2.w);
    float3 world = v.r0.xyz * l.x + v.r1.xyz * l.y + v.r2.xyz * l.z + origin;
    float3 s = max(v.size.xyz, float3(1e-4f, 1e-4f, 1e-4f));
    float3 n = v.r0.xyz * (v.normal.x / s.x) + v.r1.xyz * (v.normal.y / s.y) + v.r2.xyz * (v.normal.z / s.z);

    float fade = saturate(1.0f - length(world - g.cam_pos.xyz) * wp_view.z);
    float k = wp_wave.x;
    float dy = (sin((world.z - world.x) * k - wp_wave.y) + sin((world.z + world.x) * k + wp_wave.y)) * wp_wave.z * fade;
    world.y += dy;

    WaterOut o;
    o.clip = mul(g.view_proj, float4(world, 1.0f));
    o.world = world;
    o.normal = normalize(n);
    o.color = (v.vcol.a > 0.5f) ? float4(sq(v.vcol.rgb), v.color.a) : v.color;
    o.uv = v.uv * v.uvt.xy + ((v.uvt.w < 0.0f) ? float2(0.0f, 0.0f) : v.uvt.zw);
    o.emissive = v.emissive.rgb;
    return o;
}

float3 ray_dir(float2 ndc) {
    float4 a = mul(g.inv_view_proj, float4(ndc, 0.0f, 1.0f));
    float4 b = mul(g.inv_view_proj, float4(ndc, 1.0f, 1.0f));
    return normalize(b.xyz / b.w - a.xyz / a.w);
}

float3 scene_world(float2 px) {
    int2 size = ms_dims(scene_depth);
    int2 c = clamp((int2)px, int2(0, 0), size - 1);
    float d = ms_load(scene_depth, c, 0u);
    float2 uv = ((float2)c + 0.5f) / (float2)size;
    float3 dir = ray_dir(float2(uv.x * 2.0f - 1.0f, 1.0f - uv.y * 2.0f));
    float3 cam = g.cam_pos.xyz;
    if (d <= 1e-7f) {
        return cam + dir * 1e6f;
    }
    float3 fwd = ray_dir(float2(0.0f, 0.0f));
    return cam + dir * ((wp_view.w / d) / max(dot(dir, fwd), 1e-3f));
}

float3 scene_rgb(float2 px) {
    int2 size = tex_dims2d(scene_color);
    int2 c = clamp((int2)px, int2(0, 0), size - 1);
    return scene_color.Load(int3(c, 0)).rgb;
}

float3 water_reflection(float3 world, float3 ray, float3 env) {
    float4 origin = mul(g.view_proj, float4(world, 1.0f));
    float4 direction = mul(g.view_proj, float4(ray, 0.0f));
    float bound = origin.w * 0.2f;
    float travel = 1.0f;
    [unroll]
    for (uint i = 0u; i < 8u; i++) {
        float4 probe = origin + direction * travel;
        float2 uv = probe.xy / probe.w * float2(0.5f, -0.5f) + 0.5f;
        float3 hit = scene_world(uv * wp_view.xy);
        float depth = min(mul(g.view_proj, float4(hit, 1.0f)).w, 500.0f);
        travel += clamp(depth - probe.w, -bound, bound);
    }
    float4 probe = origin + direction * travel;
    float2 uv = probe.xy / probe.w * float2(0.5f, -0.5f) + 0.5f;
    float depth = min(mul(g.view_proj, float4(scene_world(uv * wp_view.xy), 1.0f)).w, 500.0f);
    bool valid = abs(uv.x - 0.5f) < 0.55f && abs(uv.y - 0.5f) < 0.5f
        && abs(depth - probe.w) < 10.0f && direction.w > 0.0f;
    float weight = valid ? saturate(3.9f - max(origin.w, probe.w) * 0.008f) : 0.0f;
    return lerp(env, scene_rgb(uv * wp_view.xy), weight);
}

float4 fs_water(WaterOut input) : SV_Target {
    // Hoist world derivatives before discard so SampleGrad runs cleanly.
    float3 wdx = ddx(input.world);
    float3 wdy = ddy(input.world);

    float3 cam = g.cam_pos.xyz;
    float2 px = input.clip.xy;
    float water_d = length(input.world - cam);
    float3 behind = scene_world(px);
    float scene_d0 = mul(g.view_proj, float4(behind, 1.0f)).w;
    float view_d = mul(g.view_proj, float4(input.world, 1.0f)).w;
    if (scene_d0 < view_d - 0.05f) {
        discard;
    }

    float3 up = normalize(input.normal);
    float3 v = normalize(cam - input.world);
    float3 a = abs(up);
    float2 uv = input.world.xz * 0.05f;
    float2 ddx_uv = wdx.xz * 0.05f;
    float2 ddy_uv = wdy.xz * 0.05f;
    float s = 1.0f;
    if (a.x > a.y && a.x >= a.z) {
        uv = input.world.zy * 0.05f;
        ddx_uv = wdx.zy * 0.05f;
        ddy_uv = wdy.zy * 0.05f;
        s = 0.0f;
    } else if (a.z > a.y && a.z > a.x) {
        uv = input.world.xy * 0.05f;
        ddx_uv = wdx.xy * 0.05f;
        ddy_uv = wdy.xy * 0.05f;
        s = 0.0f;
    }

    float packed_x = floor(input.uv.x * 0.25f + 1e-3f);
    float packed_y = floor(input.uv.y * 0.25f + 1e-3f);
    float r0 = packed_x - floor(packed_x / 256.0f) * 256.0f;
    float r1 = floor(packed_x / 256.0f);
    float r2 = packed_y;
    float b0 = saturate(input.uv.x - packed_x * 4.0f);
    float b1 = saturate(input.uv.y - packed_y * 4.0f);
    float b2 = max(1.0f - b0 - b1, 0.0f);
    float3 bw = float3(b0, b1, b2) / max(b0 + b1 + b2, 1e-4f);
    float2 o0 = float2(r0, floor(r0 * 2.6651442f)) * 0.1f;
    float2 o1 = float2(r1, floor(r1 * 2.6651442f)) * 0.1f;
    float2 o2 = float2(r2, floor(r2 * 2.6651442f)) * 0.1f;

    float fa = (float)(int)wp_frames.x;
    float fb = (float)(int)wp_frames.y;
    float4 ta = water_normals.SampleGrad(water_sampler, float3(uv + o0, fa), ddx_uv, ddy_uv) * bw.x
        + water_normals.SampleGrad(water_sampler, float3(uv + o1, fa), ddx_uv, ddy_uv) * bw.y
        + water_normals.SampleGrad(water_sampler, float3(uv + o2, fa), ddx_uv, ddy_uv) * bw.z;
    float4 tb = water_normals.SampleGrad(water_sampler, float3(uv + o0, fb), ddx_uv, ddy_uv) * bw.x
        + water_normals.SampleGrad(water_sampler, float3(uv + o1, fb), ddx_uv, ddy_uv) * bw.y
        + water_normals.SampleGrad(water_sampler, float3(uv + o2, fb), ddx_uv, ddy_uv) * bw.z;
    float4 t = lerp(ta, tb, wp_wave.w);

    float2 xy = t.wy * 2.0f - 1.0f;
    float tz = sqrt(saturate(1.0f - dot(xy, xy)));
    float3 tan_v = lerp(float3(up.z, 0.0f, -up.x), float3(-up.y, up.x, 0.0f), s);
    float3 bit_v = lerp(float3(0.0f, 1.0f, 0.0f), float3(0.0f, -up.z, up.y), s);
    float3 nm = normalize(tan_v * xy.x + bit_v * xy.y + up * tz);
    if (dot(nm, nm) < 0.5f) {
        nm = up;
    }
    float3 nr = lerp(up, nm, 0.25f);

    float shadow = sun_shadow(input.world, up);
    float sv = light_grid(input.world, up);
    float3 lit = input.color.rgb * (g.ambient.rgb + g.outdoor.rgb * sv * sky_gain()
        + env_cube(nm, sv)
        + g.sun_color.rgb * g.sun_dir.w * shadow * WATER_SUN);

    float2 size = wp_view.xy;
    float2 px1 = px + nm.xz * 0.05f * size;
    float scene_d1 = mul(g.view_proj, float4(scene_world(px1), 1.0f)).w;
    float k1 = saturate(scene_d1 - view_d);
    float3 refr = lerp(scene_rgb(px), scene_rgb(px1), k1);
    float depth = min(lerp(scene_d0, scene_d1, k1), 500.0f) - view_d;
    float cover = saturate(saturate(depth * wp_frames.z + wp_frames.w)
        + saturate(view_d * 0.004f - 1.0f));

    float3 under = lerp(refr, lit, cover);
    float3 reflected = reflect(-v, nr);
    float3 tint = lerp(g.sky_grad[1].rgb, g.sky_grad[0].rgb, saturate(reflected.y * 1.5882353f));
    float3 env = sky_color(reflected) * tint * sv;
    float fres = saturate((saturate(0.78f - 2.5f * abs(dot(nr, v))) + 0.3f) * input.emissive.x);
    float3 c = lerp(under, water_reflection(input.world, reflected, env), fres);

    float3 l = normalize(g.sun_dir.xyz);
    float3 hv = normalize(v + l);
    float r = 0.089f + 0.911f * t.x;
    float a2 = (r * r) * (r * r);
    float ndh = max(0.001f, dot(nm, hv));
    float ldh = dot(l, hv);
    float f5 = pow(abs(max(1.0f - ldh, 0.0f)), 5.0f);
    float fr = f5 + 0.02f * (1.0f - f5);
    float den = (ndh * a2 - ndh) * ndh + 1.0f;
    float dterm = (a2 + a2 * a2) / ((den * den) * (ldh * 3.0f + 0.5f) * (ndh * 0.75f + 0.25f));
    float spec_fade = saturate(1.0f - view_d * wp_view.z);
    c += fr * dterm * saturate(dot(nm, l) * shadow) * g.sun_color.rgb * g.sun_dir.w * spec_fade * sun_cover();

    if (g.fog.z > 0.5f) {
        c = lerp(c, fog_rgb_at(input.world), fog_amount(water_d));
    }
    return float4(c, 1.0f);
}
