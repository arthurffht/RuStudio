#define SCENE_GROUP0
#include "common.hlsli"

cbuffer CloudsUCB : register(b4) {
    float4 cu_dir_dist;
    float4 cu_cdq;
    float4 cu_color;
    float4 cu_detail;
    float4 cu_fogp;
    float4 cu_quarter;
    float4 cu_half;
    float4 cu_full;
    float4 cu_cover;
};

Texture3D<float4> cl_dist   : register(t24);
Texture2D<float4> cl_adv    : register(t25);
Texture3D<float4> cl_detail : register(t26);
Texture2D<float4> cl_noise  : register(t27);
Texture2D<float4> cl_bsm    : register(t28);
Texture2D<float4> cl_in     : register(t29);
Texture2D<float4> cl_scene  : register(t30);
TEX_DEPTH_MS cl_depth       : register(t31);

SamplerState cl_rep    : register(s5);
SamplerState cl_dist_s : register(s6);
SamplerState cl_clamp  : register(s7);

static const float CL_PHASE[48] = {
    15.717910f, 12.894090f, 7.719984f, 5.107763f, 3.957092f, 3.128542f, 2.467976f, 1.940606f,
    1.519268f, 1.172060f, 0.906811f, 0.696639f, 0.530980f, 0.403742f, 0.304674f, 0.230043f,
    0.172465f, 0.128985f, 0.096062f, 0.072035f, 0.054671f, 0.041893f, 0.032902f, 0.026549f,
    0.022501f, 0.020169f, 0.018803f, 0.018286f, 0.018086f, 0.022949f, 0.028057f, 0.033098f,
    0.033073f, 0.036000f, 0.051087f, 0.108068f, 0.172808f, 0.207800f, 0.132369f, 0.111766f,
    0.110401f, 0.105231f, 0.104189f, 0.108498f, 0.127474f, 0.207567f, 0.319779f, 0.467940f
};

static const float2 CL_DISK[27] = {
    float2(0.54f, 0.0f), float2(0.4368692f, 0.31740406f), float2(0.16686918f, 0.51357054f), float2(-0.16686918f, 0.51357054f), float2(-0.4368692f, 0.31740406f),
    float2(-0.54f, 0.0f), float2(-0.4368692f, -0.31740406f), float2(-0.16686918f, -0.51357054f), float2(0.16686918f, -0.51357054f), float2(0.4368692f, -0.31740406f),
    float2(1.0f, 0.0f), float2(0.9324722f, 0.36124167f), float2(0.7390089f, 0.67369562f), float2(0.44573835f, 0.8951633f), float2(0.09226836f, 0.99573416f),
    float2(-0.27366298f, 0.96182567f), float2(-0.6026346f, 0.7980172f), float2(-0.85021716f, 0.52643216f), float2(-0.9829731f, 0.18374951f), float2(-0.9829731f, -0.18374951f),
    float2(-0.85021716f, -0.52643216f), float2(-0.6026346f, -0.7980172f), float2(-0.27366298f, -0.96182567f), float2(0.09226836f, -0.99573416f), float2(0.44573835f, -0.8951633f),
    float2(0.7390089f, -0.67369562f), float2(0.9324722f, -0.36124167f)
};

struct ClOut {
    float4 pos : SV_Position;
    float2 uv  : TEXCOORD0;
};

ClOut vs_clouds(uint id : SV_VertexID) {
    ClOut o;
    fullscreen_tri(id, o.pos, o.uv);
    return o;
}

float3 cl_ray(float2 px, float2 size) {
    float2 ndc = float2(2.0f * px.x / size.x - 1.0f, 1.0f - 2.0f * px.y / size.y);
    float4 far_p = mul(g.inv_view_proj, float4(ndc, 1.0f, 1.0f));
    return normalize(far_p.xyz / far_p.w);
}

float2 cl_shell(float3 dir, float3 cam, float2 r2) {
    float3 o = cam;
    o.y = cam.y + 971.0f;
    float b = dot(dir, o);
    float b2 = 2.0f * b;
    return ((float2)(b * -2.0f) + sqrt(max((float2)(b2 * b2) - ((float2)dot(o, o) - r2) * 4.0f, float2(0.0f, 0.0f)))) * 0.5f;
}

