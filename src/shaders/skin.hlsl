#include "common.hlsli"

cbuffer ParamsCB : register(b0) {
    float4 p_to_part;
    float4 p_from_part;
    float4 p_extent;
    uint4 p_counts; // x: corners, y: bones
    float4 p_origin;
};

StructuredBuffer<float4> src     : register(t0);
StructuredBuffer<float4> palette : register(t1);
RWByteAddressBuffer dst          : register(u0);

[numthreads(64, 1, 1)]
void main(uint3 gid : SV_DispatchThreadID) {
    uint i = gid.x;
    if (i >= p_counts.x) {
        return;
    }
    float3 pp = src[i * 4u].xyz * p_to_part.xyz;
    float3 n = src[i * 4u + 1u].xyz / p_extent.xyz;
    float4 w = src[i * 4u + 2u];
    uint4 ids = asuint(src[i * 4u + 3u]);

    float3 q = float3(0.0f, 0.0f, 0.0f);
    float3 m = float3(0.0f, 0.0f, 0.0f);
    float total = 0.0f;
    [unroll]
    for (uint k = 0u; k < 4u; k++) {
        float wk = lane4(w, k);
        uint b = (k == 0u) ? ids.x : ((k == 1u) ? ids.y : ((k == 2u) ? ids.z : ids.w));
        if (wk > 0.0f && b < p_counts.y) {
            float4 r0 = palette[b * 3u];
            float4 r1 = palette[b * 3u + 1u];
            float4 r2 = palette[b * 3u + 2u];
            q += float3(dot(r0.xyz, pp) + r0.w, dot(r1.xyz, pp) + r1.w, dot(r2.xyz, pp) + r2.w) * wk;
            m += float3(dot(r0.xyz, n), dot(r1.xyz, n), dot(r2.xyz, n)) * wk;
            total += wk;
        }
    }

    float shortfall = 1.0f - total;
    if (abs(shortfall) > 1e-4f) {
        q += p_origin.xyz * shortfall;
    }
    float3 p = q * p_from_part.xyz;
    float3 nn = float3(0.0f, 0.0f, 0.0f);
    if (dot(m, m) > 0.0f) {
        nn = normalize(m) * p_extent.xyz;
    }

    uint byte_off = i * 64u;
    dst.Store3(byte_off, asuint(p));
    dst.Store3(byte_off + 12u, asuint(nn));
}
