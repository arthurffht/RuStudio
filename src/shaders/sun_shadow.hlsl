#include "common.hlsli"

cbuffer SunPassCB : register(b0) {
    float4x4 u_vp;
    float4 u_rect;
    float4 u_hw[3];
    float4 u_vw[2];
    float4 u_vo;
    float4 u_centre;
};

Texture2D<float> depth_tex    : register(t0);
Texture2D<float4> moments_tex : register(t1);
SamplerState moments_sampler  : register(s0);

float4 vs(uint id : SV_VertexID) : SV_Position {
    float4 p;
    float2 uv;
    fullscreen_tri(id, p, uv);
    return p;
}

float4 moments(int2 px) {
    int hi = (int)u_rect.x - 1;
    float z = 2.0f * depth_tex.Load(int3(clamp(px, int2(0, 0), int2(hi, hi)), 0)) - 1.0f;
    float ep = exp(EVSM_POS_EXP * z);
    float en = -exp(-EVSM_NEG_EXP * z);
    return float4(ep, ep * ep, en, en * en);
}

float4 fs_h(float4 p : SV_Position) : SV_Target {
    int2 px = (int2)floor(p.xy);
    float4 sum = moments(px) * u_hw[0].x;
    [unroll]
    for (int i = 1; i < 9; i++) {
        float w = lane4(u_hw[i / 4], (uint)(i % 4));
        if (w <= 0.0f) {
            break;
        }
        sum += (moments(px + int2(i, 0)) + moments(px - int2(i, 0))) * w;
    }
    return sum;
}

float4 fs_v(float4 p : SV_Position) : SV_Target {
    uint w, h;
    moments_tex.GetDimensions(w, h);
    float2 size = float2((float)w, (float)h);
    float2 at = p.xy - u_rect.zw;
    float lo = 0.5f;
    float hi = u_rect.x - 0.5f;
    float4 sum = moments_tex.SampleLevel(moments_sampler, at / size, 0.0f) * u_vw[0].x;
    [unroll]
    for (int i = 1; i < 5; i++) {
        float wt = lane4(u_vw[i / 4], (uint)(i % 4));
        if (wt <= 0.0f) {
            break;
        }
        float o = lane4(u_vo, (uint)(i - 1));
        float2 a = float2(at.x, clamp(at.y + o, lo, hi)) / size;
        float2 b = float2(at.x, clamp(at.y - o, lo, hi)) / size;
        sum += (moments_tex.SampleLevel(moments_sampler, a, 0.0f) + moments_tex.SampleLevel(moments_sampler, b, 0.0f)) * wt;
    }
    return sum;
}
