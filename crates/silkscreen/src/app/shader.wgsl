// YUV (NV12, NV24, I420 or I444) to RGB.

struct Params {
    // rgb[i] = dot(row_i.xyz, yuv) + row_i.w
    row_r: vec4<f32>,
    row_g: vec4<f32>,
    row_b: vec4<f32>,
    // 0 = interleaved UV in tex_u (NV12, NV24), 1 = separate U and V (I420, I444)
    mode: u32,
    // 1 when the swapchain is sRGB and expects linear output
    linearize: u32,
    _pad: vec2<u32>,
}

@group(0) @binding(0) var tex_y: texture_2d<f32>;
@group(0) @binding(1) var tex_u: texture_2d<f32>;
@group(0) @binding(2) var tex_v: texture_2d<f32>;
@group(0) @binding(3) var samp: sampler;
@group(0) @binding(4) var<uniform> params: Params;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

// One triangle covering the viewport.
@vertex
fn vs(@builtin(vertex_index) i: u32) -> VsOut {
    let uv = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    var out: VsOut;
    out.pos = vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
    out.uv = uv;
    return out;
}

fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    let lo = c / 12.92;
    let hi = pow((c + 0.055) / 1.055, vec3<f32>(2.4));
    return select(hi, lo, c <= vec3<f32>(0.04045));
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    let y = textureSample(tex_y, samp, in.uv).r;
    let u_sample = textureSample(tex_u, samp, in.uv);
    let v_sample = textureSample(tex_v, samp, in.uv).r;
    let uv = select(vec2<f32>(u_sample.r, v_sample), u_sample.rg, params.mode == 0u);
    let yuv = vec3<f32>(y, uv.x, uv.y);
    var rgb = clamp(vec3<f32>(
        dot(params.row_r.xyz, yuv) + params.row_r.w,
        dot(params.row_g.xyz, yuv) + params.row_g.w,
        dot(params.row_b.xyz, yuv) + params.row_b.w,
    ), vec3<f32>(0.0), vec3<f32>(1.0));
    if params.linearize == 1u {
        rgb = srgb_to_linear(rgb);
    }
    return vec4<f32>(rgb, 1.0);
}
