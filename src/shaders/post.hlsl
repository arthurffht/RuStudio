#define SCENE_GROUP0
#include "common.hlsli"

Texture2D<float4> hdr_tex   : register(t15);
Texture2D<float4> bloom_tex : register(t16);
SamplerState hdr_sampler    : register(s3);

TEX_DEPTH_MS fx_depth_ms    : register(t17);
TEX_DEPTH_MS ssao_depth_ms  : register(t18);
Texture2D<float4> ssao_in   : register(t19);
Texture2D<float4> ssao_scene : register(t20);
Texture2D<float4> dof_tex   : register(t21);
Texture2D<float4> near_tex  : register(t22);

struct PostOut {
    float4 pos : SV_Position;
    float2 uv  : TEXCOORD0;
};

PostOut vs_post(uint id : SV_VertexID) {
    PostOut o;
    fullscreen_tri(id, o.pos, o.uv);
    return o;
}

// ------------------------------------------------------------------ Bloom

float3 bloom_source(float3 hdr) {
    float3 c = clamp(hdr, float3(0.0f, 0.0f, 0.0f), float3(4.0f, 4.0f, 4.0f));
    float m = max(c.r, max(c.g, c.b));
    if (m <= g.bloom.x) {
        return float3(0.0f, 0.0f, 0.0f);
    }
    return (c / (m + 0.001f)) * (m - g.bloom.x);
}

float3 bloom_scene_sample(float2 uv) {
    int2 dim = tex_dims2d(hdr_tex);
    float2 p = uv * (float2)dim - 0.5f;
    int2 base = (int2)floor(p);
    float2 f = frac(p);
    float3 t0 = sqrt(saturate(hdr_tex.Load(int3(clamp(base + int2(0, 0), int2(0, 0), dim - 1), 0)).rgb * g.params.y * 0.25f));
    float3 t1 = sqrt(saturate(hdr_tex.Load(int3(clamp(base + int2(1, 0), int2(0, 0), dim - 1), 0)).rgb * g.params.y * 0.25f));
    float3 t2 = sqrt(saturate(hdr_tex.Load(int3(clamp(base + int2(0, 1), int2(0, 0), dim - 1), 0)).rgb * g.params.y * 0.25f));
    float3 t3 = sqrt(saturate(hdr_tex.Load(int3(clamp(base + int2(1, 1), int2(0, 0), dim - 1), 0)).rgb * g.params.y * 0.25f));
    float3 encoded = lerp(lerp(t0, t1, f.x), lerp(t2, t3, f.x), f.y);
    return 4.0f * encoded * encoded;
}

float4 fs_bloom_bright(PostOut input) : SV_Target {
    float2 texel = 1.0f / (float2)tex_dims2d(hdr_tex);
    static const float2 offsets[9] = {
        float2(0.0f, 0.0f), float2(0.75f, 0.0f), float2(-0.75f, 0.0f),
        float2(0.0f, 0.75f), float2(0.0f, -0.75f),
        float2(0.70512277f, 0.70512277f), float2(-0.70512277f, -0.70512277f),
        float2(0.70512277f, -0.70512277f), float2(-0.70512277f, 0.70512277f)
    };
    float3 scene = float3(0.0f, 0.0f, 0.0f);
    [unroll]
    for (int k = 0; k < 9; k++) {
        float2 uv = clamp(input.uv + offsets[k] * texel * 2.0f, texel * 0.5f, 1.0f - texel * 0.5f);
        scene += bloom_scene_sample(uv);
    }
    return float4(bloom_source(scene / 9.0f), 1.0f);
}

float4 fs_bloom_down(PostOut input) : SV_Target {
    float2 t = 1.0f / (float2)tex_dims2d(hdr_tex);
    float3 c = hdr_tex.SampleLevel(hdr_sampler, input.uv, 0.0f).rgb;
    if (TERRAIN_CAPTURE_ID) {
        return float4(c, 1.0f);
    }
    static const float2 offsets[8] = {
        float2(0.75f, 0.0f), float2(-0.75f, 0.0f), float2(0.0f, 0.75f), float2(0.0f, -0.75f),
        float2(0.70512277f, 0.70512277f), float2(-0.70512277f, -0.70512277f),
        float2(0.70512277f, -0.70512277f), float2(-0.70512277f, 0.70512277f)
    };
    [unroll]
    for (int i = 0; i < 8; i++) {
        c += hdr_tex.SampleLevel(hdr_sampler, clamp(input.uv + offsets[i] * t * 2.0f, t * 0.5f, 1.0f - t * 0.5f), 0.0f).rgb;
    }
    return float4(c / 9.0f, 1.0f);
}

