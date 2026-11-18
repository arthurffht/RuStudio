#include "common.hlsli"

struct ClOut {
    float4 pos : SV_Position;
    float2 uv : TEXCOORD0;
};

ClOut vs_clouds(uint id : SV_VertexID) {
    ClOut o;
    fullscreen_tri(id, o.pos, o.uv);
    return o;
}

float4 fs_clouds_bsm(ClOut i) : SV_Target { return float4(1, 1, 1, 1); }
float4 fs_clouds_cover(ClOut i) : SV_Target { return float4(0, 0, 0, 0); }
float4 fs_clouds_wind(ClOut i) : SV_Target { return float4(0, 0, 0, 0); }
float4 fs_clouds_up(ClOut i) : SV_Target { return float4(0, 0, 0, 0); }
float4 fs_clouds_comp(ClOut i) : SV_Target { return float4(0, 0, 0, 1); }
