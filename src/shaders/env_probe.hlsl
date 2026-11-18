#include "common.hlsli"

cbuffer ProbeParamsCB : register(b0) {
    float4 pp_p;
};

cbuffer ProbeCoeffCB : register(b1) {
    float4 coeff[60];
};

TextureCube<float4> env   : register(t0);
Texture2D<float4> face_src : register(t1);
SamplerState env_sampler  : register(s0);

struct ProbeOut {
    float4 pos : SV_Position;
    float2 uv  : TEXCOORD0;
};

ProbeOut vs_probe(uint id : SV_VertexID) {
    ProbeOut o;
    fullscreen_tri(id, o.pos, o.uv);
    return o;
}

float3 face_dir(int face, float x, float y) {
    switch (face) {
        case 0: return float3(1.0f, y, -x);
        case 1: return float3(-1.0f, y, x);
        case 2: return float3(x, 1.0f, -y);
        case 3: return float3(x, -1.0f, y);
        case 4: return float3(x, y, 1.0f);
        default: return float3(-x, y, -1.0f);
    }
}

float4 fs_probe_default(ProbeOut input) : SV_Target {
    float3 d = face_dir((int)pp_p.y, input.uv.x * 2.0f - 1.0f, 1.0f - input.uv.y * 2.0f);
    float3 c = env.SampleLevel(env_sampler, d, pp_p.x).rgb;
    return float4(c * c, 1.0f);
}

float4 fs_probe_blit(ProbeOut input) : SV_Target {
    uint w, h;
    face_src.GetDimensions(w, h);
    int2 p = (int2)input.pos.xy;
    return float4(face_src.Load(int3((int)w - 1 - p.x, p.y, 0)).rgb, 1.0f);
}

float4 fs_probe_down(ProbeOut input) : SV_Target {
    float z = pp_p.z;
    int face = (int)pp_p.y;
    float x0 = input.uv.x * 2.0f - 0.75f * z - 1.0f;
    float x1 = input.uv.x * 2.0f + 0.75f * z - 1.0f;
    float y0 = -input.uv.y * 2.0f + 0.75f * z + 1.0f;
    float y1 = -input.uv.y * 2.0f - 0.75f * z + 1.0f;
    float r00 = x0 * x0 + y0 * y0 + 1.0f;
    float r10 = x1 * x1 + y0 * y0 + 1.0f;
    float r01 = x0 * x0 + y1 * y1 + 1.0f;
    float r11 = x1 * x1 + y1 * y1 + 1.0f;
    float w00 = r00 * sqrt(r00);
    float w10 = r10 * sqrt(r10);
    float w01 = r01 * sqrt(r01);
    float w11 = r11 * sqrt(r11);
    float norm = 0.5f / (w00 + w10 + w01 + w11);
    float lod = pp_p.x;
    float4 c = env.SampleLevel(env_sampler, face_dir(face, x0, y0), lod) * (w00 * norm + 0.125f)
        + env.SampleLevel(env_sampler, face_dir(face, x1, y0), lod) * (w10 * norm + 0.125f)
        + env.SampleLevel(env_sampler, face_dir(face, x0, y1), lod) * (w01 * norm + 0.125f)
        + env.SampleLevel(env_sampler, face_dir(face, x1, y1), lod) * (w11 * norm + 0.125f);
    return float4(c.rgb, 1.0f);
}

float4 fs_probe_conv(ProbeOut input) : SV_Target {
    int level = (int)pp_p.x;
    float3 v1 = face_dir((int)pp_p.y, input.uv.x * 2.0f - 1.0f, 1.0f - input.uv.y * 2.0f);
    float3 n = normalize(v1);
    float3 a = abs(v1);
    float4 acc = float4(0.0f, 0.0f, 0.0f, 0.0f);
    [unroll]
    for (int i = 0; i < 3; i++) {
        int hi = i >> 1;
        int first = (1 - (i & 1)) - hi;
        int second = 2 - hi;
        float af = (first == 0) ? a.x : ((first == 1) ? a.y : a.z);
        float as_ = (second == 0) ? a.x : ((second == 1) ? a.y : a.z);
        float w_axis = (max(af, as_) - 0.75f) * 4.0f;
        if (w_axis > 0.0f) {
            float3 axis = float3(0.0f, 0.0f, 1.0f);
            if (i == 0) { axis = float3(1.0f, 0.0f, 0.0f); }
            if (i == 1) { axis = float3(0.0f, 1.0f, 0.0f); }
            float3 t = normalize(cross(axis, n));
            float3 b = cross(n, t);
            [unroll]
            for (int j = 0; j < 2; j++) {
                int k = 2 * i + j;
                int base_idx = level * 30;
                float4 ca = coeff[base_idx + k];
                float4 cb = coeff[base_idx + 6 + k];
                float4 cc = coeff[base_idx + 12 + k];
                float4 cl = coeff[base_idx + 18 + k];
                float4 cw = coeff[base_idx + 24 + k];
                [unroll]
                for (uint c = 0u; c < 4u; c++) {
                    float3 d = t * lane4(ca, c) + b * lane4(cb, c) + n * lane4(cc, c);
                    float w = lane4(cw, c) * w_axis;
                    float3 dc = d / max(abs(d.x), max(abs(d.y), abs(d.z)));
                    float lod = min(5.0f, lane4(cl, c) + 0.75f * log2(dot(dc, dc)));
                    acc += float4(env.SampleLevel(env_sampler, dc, lod).rgb * w, w);
                }
            }
        }
    }
    float4 r = acc / acc.w;
    return float4(max(r.rgb, float3(0.0f, 0.0f, 0.0f)), 1.0f);
}