float4 fs_bloom_up(PostOut input) : SV_Target {
    float2 full = (float2)tex_dims2d(bloom_tex);
    float2 dim = (float2)tex_dims2d(hdr_tex);
    float lod = min(5.999f, 3.0f + max(-1.0f, log2(sqrt(full.x * full.y / 2073600.0f)) + max(0.0f, log2(g.bloom.y / 7.0f))));
    float level = round(log2(full.x / dim.x));
    float weight = (level >= floor(lod) - 0.1f) ? frac(lod) : 1.0f;
    float2 t = 1.0f / dim;
    float3 c = float3(0.0f, 0.0f, 0.0f);
    [unroll]
    for (int y = -1; y <= 1; y++) {
        [unroll]
        for (int x = -1; x <= 1; x++) {
            float w = ((x == 0) ? 2.0f : 1.0f) * ((y == 0) ? 2.0f : 1.0f);
            float2 uv = clamp(input.uv + float2((float)x, (float)y) * t, 0.5f / dim, 1.0f - 0.5f / dim);
            c += hdr_tex.SampleLevel(hdr_sampler, uv, 0.0f).rgb * w;
        }
    }
    return float4(c * (weight / 16.0f), 1.0f);
}

// ------------------------------------------------------------------ DoF

static const float2 DOF_DISK[45] = {
    float2(0.22490672767162323f, 0.16340430080890656f),
    float2(-0.08590672165155411f, 0.26439371705055237f),
    float2(-0.27799999713897705f, 3.40440574038857e-17f),
    float2(-0.08590672165155411f, -0.26439371705055237f),
    float2(0.22490672767162323f, -0.16340430080890656f),
    float2(0.5400000214576721f, 0.0f),
    float2(0.43686920404434204f, 0.31740406155586243f),
    float2(0.16686917841434479f, 0.5135705471038818f),
    float2(-0.16686917841434479f, 0.5135705471038818f),
    float2(-0.43686920404434204f, 0.31740406155586243f),
    float2(-0.5400000214576721f, 6.612874892975988e-17f),
    float2(-0.43686920404434204f, -0.31740406155586243f),
    float2(-0.16686917841434479f, -0.5135705471038818f),
    float2(0.16686917841434479f, -0.5135705471038818f),
    float2(0.43686920404434204f, -0.31740406155586243f),
    float2(0.7651021480560303f, 0.18858073651790619f),
    float2(0.5898264646530151f, 0.5225406289100647f),
    float2(0.2794286608695984f, 0.736792802810669f),
    float2(-0.09498289972543716f, 0.7822545766830444f),
    float2(-0.4476350247859955f, 0.6485112905502319f),
    float2(-0.6977393627166748f, 0.36620184779167175f),
    float2(-0.7879999876022339f, 4.464412615259102e-16f),
    float2(-0.6977393627166748f, -0.36620184779167175f),
    float2(-0.4476350247859955f, -0.6485112905502319f),
    float2(-0.09498289972543716f, -0.7822545766830444f),
    float2(0.2794286608695984f, -0.736792802810669f),
    float2(0.5898264646530151f, -0.5225406289100647f),
    float2(0.7651021480560303f, -0.18858073651790619f),
    float2(1.0f, 0.0f),
    float2(0.9324722290039062f, 0.3612416684627533f),
    float2(0.739008903503418f, 0.6736956238746643f),
    float2(0.4457383453845978f, 0.8951632976531982f),
    float2(0.09226836264133453f, 0.9957341551780701f),
    float2(-0.2736629843711853f, 0.9618256688117981f),
    float2(-0.602634608745575f, 0.7980172038078308f),
    float2(-0.8502171635627747f, 0.5264321565628052f),
    float2(-0.9829730987548828f, 0.1837495118379593f),
    float2(-0.9829730987548828f, -0.1837495118379593f),
    float2(-0.8502171635627747f, -0.5264321565628052f),
    float2(-0.602634608745575f, -0.7980172038078308f),
    float2(-0.2736629843711853f, -0.9618256688117981f),
    float2(0.09226836264133453f, -0.9957341551780701f),
    float2(0.4457383453845978f, -0.8951632976531982f),
    float2(0.739008903503418f, -0.6736956238746643f),
    float2(0.9324722290039062f, -0.3612416684627533f)
};

