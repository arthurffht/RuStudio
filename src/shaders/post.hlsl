#include "common.hlsli"

struct PostOut {
    float4 pos : SV_Position;
    float2 uv : TEXCOORD0;
};

PostOut vs_post(uint id : SV_VertexID) {
    PostOut o;
    fullscreen_tri(id, o.pos, o.uv);
    return o;
}

float4 fs_bloom_bright(PostOut i) : SV_Target { return float4(0, 0, 0, 1); }
float4 fs_bloom_down(PostOut i) : SV_Target { return float4(0, 0, 0, 1); }
float4 fs_bloom_up(PostOut i) : SV_Target { return float4(0, 0, 0, 1); }
float4 fs_dof_h(PostOut i) : SV_Target { return float4(0, 0, 0, 1); }
float4 fs_near_h(PostOut i) : SV_Target { return float4(0, 0, 0, 1); }
float4 fs_near_v(PostOut i) : SV_Target { return float4(0, 0, 0, 1); }
float4 fs_near_mask0(PostOut i) : SV_Target { return float4(0, 0, 0, 1); }
float4 fs_near_mask1(PostOut i) : SV_Target { return float4(0, 0, 0, 1); }
float4 fs_near_mask2(PostOut i) : SV_Target { return float4(0, 0, 0, 1); }
float4 fs_near_mask3(PostOut i) : SV_Target { return float4(0, 0, 0, 1); }
float4 fs_near_mask4(PostOut i) : SV_Target { return float4(0, 0, 0, 1); }
float4 fs_near_mask5(PostOut i) : SV_Target { return float4(0, 0, 0, 1); }
float4 fs_near_edge(PostOut i) : SV_Target { return float4(0, 0, 0, 1); }
float4 fs_near_bokeh(PostOut i) : SV_Target { return float4(0, 0, 0, 1); }
float4 fs_near_tent(PostOut i) : SV_Target { return float4(0, 0, 0, 1); }
float4 fs_dof_v(PostOut i) : SV_Target { return float4(0, 0, 0, 1); }
float4 fs_post(PostOut i) : SV_Target { return float4(0, 0, 0, 1); }
float4 fs_ssao_down(PostOut i) : SV_Target { return float4(0, 0, 0, 1); }
float4 fs_ssao(PostOut i) : SV_Target { return float4(1, 1, 1, 1); }
float4 fs_ssao_blur_x(PostOut i) : SV_Target { return float4(1, 1, 1, 1); }
float4 fs_ssao_blur_y(PostOut i) : SV_Target { return float4(1, 1, 1, 1); }
float4 fs_ssao_apply(PostOut i) : SV_Target { return float4(1, 1, 1, 1); }
