#include "common.hlsli"

cbuffer EguiCB : register(b0) {
    float2 u_screen_size;
    float2 _pad;
};

Texture2D<float4> egui_tex : register(t0);
SamplerState egui_sampler : register(s0);

struct EguiIn {
    float2 pos : POSITION;
    float2 uv  : TEXCOORD0;
    float4 col : COLOR0;
};

struct EguiOut {
    float4 pos : SV_Position;
    float2 uv  : TEXCOORD0;
    float4 col : COLOR0;
};

EguiOut vs_main(EguiIn v) {
    EguiOut o;
    o.pos = float4(
        2.0f * v.pos.x / max(u_screen_size.x, 1.0f) - 1.0f,
        1.0f - 2.0f * v.pos.y / max(u_screen_size.y, 1.0f),
        0.0f,
        1.0f
    );
    o.uv = v.uv;
    o.col = v.col;
    return o;
}

float4 ps_main(EguiOut i) : SV_Target {
    return egui_tex.Sample(egui_sampler, i.uv) * i.col;
}