static const float2 DOF_CHEAP_DISK[27] = {
    float2(0.5400000215f, 0.0f),
    float2(0.436869204f, 0.3174040616f),
    float2(0.1668691784f, 0.5135705471f),
    float2(-0.1668691784f, 0.5135705471f),
    float2(-0.436869204f, 0.3174040616f),
    float2(-0.5400000215f, 6.612874893e-17f),
    float2(-0.436869204f, -0.3174040616f),
    float2(-0.1668691784f, -0.5135705471f),
    float2(0.1668691784f, -0.5135705471f),
    float2(0.436869204f, -0.3174040616f),
    float2(1.0f, 0.0f),
    float2(0.932472229f, 0.3612416685f),
    float2(0.7390089035f, 0.6736956239f),
    float2(0.4457383454f, 0.8951632977f),
    float2(0.09226836264f, 0.9957341552f),
    float2(-0.2736629844f, 0.9618256688f),
    float2(-0.6026346087f, 0.7980172038f),
    float2(-0.8502171636f, 0.5264321566f),
    float2(-0.9829730988f, 0.1837495118f),
    float2(-0.9829730988f, -0.1837495118f),
    float2(-0.8502171636f, -0.5264321566f),
    float2(-0.6026346087f, -0.7980172038f),
    float2(-0.2736629844f, -0.9618256688f),
    float2(0.09226836264f, -0.9957341552f),
    float2(0.4457383454f, -0.8951632977f),
    float2(0.7390089035f, -0.6736956239f),
    float2(0.932472229f, -0.3612416685f)
};

float fx_depth(float2 uv) {
    int2 size = ms_dims(fx_depth_ms);
    int2 p = clamp((int2)(uv * (float2)size), int2(0, 0), size - 1);
    return ms_load(fx_depth_ms, p, 0u);
}

float fx_distance(float2 uv) {
    float z = g.ssao.z / max(fx_depth(uv), 1e-8f);
    float2 ray = float2((uv.x * 2.0f - 1.0f) / (2.0f * g.ssao.x), (1.0f - uv.y * 2.0f) / (2.0f * g.ssao.y));
    return min(z * sqrt(1.0f + dot(ray, ray)), 500.0f);
}

float dof_coc(float2 uv) {
    float d = fx_distance(uv);
    float far_c = saturate((d - g.dof.x - g.dof.y - 0.5f) / d);
    float near_c = saturate((g.dof.x - g.dof.y + 0.5f - d) / d);
    float far_v = (g.dof.w > 0.0f) ? far_c : 0.0f;
    return (near_c > 0.0f && g.dof.z > 0.0f) ? (-near_c) : far_v;
}

float4 fs_dof_h(PostOut input) : SV_Target {
    float3 c = hdr_tex.SampleLevel(hdr_sampler, input.uv, 0.0f).rgb;
    return float4(c, dof_coc(input.uv));
}

float2 near_reduction_params() {
    float2 full = (float2)ms_dims(fx_depth_ms);
    float2 quarter = max(floor((full + fmod(full, 2.0f)) / 4.0f), float2(1.0f, 1.0f));
    return float2(2.0f * (full.x - quarter.x), full.x - quarter.y) / full.x;
}

float3 near_color(float2 uv) {
    int2 size = tex_dims2d(hdr_tex);
    float2 p = uv * (float2)size - 0.5f;
    int2 b = (int2)floor(p);
    float2 f = frac(p);
    int2 hi = size - 1;
    float3 a = sqrt(max(hdr_tex.Load(int3(clamp(b, int2(0, 0), hi), 0)).rgb, float3(0.0f, 0.0f, 0.0f)));
    float3 c = sqrt(max(hdr_tex.Load(int3(clamp(b + int2(1, 0), int2(0, 0), hi), 0)).rgb, float3(0.0f, 0.0f, 0.0f)));
    float3 d = sqrt(max(hdr_tex.Load(int3(clamp(b + int2(0, 1), int2(0, 0), hi), 0)).rgb, float3(0.0f, 0.0f, 0.0f)));
    float3 e = sqrt(max(hdr_tex.Load(int3(clamp(b + int2(1, 1), int2(0, 0), hi), 0)).rgb, float3(0.0f, 0.0f, 0.0f)));
    return lerp(lerp(a, c, f.x), lerp(d, e, f.x), f.y);
}

float4 fs_near_h(PostOut input) : SV_Target {
    float2 full = (float2)ms_dims(fx_depth_ms);
    float quarter_width = max(floor((full.x + fmod(full.x, 2.0f)) / 4.0f), 1.0f);
    float dx = near_reduction_params().x / quarter_width;
    float2 uv_max = 1.0f - 0.5f / float2(quarter_width, full.y);
    float3 color = float3(0.0f, 0.0f, 0.0f);
    float coc = 0.0f;
    [unroll]
    for (int i = 0; i < 4; i++) {
        float2 uv = min(input.uv + float2(((float)i - 1.5f) * dx, 0.0f), uv_max);
        float3 c = near_color(uv);
        float d = fx_distance(uv);
        color += c * c * 0.25f;
        coc = max(coc, saturate((g.dof.x - g.dof.y + 0.5f - d) / d));
    }
    return float4(color, coc);
}

