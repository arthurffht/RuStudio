#include "common.hlsli"

float4 vs_mask(float3 pos : TEXCOORD0) : SV_Position { return float4(pos, 1); }
float4 fs_mask() : SV_Target { return float4(1, 0, 0, 0); }
float4 vs_full(uint id : SV_VertexID) : SV_Position { float4 p; float2 uv; fullscreen_tri(id, p, uv); return p; }
float4 fs_comp(float4 p : SV_Position) : SV_Target { return float4(0, 0, 0, 0); }