static const float2 CL_LAYER = float2(946729.0f, 951600.25f);

float3 cl_cam() {
    return g.cam_pos.xyz * 0.00028f;
}

float3 cl_ldir() {
    return -normalize(g.sun_dir.xyz);
}

float2 cl_field(float3 p, float lod) {
    float2 a = cl_adv.SampleLevel(cl_rep, p.xz * 0.125f + float2(0.0f, cu_dir_dist.w * -17.5f), lod).xy * 2.0f - 1.0f;
    float3 adv = float3(a.x, 0.0f, a.y);
    float3 q = p;
    q.x = p.x + cu_dir_dist.w * 39.0625f;
    float dens = cu_cdq.y * saturate(1.0f + 0.001f * (6000.0f - g.cam_pos.y));
    float2 m = lerp(float2(0.15f, 0.07f), float2(0.075f, 0.05f), dens);
    float2 det = float2(cu_detail.x, 0.35f * cu_detail.x) * (float2(
        cl_detail.SampleLevel(cl_rep, q * (0.5f * cu_detail.y) + adv * m.x, lod).x,
        cl_detail.SampleLevel(cl_rep, q * (2.0f * cu_detail.y) + adv * m.y, lod).x) * 2.0f - 1.0f);
    float d = cl_dist.SampleLevel(cl_dist_s, ((p + adv * 0.75f) * float3(0.03125f, 1.0f, 0.03125f)).xzy, lod).x;
    return float2(d + (det.x + det.y), dens);
}

float4 fs_clouds_bsm(ClOut input) : SV_Target {
    float2 uv = input.pos.xy * cu_cover.z;
    float3 l = -cl_ldir();
    l.y = (l.y < 0.0f) ? min(-0.3f, l.y) : max(0.3f, l.y);
    l = normalize(l);
    float step_len = 0.083333336f / max(abs(l.y), 1e-4f);
    float3 p0 = float3(32.0f * uv.x, (l.y <= 0.0f) ? 0.984375f : 0.015625f, 32.0f * uv.y);
    float lod = (cu_cdq.z <= 9.0f) ? 1.0f : 0.0f;
    float thr = 1.04f * cu_cdq.x;
    float first = -1.0f;
    float sum = 0.0f;
    float scaled = 0.0f;
    float tr = 1.0f;
    int n = 1;
    [loop]
    for (int i = 0; i <= 12; i++) {
        float2 f = cl_field(p0 + l * (float)i * step_len, lod);
        if (f.x < thr && tr > 1e-5f) {
            float s = f.y * (thr - f.x);
            sum += s;
            scaled = sum * 1.4880953f;
            if (first < 0.0f) {
                first = scaled;
            }
            tr *= exp2(s * -0.12022459f);
            n += 1;
        }
    }
    float back = 0.0f;
    if (n > 1) {
        float3 pe = p0 + l * 12.0f * step_len;
        float t2 = tr;
        float acc = 0.0f;
        [loop]
        for (int j = 0; j <= 12; j++) {
            float2 f = cl_field(pe - l * (float)j * step_len, lod);
            if (f.x < thr) {
                float s = f.y * (thr - f.x);
                float t3 = t2 * exp2(s * -0.12022459f);
                acc += s;
                if (t3 <= 1e-5f) {
                    break;
                }
                t2 = t3;
            }
        }
        back = acc;
    }
    return float4(1.0f - 0.5f * scaled, sum / (float)n, 1.0f - 0.5f * first, back);
}

float4 fs_clouds_cover(ClOut input) : SV_Target {
    float3 cam = cl_cam();
    float3 l = -cl_ldir();
    float2 t = cl_shell(l, cam, CL_LAYER);
    float2 c = (((cam + l * t.x) + l * (t.y - t.x)) + (cu_dir_dist.xyz * cu_dir_dist.w) * 50.0f).xz * 0.03125f;
    float r = cu_cover.y * cu_cover.z;
    float sum = cl_bsm.SampleLevel(cl_rep, c, 0.0f).y;
    [unroll]
    for (int i = 0; i < 27; i++) {
        sum += cl_bsm.SampleLevel(cl_rep, c + CL_DISK[i] * r, 0.0f).y;
    }
    float avg = sum * 0.035714287f;
    return (float4)max(0.0f, 1.0f - cu_cover.x * (avg * avg));
}