float4 fs_near_v(PostOut input) : SV_Target {
    float2 full = (float2)ms_dims(fx_depth_ms);
    float2 quarter = max(floor((full + fmod(full, 2.0f)) / 4.0f), float2(1.0f, 1.0f));
    float dy = near_reduction_params().y / quarter.y;
    float2 uv_max = 1.0f - 0.5f / float2(quarter.x, full.y);
    float4 a = hdr_tex.SampleLevel(hdr_sampler, min(input.uv + float2(0.0f, 0.5f * dy), uv_max), 0.0f);
    float4 b = hdr_tex.SampleLevel(hdr_sampler, min(input.uv - float2(0.0f, 0.5f * dy), uv_max), 0.0f);
    return float4((a.rgb + b.rgb) * 0.5f, max(a.a, b.a));
}

float4 fs_near_mask0(PostOut input) : SV_Target {
    float2 s = float2(1.0f, 0.0f) / (float2)tex_dims2d(hdr_tex) * near_reduction_params().x;
    float a = hdr_tex.SampleLevel(hdr_sampler, input.uv, 0.0f).a;
    float b = hdr_tex.SampleLevel(hdr_sampler, input.uv + s, 0.0f).a;
    float c = hdr_tex.SampleLevel(hdr_sampler, input.uv + 2.0f * s, 0.0f).a;
    float d = hdr_tex.SampleLevel(hdr_sampler, input.uv - s, 0.0f).a;
    float e = hdr_tex.SampleLevel(hdr_sampler, input.uv - 2.0f * s, 0.0f).a;
    return (float4)max(a, max(max(b, c), max(d, e)));
}

float4 fs_near_mask1(PostOut input) : SV_Target {
    float2 s = float2(0.0f, 1.0f) / (float2)tex_dims2d(hdr_tex) * near_reduction_params().y;
    float a = hdr_tex.SampleLevel(hdr_sampler, input.uv, 0.0f).r;
    float b = hdr_tex.SampleLevel(hdr_sampler, input.uv + s, 0.0f).r;
    float c = hdr_tex.SampleLevel(hdr_sampler, input.uv + 2.0f * s, 0.0f).r;
    float d = hdr_tex.SampleLevel(hdr_sampler, input.uv - s, 0.0f).r;
    float e = hdr_tex.SampleLevel(hdr_sampler, input.uv - 2.0f * s, 0.0f).r;
    return (float4)max(a, max(max(b, c), max(d, e)));
}

float4 fs_near_mask2(PostOut input) : SV_Target {
    float2 s = float2(1.0f, 0.0f) / (float2)tex_dims2d(hdr_tex) * g.dof.z;
    float a = hdr_tex.SampleLevel(hdr_sampler, input.uv, 0.0f).r;
    float b = hdr_tex.SampleLevel(hdr_sampler, input.uv + s, 0.0f).r;
    float c = hdr_tex.SampleLevel(hdr_sampler, input.uv + 2.0f * s, 0.0f).r;
    float d = hdr_tex.SampleLevel(hdr_sampler, input.uv - s, 0.0f).r;
    float e = hdr_tex.SampleLevel(hdr_sampler, input.uv - 2.0f * s, 0.0f).r;
    return (float4)max(a, max(max(b, c), max(d, e)));
}

float4 fs_near_mask3(PostOut input) : SV_Target {
    float2 s = float2(0.0f, 1.0f) / (float2)tex_dims2d(hdr_tex) * g.dof.z;
    float a = hdr_tex.SampleLevel(hdr_sampler, input.uv, 0.0f).r;
    float b = hdr_tex.SampleLevel(hdr_sampler, input.uv + s, 0.0f).r;
    float c = hdr_tex.SampleLevel(hdr_sampler, input.uv + 2.0f * s, 0.0f).r;
    float d = hdr_tex.SampleLevel(hdr_sampler, input.uv - s, 0.0f).r;
    float e = hdr_tex.SampleLevel(hdr_sampler, input.uv - 2.0f * s, 0.0f).r;
    return (float4)max(a, max(max(b, c), max(d, e)));
}

float4 fs_near_mask4(PostOut input) : SV_Target {
    float2 s = float2(1.0f, 0.0f) / (float2)tex_dims2d(hdr_tex) * g.dof.z;
    float a = hdr_tex.SampleLevel(hdr_sampler, input.uv, 0.0f).r;
    float b = hdr_tex.SampleLevel(hdr_sampler, input.uv + s, 0.0f).r;
    float c = hdr_tex.SampleLevel(hdr_sampler, input.uv + 2.0f * s, 0.0f).r;
    float d = hdr_tex.SampleLevel(hdr_sampler, input.uv - s, 0.0f).r;
    float e = hdr_tex.SampleLevel(hdr_sampler, input.uv - 2.0f * s, 0.0f).r;
    return (float4)min(a, min(max(b, c), min(d, e)));
}

