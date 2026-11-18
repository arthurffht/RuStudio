#include "common.hlsli"

Texture2D<float4> src_tex : register(t0);
SamplerState src_samp : register(s0);

struct VfOut {
    float4 pos : SV_Position;
    float2 uv : TEXCOORD0;
};

VfOut vs(uint id : SV_VertexID) {
    VfOut o;
    fullscreen_tri(id, o.pos, o.uv);
    return o;
}

float4 fs(VfOut i) : SV_Target {
    return src_tex.Sample(src_samp, i.uv);
}
