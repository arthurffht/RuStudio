#include "common.hlsli"

float4 vs(uint id : SV_VertexID) : SV_Position { float4 p; float2 uv; fullscreen_tri(id, p, uv); return p; }
float4 fs_h(float4 p : SV_Position) : SV_Target { return float4(0, 0, 0, 0); }
float4 fs_v(float4 p : SV_Position) : SV_Target { return float4(0, 0, 0, 0); }