float4 fs_near_mask5(PostOut input) : SV_Target {
    float2 s = float2(0.0f, 1.0f) / (float2)tex_dims2d(hdr_tex) * g.dof.z;
    float a = hdr_tex.SampleLevel(hdr_sampler, input.uv, 0.0f).r;
    float b = hdr_tex.SampleLevel(hdr_sampler, input.uv + s, 0.0f).r;
    float c = hdr_tex.SampleLevel(hdr_sampler, input.uv + 2.0f * s, 0.0f).r;
    float d = hdr_tex.SampleLevel(hdr_sampler, input.uv - s, 0.0f).r;
    float e = hdr_tex.SampleLevel(hdr_sampler, input.uv - 2.0f * s, 0.0f).r;
    return (float4)min(a, min(max(b, c), min(d, e)));
}

float4 fs_near_edge(PostOut input) : SV_Target {
    float2 s = 1.0f / (float2)tex_dims2d(hdr_tex);
    float4 center = hdr_tex.SampleLevel(hdr_sampler, input.uv, 0.0f);
    float threshold = bloom_tex.SampleLevel(hdr_sampler, input.uv, 0.0f).r * 1.05f;
    static const float2 offsets[4] = {
        float2(-1.0f, -1.0f), float2(1.0f, -1.0f), float2(-1.0f, 1.0f), float2(1.0f, 1.0f)
    };
    float3 sum = center.rgb;
    float count = 1.0f;
    [unroll]
    for (int i = 0; i < 4; i++) {
        float2 uv = input.uv + offsets[i] * s;
        float3 color = hdr_tex.SampleLevel(hdr_sampler, uv, 0.0f).rgb;
        float mask = bloom_tex.SampleLevel(hdr_sampler, uv, 0.0f).r;
        if (threshold >= ((i == 1) ? color.r : mask)) {
            sum += color;
            count += 1.0f;
        }
    }
    return float4(sum / count, center.a);
}

float4 fs_near_bokeh(PostOut input) : SV_Target {
    float4 center = hdr_tex.SampleLevel(hdr_sampler, input.uv, 0.0f);
    float2 size = (float2)tex_dims2d(hdr_tex);
    float mask = bloom_tex.SampleLevel(hdr_sampler, input.uv, 0.0f).r;
    float radius = mask * mask * g.dof.z;
    float2 scale = float2(size.y / size.x, 1.0f) * 0.014f;
    float4 sum = center;
    float weight = mask * mask;
    [unroll]
    for (int i = 0; i < 45; i++) {
        float2 uv = input.uv + DOF_DISK[i] * radius * scale;
        float4 t = hdr_tex.SampleLevel(hdr_sampler, uv, 0.0f);
        if (all(uv >= 0.0f) && all(uv <= 1.0f)) {
            sum += t * t.a;
            weight += t.a;
        }
    }
    return sum * saturate(1.0f / (weight + 0.001f));
}

float4 fs_near_tent(PostOut input) : SV_Target {
    float2 s = 0.5f / (float2)tex_dims2d(hdr_tex);
    return (hdr_tex.SampleLevel(hdr_sampler, input.uv + s, 0.0f)
        + hdr_tex.SampleLevel(hdr_sampler, input.uv - s, 0.0f)
        + hdr_tex.SampleLevel(hdr_sampler, input.uv + s * float2(1.0f, -1.0f), 0.0f)
        + hdr_tex.SampleLevel(hdr_sampler, input.uv + s * float2(-1.0f, 1.0f), 0.0f)) * 0.25f;
}

static const float DOF_CHEAP_BELOW = 0.22f;

float dof_radius(float coc, float intensity) {
    return coc * coc * intensity * ((intensity < DOF_CHEAP_BELOW) ? 1.4f : 1.0f);
}

float dof_weight(float coc, float intensity) {
    float a = (intensity < DOF_CHEAP_BELOW) ? coc : min(coc * 1.5f, 1.0f);
    return (intensity > 0.0f && coc > 0.0f) ? (a * a) : 0.0f;
}

