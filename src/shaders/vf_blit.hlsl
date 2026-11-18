#include "common.hlsli"

Texture2D<float4> src_tex : register(t0);
SamplerState src_samp     : register(s0);

struct VfOut {
    float4 pos : SV_Position;
    float2 uv  : TEXCOORD0;
};

VfOut vs(uint id : SV_VertexID) {
    VfOut o;
    fullscreen_tri(id, o.pos, o.uv);
    return o;
}

float4 fs(VfOut input) : SV_Target {
    float4 c = src_tex.Load(int3((int2)input.pos.xy, 0));
    float a = saturate(c.a);
    if (a <= 0.0f) {
        return float4(0.0f, 0.0f, 0.0f, 0.0f);
    }
    return float4(sqrt(max(c.rgb / a, float3(0.0f, 0.0f, 0.0f))) * a, a);
}
