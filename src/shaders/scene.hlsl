#include "common.hlsli"

float4 vs_sky(uint id : SV_VertexID) : SV_Position { float4 p; float2 uv; fullscreen_tri(id, p, uv); return p; }
float4 fs_sky(float4 p : SV_Position) : SV_Target { return float4(0, 0, 0, 1); }

float4 vs_part(float3 pos : TEXCOORD0) : SV_Position { return float4(pos, 1); }
float4 fs_part(float4 p : SV_Position) : SV_Target { return float4(1, 0, 1, 1); }
float4 fs_part_enc(float4 p : SV_Position) : SV_Target { return float4(1, 0, 1, 1); }
float4 fs_decal_depth(float4 p : SV_Position) : SV_Target { return float4(1, 1, 1, 1); }

float4 fs_see_encode(float4 p : SV_Position, uint s : SV_SampleIndex) : SV_Target { return float4(0, 0, 0, 0); }
float4 fs_see_decode(float4 p : SV_Position, uint s : SV_SampleIndex) : SV_Target { return float4(0, 0, 0, 0); }

float4 vs_light_shadow(float3 pos : TEXCOORD0) : SV_Position { return float4(pos, 1); }
float4 fs_light_shadow(float4 p : SV_Position) : SV_Target { return float4(0, 0, 0, 1); }

float4 vs_shadow(float3 pos : TEXCOORD0) : SV_Position { return float4(pos, 1); }

float4 vs_terrain(float3 pos : TEXCOORD0) : SV_Position { return float4(pos, 1); }
float4 fs_terrain(float4 p : SV_Position) : SV_Target { return float4(0, 1, 0, 1); }
float4 vs_terrain_shadow(float3 pos : TEXCOORD0) : SV_Position { return float4(pos, 1); }

float4 vs_line(float3 pos : TEXCOORD0) : SV_Position { return float4(pos, 1); }
float4 fs_line(float4 p : SV_Position) : SV_Target { return float4(1, 1, 1, 1); }

float fs_glass_depth(float4 p : SV_Position) : SV_Target { return 0.0f; }