float4 fs_dof_v(PostOut input) : SV_Target {
    float4 center = hdr_tex.SampleLevel(hdr_sampler, input.uv, 0.0f);
    bool near = center.a < 0.0f && g.dof.z > 0.0f && g.dof.z < DOF_CHEAP_BELOW;
    float side = near ? -1.0f : 1.0f;
    float c = max(center.a * side, 0.0f);
    float coc2 = c * c;
    float2 size = (float2)tex_dims2d(hdr_tex);
    float2 scale = float2(size.y / size.x, 1.0f) * 0.014f;
    float intensity = near ? g.dof.z : g.dof.w;
    float radius = dof_radius(c, intensity);
    float4 sum = center * coc2;
    float weight = coc2;
    [unroll]
    for (int i = 0; i < 27; i++) {
        float2 offset = DOF_CHEAP_DISK[i] * radius;
        float2 uv = input.uv + offset * scale;
        float4 tap = hdr_tex.SampleLevel(hdr_sampler, saturate(uv), 0.0f);
        float coc = max(tap.a * side, 0.0f);
        if (all(uv >= 0.0f) && all(uv <= 1.0f)) {
            sum += tap * coc;
            weight += coc;
        }
    }
    float4 blurred = sum * saturate(1.0f / (weight + 0.001f));
    if (!near && intensity >= DOF_CHEAP_BELOW) {
        return float4((c > 0.0f) ? blurred.rgb : center.rgb, center.a);
    }
    return float4(lerp(center.rgb, blurred.rgb, dof_weight(c, intensity)), center.a);
}

float4 post_near(float2 uv) {
    int2 size = tex_dims2d(near_tex);
    float2 p = uv * (float2)size - 0.5f;
    int2 b = (int2)floor(p);
    float2 f = frac(p);
    int2 hi = size - 1;
    return lerp(lerp(near_tex.Load(int3(clamp(b, int2(0, 0), hi), 0)),
                     near_tex.Load(int3(clamp(b + int2(1, 0), int2(0, 0), hi), 0)), f.x),
                lerp(near_tex.Load(int3(clamp(b + int2(0, 1), int2(0, 0), hi), 0)),
                     near_tex.Load(int3(clamp(b + int2(1, 1), int2(0, 0), hi), 0)), f.x), f.y);
}

float4 fs_post(PostOut input) : SV_Target {
    float3 c = hdr_tex.SampleLevel(hdr_sampler, input.uv, 0.0f).rgb;
    if (TERRAIN_CAPTURE_ID) {
        return float4(c, 1.0f);
    }
    if (g.dof.z > 0.0f || g.dof.w > 0.0f) {
        float dist = fx_distance(input.uv);
        float far_coc = saturate((dist - (g.dof.x + g.dof.y + 0.5f)) / dist);
        float near_coc = saturate(((g.dof.x - g.dof.y + 0.5f) - dist) / dist);
        float3 scene = c;
        if (g.dof.w > 0.0f || g.dof.z < DOF_CHEAP_BELOW) {
            c = max(dof_tex.SampleLevel(hdr_sampler, input.uv, 0.0f).rgb, float3(0.0f, 0.0f, 0.0f));
        }
        if (g.dof.w >= DOF_CHEAP_BELOW && far_coc > 0.0f) {
            float2 step_uv = 4.0f / (float2)tex_dims2d(dof_tex);
            float3 tent = float3(0.0f, 0.0f, 0.0f);
            [unroll]
            for (int y = -1; y <= 1; y++) {
                [unroll]
                for (int x = -1; x <= 1; x++) {
                    float w = ((x == 0) ? 2.0f : 1.0f) * ((y == 0) ? 2.0f : 1.0f);
                    float2 uv = saturate(input.uv + float2((float)x, (float)y) * step_uv);
                    tent += max(dof_tex.SampleLevel(hdr_sampler, uv, 0.0f).rgb, float3(0.0f, 0.0f, 0.0f)) * w * 0.0625f;
                }
            }
            c = lerp(scene, tent, dof_weight(far_coc, g.dof.w));
        }
        if (g.dof.z >= DOF_CHEAP_BELOW) {
            float2 step_uv = 4.0f / (float2)tex_dims2d(hdr_tex);
            float4 blurred = float4(0.0f, 0.0f, 0.0f, 0.0f);
            [unroll]
            for (int y = -1; y <= 1; y++) {
                [unroll]
                for (int x = -1; x <= 1; x++) {
                    float w = ((x == 0) ? 2.0f : 1.0f) * ((y == 0) ? 2.0f : 1.0f);
                    blurred += post_near(input.uv + float2((float)x, (float)y) * step_uv) * w * 0.0625f;
                }
            }
            float amount = min(max(blurred.a, near_coc) * 1.5f, 1.0f);
            c = lerp(c, blurred.rgb, amount * amount);
        }
    }

    c = min(c, (float3)(4.0f / max(g.params.y, 1e-4f)));
    c += bloom_tex.SampleLevel(hdr_sampler, input.uv, 0.0f).rgb * g.cc_tint.w / max(g.params.y, 1e-3f);
    c = tonemap(c * g.params.y);

    if (g.cc.w > 0.5f) {
        c = c + g.cc.x;
        c = (c - 0.5f) * (1.0f + g.cc.y) + 0.5f;
        float luma = dot(c, float3(0.299f, 0.587f, 0.114f));
        c = lerp((float3)luma, c, 1.0f + g.cc.z);
        c = c * g.cc_tint.rgb;
    }
    return float4(saturate(c), 1.0f);
}

