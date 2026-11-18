#include "common.hlsli"

float4 vs_gui(float3 pos : TEXCOORD0) : SV_Position { return float4(pos, 1); }
float4 fs_gui(float4 p : SV_Position) : SV_Target { return float4(1, 1, 1, 1); }

float4 vs_mip(uint id : SV_VertexID) : SV_Position { float4 p; float2 uv; fullscreen_tri(id, p, uv); return p; }
float4 fs_mip(float4 p : SV_Position) : SV_Target { return float4(0, 0, 0, 1); }

float4 vs_strip(float3 pos : TEXCOORD0) : SV_Position { return float4(pos, 1); }
float4 vs_particle(float3 pos : TEXCOORD0) : SV_Position { return float4(pos, 1); }
float4 fs_fx(float4 p : SV_Position) : SV_Target { return float4(1, 1, 1, 1); }

float4 vs_glass_mask(float3 pos : TEXCOORD0) : SV_Position { return float4(pos, 1); }
float4 fs_in(float4 p : SV_Position) : SV_Target { return float4(1, 1, 1, 1); }
float4 fs_out(float4 p : SV_Position) : SV_Target { return float4(0, 0, 0, 0); }
