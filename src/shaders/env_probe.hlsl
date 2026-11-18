#include "common.hlsli"

struct ProbeOut {
    float4 pos : SV_Position;
    float2 uv : TEXCOORD0;
};

ProbeOut vs_probe(uint id : SV_VertexID) {
    ProbeOut o;
    fullscreen_tri(id, o.pos, o.uv);
    return o;
}

float4 fs_probe_default(ProbeOut i) : SV_Target { return float4(0, 0, 0, 1); }
float4 fs_probe_blit(ProbeOut i) : SV_Target { return float4(0, 0, 0, 1); }
float4 fs_probe_down(ProbeOut i) : SV_Target { return float4(0, 0, 0, 1); }
float4 fs_probe_conv(ProbeOut i) : SV_Target { return float4(0, 0, 0, 1); }