// ------------------------------------------------------------------ SSAO

float ssao_lin(float d) {
    if (d <= 0.0f) {
        return 1.0f;
    }
    return min(g.ssao.z / d * SSAO_DEPTH_SCALE, SSAO_DEPTH_MAX);
}

float4 ssao_load(float2 uv) {
    int2 dims = tex_dims2d(ssao_in);
    int2 p = clamp((int2)(uv * (float2)dims), int2(0, 0), dims - 1);
    return ssao_in.Load(int3(p, 0));
}

float4 fs_ssao_down(PostOut input) : SV_Target {
    int2 dims = ms_dims(ssao_depth_ms) - 1;
    int2 p = (int2)input.pos.xy * 2;
    float m = 1.0f;
    [unroll]
    for (int j = 0; j < 2; j++) {
        [unroll]
        for (int i = 0; i < 2; i++) {
            m = min(m, ssao_lin(ms_load(ssao_depth_ms, min(p + int2(i, j), dims), 0u)));
        }
    }
    return float4(m, 0.0f, 0.0f, 1.0f);
}

float3 ssao_point(float2 uv) {
    float d = ssao_load(uv).x / SSAO_DEPTH_SCALE;
    return float3((uv.x - 0.5f) * d / g.ssao.x, (0.5f - uv.y) * d / g.ssao.y, -d);
}

float4 fs_ssao(PostOut input) : SV_Target {
    float2 dims = (float2)tex_dims2d(ssao_in);
    float2 texel = 1.0f / dims;
    float d = ssao_load(input.uv).x;
    if (d > 0.99f || g.ssao.w < 0.5f) {
        return float4(1.0f, d, 0.0f, 1.0f);
    }
    float3 p = ssao_point(input.uv);
    float3 right_d = ssao_point(input.uv + float2(texel.x, 0.0f)) - p;
    float3 left_d = p - ssao_point(input.uv - float2(texel.x, 0.0f));
    float3 down_d = ssao_point(input.uv + float2(0.0f, texel.y)) - p;
    float3 up_d = p - ssao_point(input.uv - float2(0.0f, texel.y));
    float3 dx = (abs(left_d.z) < abs(right_d.z)) ? left_d : right_d;
    float3 dy = (abs(up_d.z) < abs(down_d.z)) ? up_d : down_d;
    float3 normal = normalize(cross(dy, dx));
    uint2 pixel = (uint2)input.pos.xy;
    float jitter = frac((float)((pixel.y - pixel.x) & 3u) * 0.25f)
        + frac(frac(dot(input.uv * 0.5f, float2(0.06711056f, 0.00583715f))) * 52.9829189f) * 0.5f;
    float radial = max(4.0f / max(dot(p, p), 1e-8f), 21.0f) * 0.125f;
    float occlusion = 0.0f;
    [unroll]
    for (uint direction = 0u; direction < 4u; direction++) {
        float angle = (jitter - 0.25f + (float)direction) * 1.57079632679f;
        float2 axis = float2(cos(angle), sin(angle)) * texel;
        [unroll]
        for (uint step_i = 0u; step_i < 7u; step_i++) {
            float radius = max((jitter - 0.25f + (float)step_i) * radial, (float)min(step_i + 1u, 2u));
            float2 uv = clamp(input.uv + axis * radius, texel * 0.5f, 1.0f - texel * 0.5f);
            float3 delta = ssao_point(uv) - p;
            float len2 = dot(delta, delta);
            float cosine = dot(normal, delta) * rsqrt(max(len2, 1e-8f));
            occlusion += max(1.0f - len2 * 0.0625f, 0.0f) * ((cosine > 0.18f) ? saturate(cosine) : 0.0f);
        }
    }
    return float4(1.0f - min(occlusion * 0.04357143119f, 1.0f), d, 0.0f, 1.0f);
}

float4 ssao_blur(float2 uv, float2 dir) {
    float2 texel = 1.0f / (float2)tex_dims2d(ssao_in);
    float4 c = ssao_load(uv);
    float th = clamp(c.y * SSAO_BLUR_DEPTH, SSAO_BLUR_DEPTH_MIN, SSAO_BLUR_DEPTH_MAX);
    float acc = 0.0f;
    float ws = 0.0f;
    [unroll]
    for (int i = -3; i <= 3; i++) {
        float fi = (float)i;
        float4 s = ssao_load(uv + dir * texel * fi);
        float w = exp(-fi * fi * SSAO_BLUR_K) * ((abs(s.y - c.y) < th) ? 1.0f : 0.0f);
        acc += s.x * w;
        ws += w;
    }
    return float4(acc / max(ws, 1e-6f), c.y, 0.0f, 1.0f);
}