float4 fs_clouds_wind(ClOut input) : SV_Target {
    float3 dir = cl_ray(input.pos.xy, cu_quarter.xy);
    if (dir.y < 0.0f) {
        discard;
    }
    float3 cam = cl_cam();
    float2 t = cl_shell(dir, cam, CL_LAYER);
    float t0 = t.x;
    float span = t.y - t0;
    float3 ldir = cl_ldir();
    float mu = dot(ldir, dir);
    float fi = (0.5f + 0.5f * mu) * 46.5f;
    int ii = clamp((int)fi, 0, 46);
    float ph = lerp(CL_PHASE[ii], CL_PHASE[ii + 1], frac(fi));
    float ph8 = ph * 0.125f;
    float3 entry = cam + dir * t0;
    entry.y = 0.0f;
    int steps = (int)cu_cdq.z;
    float nf = (float)steps;
    float t_step = (1.0f / nf) * span;
    float p_step = (1.0f / max(dir.y, 1e-4f)) / nf;
    float bn = cl_noise.SampleLevel(cl_rep, input.pos.xy, 0.0f).x;
    float3 p = entry + (dir * span) * (0.15f + 0.95f * bn);
    p.y = 0.0f;
    p += cu_dir_dist.xyz;
    float3 lamp0 = g.sun_color.rgb * g.sun_dir.w;
    float3 d2 = dir * dir;
    float3 neg = (dir < 0.0f) ? d2 : float3(0.0f, 0.0f, 0.0f);
    float3 pos_d = d2 - neg;
    float3 cube = g.env[0].rgb * pos_d.x + g.env[2].rgb * pos_d.y + g.env[4].rgb * pos_d.z
        + g.env[1].rgb * neg.x + g.env[3].rgb * neg.y + g.env[5].rgb * neg.z;
    float fade_sun = saturate(exp2(ldir.y));
    float lod0 = (cu_cdq.z <= 9.0f) ? 1.0f : 0.0f;
    float3 col = float3(0.0f, 0.0f, 0.0f);
    float tr = 1.0f;
    float acc = 0.0f;
    float ti = t0;
    bool hit = false;
    float end_tr = 1.0f;
    float end_acc = 0.0f;
    bool done = false;
    [loop]
    for (int i = 0; i < steps; i++) {
        float lod = lod0 + pow(abs(max(ti, 0.0f)), 3.0f) * 2.631579e-5f;
        float2 f = cl_field(p, lod);
        float s = (f.y * 50.0f) * (cu_cdq.x - f.x);
        bool inside = f.x < cu_cdq.x;
        if (inside) {
            float4 b = cl_bsm.SampleLevel(cl_rep, p.xz * 0.03125f, 0.0f);
            float od = min(b.z * 0.5f, (b.y + 0.35f * bn) * max(0.0f, (1.0f - ((float)i / nf + f.x / nf)) - b.x));
            float acc2 = acc + s;
            float3 grad = lerp((float3)(0.15f + 0.3f * p.y), lerp(g.sky_grad[1].rgb, g.sky_grad[0].rgb, p.y), fade_sun);
            float3 amb = max(grad * cube, g.outdoor.rgb + g.ambient.rgb);
            float lobes = ((0.29f * exp2(od * -23.70142f)) + ((0.169f * lerp(0.08f, 1.0f, smoothstep(0.0f, 1.5f, abs(mu)))) * exp2(od * -659.51776f)))
                + (((1.0f - f.y) * 320.0f) * saturate(mu * ph8)) * exp2(od * -10.0f);
            float3 sun = lamp0 * ((4.0f * (lobes * (ph * 0.02925f) + 0.092f * exp2(b.w * -2.325f))) + (0.0425f + ((s / nf) * 0.135f) * p.y));
            float k = (1.442695f / nf) * s;
            float3 src = ((amb + sun * 2.1f) * cu_color.rgb) * k;
            float e = exp2(-k);
            col += ((src - src * e) * tr) / ((k < 1e-4f) ? 1e-4f : k);
            float tr2 = tr * e;
            if (tr2 <= 1e-5f) {
                end_tr = 0.0f;
                end_acc = acc2;
                done = true;
                break;
            }
            tr = tr2;
            acc = acc2;
        }
        hit = hit || inside;
        ti += t_step;
        p += dir * p_step;
    }
    if (!done) {
        end_tr = tr;
        end_acc = acc;
    }
    float alpha = 1.0f - end_tr;
    col += ((((lamp0 * saturate(1.0f - (0.08f * end_acc) / nf)) * 2.0f) * saturate(-mu)) * ph8 * alpha)
        * exp2(0.11f - 0.035f * end_acc) * cu_color.rgb;
    if (!hit) {
        discard;
    }
    float fogf = pow(abs(max(exp2((cu_fogp.z * 3.5714285f) * (t0 * t0)), 1e-4f)), 0.125f);
    float3 sky;
    if (cu_fogp.w != 0.0f) {
        sky = g.fog_color.rgb;
    } else {
        sky = sky_color_lod(dir, max(cu_fogp.y, fogf) * 5.0f);
    }
    return float4(lerp(sky, col, fogf), alpha * max(fogf, cu_fogp.y));
}

