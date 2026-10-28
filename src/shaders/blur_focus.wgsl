// Blur → Focus wallpaper transition.
//
// The outgoing image racks out of focus while the incoming image resolves
// from heavily blurred to sharp, crossfading in the middle. The blur is a
// golden-angle spiral disc filter with per-pixel rotation dither
// (interleaved gradient noise) so a small tap count stays band-free even at
// large radii.
//
// Everything that is constant across the frame (easing, zoom, cover mapping)
// is evaluated once on the CPU; the fragment stage only samples and mixes.

struct Uniforms {
    // xy: uv scale, zw: uv offset — cover fit and zoom folded together.
    map_a: vec4<f32>,
    map_b: vec4<f32>,
    // Height/width of the target, so blur discs stay round.
    aspect: f32,
    blur_a: f32,
    blur_b: f32,
    fade: f32,
};

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var samp: sampler;
@group(0) @binding(2) var tex_a: texture_2d<f32>;
@group(0) @binding(3) var tex_b: texture_2d<f32>;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vid: u32) -> VsOut {
    let xy = vec2<f32>(f32((vid << 1u) & 2u), f32(vid & 2u)) * 2.0 - 1.0;
    var o: VsOut;
    o.pos = vec4<f32>(xy, 0.0, 1.0);
    o.uv = vec2<f32>((xy.x + 1.0) * 0.5, (1.0 - xy.y) * 0.5);
    return o;
}

fn ign(pix: vec2<f32>) -> f32 {
    return fract(52.9829189 * fract(dot(pix, vec2<f32>(0.06711056, 0.00583715))));
}

const TAPS: i32 = 16;
const GOLDEN_ANGLE: f32 = 2.39996323;
const TAU: f32 = 6.2831853;

fn disc_blur(tex: texture_2d<f32>, uv: vec2<f32>, radius: f32, rot: f32) -> vec3<f32> {
    var acc = textureSampleLevel(tex, samp, uv, 0.0).rgb;
    if (radius < 1e-5) {
        return acc;
    }
    let aspect = vec2<f32>(u.aspect, 1.0);
    for (var i: i32 = 0; i < TAPS; i = i + 1) {
        let fi = f32(i) + 0.5;
        let r = radius * sqrt(fi / f32(TAPS));
        let a = fi * GOLDEN_ANGLE + rot;
        acc = acc + textureSampleLevel(tex, samp, uv + vec2<f32>(cos(a), sin(a)) * r * aspect, 0.0).rgb;
    }
    return acc / f32(TAPS + 1);
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let rot = ign(in.pos.xy) * TAU;
    var col_a = vec3<f32>(0.0);
    var col_b = vec3<f32>(0.0);
    if (u.fade < 0.999) {
        col_a = disc_blur(tex_a, in.uv * u.map_a.xy + u.map_a.zw, u.blur_a, rot);
    }
    if (u.fade > 0.001) {
        col_b = disc_blur(tex_b, in.uv * u.map_b.xy + u.map_b.zw, u.blur_b, rot + GOLDEN_ANGLE);
    }
    return vec4<f32>(mix(col_a, col_b, u.fade), 1.0);
}