float4 fs_ssao_blur_x(PostOut input) : SV_Target {
    return ssao_blur(input.uv, float2(1.0f, 0.0f));
}

float4 fs_ssao_blur_y(PostOut input) : SV_Target {
    return ssao_blur(input.uv, float2(0.0f, 1.0f));
}

float3 ssao_encoded_pixel(int2 pixel) {
    int2 dims = tex_dims2d(ssao_scene);
    float3 c = ssao_scene.Load(int3(clamp(pixel, int2(0, 0), dims - 1), 0)).rgb;
    return round(sqrt(saturate(c * g.params.y * 0.25f)) * 255.0f) / 255.0f;
}

float3 ssao_encoded_half(int2 pixel) {
    int2 sdims = tex_dims2d(ssao_scene);
    float2 full = (float2)sdims;
    float2 half_f = (float2)((uint2)(sdims + 1) / 2u);
    float2 pos = ((float2)clamp(pixel, int2(0, 0), (int2)half_f - 1) + 0.5f) * full / half_f - 0.5f;
    int2 p = (int2)floor(pos);
    float2 t = frac(pos);
    float3 c = lerp(lerp(ssao_encoded_pixel(p), ssao_encoded_pixel(p + int2(1, 0)), t.x),
                    lerp(ssao_encoded_pixel(p + int2(0, 1)), ssao_encoded_pixel(p + int2(1, 1)), t.x), t.y);
    return round(c * 255.0f) / 255.0f;
}

float3 ssao_encoded_scene(float2 uv) {
    int2 sdims = tex_dims2d(ssao_scene);
    float2 half_f = (float2)((uint2)(sdims + 1) / 2u);
    float2 p = uv * half_f - 0.5f;
    int2 q = (int2)floor(p);
    float2 t = frac(p);
    return lerp(lerp(ssao_encoded_half(q), ssao_encoded_half(q + int2(1, 0)), t.x),
                lerp(ssao_encoded_half(q + int2(0, 1)), ssao_encoded_half(q + int2(1, 1)), t.x), t.y);
}

float4 fs_ssao_apply(PostOut input) : SV_Target {
    if (g.ssao.w < 0.5f) {
        return float4(1.0f, 1.0f, 1.0f, 1.0f);
    }
    int2 full_i = ms_dims(ssao_depth_ms);
    float2 full = (float2)full_i;
    int2 dims = full_i - 1;
    int2 p = clamp((int2)input.pos.xy, int2(0, 0), dims);
    float raw = ms_load(ssao_depth_ms, p, 0u);
    float d = ssao_lin(raw);
    float2 o = 2.0f / full;
    float4 c = ssao_load(input.uv);
    float4 n0 = ssao_load(input.uv + float2(o.x, 0.0f));
    float4 n1 = ssao_load(input.uv - float2(o.x, 0.0f));
    float4 n2 = ssao_load(input.uv + float2(0.0f, o.y));
    float4 n3 = ssao_load(input.uv - float2(0.0f, o.y));
    float4 nb[4] = { n0, n1, n2, n3 };
    float m = 0.0f;
    float s = 0.0f;
    [unroll]
    for (int i = 0; i < 4; i++) {
        float k = (abs(nb[i].y - d) < SSAO_UPSAMPLE_DEPTH) ? 1.0f : 0.0f;
        m += k;
        s += nb[i].x * k;
    }
    float ao = c.x;
    int2 in_dims = tex_dims2d(ssao_in);
    bool same_res = all(in_dims == full_i);
    if (!same_res && abs(c.y - d) * m > 0.00004f) {
        ao = s / m;
    }
    if (same_res) {
        int2 hi = in_dims - 1;
        float gathered = (c.x + ssao_in.Load(int3(min(p + int2(1, 0), hi), 0)).x
            + ssao_in.Load(int3(min(p + int2(0, 1), hi), 0)).x
            + ssao_in.Load(int3(min(p + int2(1, 1), hi), 0)).x) * 0.25f;
        ao = c.x + (gathered - c.x) * 0.25f;
    }
    float y = 0.0f;
    if (g.fog.z > 0.5f && raw > 0.0f) {
        float2 uvc = input.uv - 0.5f;
        float dist = sqrt(dot(uvc, uvc) * 0.5f + 1.0f) * (g.ssao.z / raw);
        y = fog_amount(dist);
    }
    float a = (1.0f - y) * max(ao, 0.5f) + y;
    float3 encoded = ssao_encoded_scene(input.uv);
    float visibility = max(a, 0.5f);
    float3 q = max((float3)visibility, visibility * (
        visibility * (visibility * (encoded * 2.0404f - 0.3324f)
            + (encoded * -4.7951f + 0.6417f)) + (encoded * 2.7552f + 0.6903f)));
    return float4(q * q, 1.0f);
}
