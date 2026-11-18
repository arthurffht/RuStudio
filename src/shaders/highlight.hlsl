#include "common.hlsli"

cbuffer ParamsCB : register(b0) {
    float4x4 p_view_proj;
    float4 p_fill;
    float4 p_outline;
    float4 p_mode;
    float4 _pad[9];
};

Texture2D<float4> mask : register(t0);

struct HlIn {
    float3 pos  : TEXCOORD0;
    float4 r0   : TEXCOORD2;
    float4 r1   : TEXCOORD3;
    float4 r2   : TEXCOORD4;
    float4 size : TEXCOORD5;
};

float4 vs_mask(HlIn v) : SV_Position {
    float3 l = v.pos * v.size.xyz;
    float3 origin = float3(v.r0.w, v.r1.w, v.r2.w);
    float3 world = v.r0.xyz * l.x + v.r1.xyz * l.y + v.r2.xyz * l.z + origin;
    float4 c = mul(p_view_proj, float4(world, 1.0f));
    c.z = c.z * (1.0f + v.size.w);
    return c;
}

float4 fs_mask() : SV_Target {
    return float4(1.0f, 1.0f, 1.0f, 1.0f);
}

float4 vs_full(uint id : SV_VertexID) : SV_Position {
    float4 pos;
    float2 uv;
    fullscreen_tri(id, pos, uv);
    return pos;
}

bool inside(int2 q) {
    uint w, h;
    mask.GetDimensions(w, h);
    int2 d = int2((int)w, (int)h);
    if (q.x < 0 || q.y < 0 || q.x >= d.x || q.y >= d.y) {
        return false;
    }
    float4 m = mask.Load(int3(q, 0));
    return ((p_mode.x > 0.5f) ? m.r : m.g) > 0.5f;
}

float4 fs_comp(float4 pos : SV_Position) : SV_Target {
    int2 q = (int2)pos.xy;
    bool me = inside(q);
    bool edge = inside(q + int2(1, 0)) != me || inside(q - int2(1, 0)) != me
        || inside(q + int2(0, 1)) != me || inside(q - int2(0, 1)) != me;
    float af = me ? p_fill.a : 0.0f;
    float ao = edge ? p_outline.a : 0.0f;
    if (af <= 0.0f && ao <= 0.0f) {
        discard;
    }
    float3 rgb = p_fill.rgb * af * (1.0f - ao) + p_outline.rgb * ao;
    float a = 1.0f - (1.0f - af) * (1.0f - ao);
    return float4(rgb, a);
}
