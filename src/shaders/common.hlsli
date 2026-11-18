#ifndef RUSTUDIO_COMMON_HLSLI
#define RUSTUDIO_COMMON_HLSLI

#pragma pack_matrix(column_major)
#include "mech_constants.hlsli"

#ifndef MSAA_SAMPLES
#define MSAA_SAMPLES 1
#endif

// Standard fullscreen triangle helper: i in 0..3 -> clip pos (-1..3) and uv (0..2).
// Note DX11 NDC: Y=+1 top, Y=-1 bottom; UV (0,0) is top-left!
void fullscreen_tri(uint id, out float4 pos, out float2 uv) {
    float2 p = float2((id << 1) & 2, id & 2);
    uv = p;
    pos = float4(p.x * 2.0f - 1.0f, 1.0f - p.y * 2.0f, 0.0f, 1.0f);
}

#endif
