#include "common.hlsli"

float4 vs_water(float3 pos : TEXCOORD0) : SV_Position { return float4(pos, 1); }
float4 fs_water(float4 p : SV_Position) : SV_Target { return float4(0, 0.3f, 0.6f, 0.8f); }