float4 cl_tent(float2 uv, float2 texel) {
    float4 o = float4(1.0f, 1.0f, -1.0f, 0.0f) * float4(texel, texel);
    return (cl_in.SampleLevel(cl_clamp, uv, 0.0f) * 4.0f
        + cl_in.SampleLevel(cl_clamp, uv - o.xy, 0.0f)
        + cl_in.SampleLevel(cl_clamp, uv - o.wy, 0.0f) * 2.0f
        + cl_in.SampleLevel(cl_clamp, uv - o.zy, 0.0f)
        + cl_in.SampleLevel(cl_clamp, uv + o.zw, 0.0f) * 2.0f
        + cl_in.SampleLevel(cl_clamp, uv + o.xw, 0.0f) * 2.0f
        + cl_in.SampleLevel(cl_clamp, uv + o.zy, 0.0f)
        + cl_in.SampleLevel(cl_clamp, uv + o.wy, 0.0f) * 2.0f
        + cl_in.SampleLevel(cl_clamp, uv + o.xy, 0.0f)) * 0.0625f;
}

float4 fs_clouds_up(ClOut input) : SV_Target {
    float3 dir = cl_ray(input.pos.xy, cu_half.xy);
    if (dir.y < -0.5f) {
        discard;
    }
    float2 uv = input.pos.xy * cu_half.zw;
    float4 c = cl_tent(uv, cu_half.zw);
    float3 cam = cl_cam();
    float2 t = cl_shell(dir, cam, CL_LAYER);
    float3 far_p = (cam + dir * t.x) + dir * (t.y - t.x);
    far_p.y = 0.0f;
    float dist = length(far_p - cam) + (t.y - t.x);
    float k = saturate((dist - 227.45f) * 0.09756097f);
    c.w = lerp(c.w, 0.0f, k * k);
    return c;
}

float4 fs_clouds_comp(ClOut input) : SV_Target {
    int2 px = (int2)input.pos.xy;
    if (ms_load(cl_depth, px, 0u) > 0.0f) {
        discard;
    }
    float3 dir = cl_ray(input.pos.xy, cu_full.xy);
    if (dir.y < -0.01f) {
        discard;
    }
    float2 uv = input.pos.xy * cu_full.zw;
    float4 c = cl_tent(uv, cu_full.zw);
    if (!(c.w > 0.12f)) {
        discard;
    }
    float e = g.params.y * 0.25f;
    float3 ec = sqrt(saturate(c.rgb * e));
    float a = c.w * c.w;
    float3 dst = cl_scene.Load(int3(px, 0)).rgb;
    float3 ed = sqrt(saturate(dst * e));
    float3 o = ec * a + ed * (1.0f - a);
    return float4(o * o / max(e, 1e-6f), 1.0f);
}
