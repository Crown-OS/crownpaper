// Blur → Focus wallpaper transition.
//
// The outgoing image racks out of focus while the incoming image resolves
// from heavily blurred to sharp, crossfading in the middle. A subtle scale
// pull on both layers sells the "focus rack" feel. The blur is a golden-angle
// spiral disc filter with per-pixel rotation dither (interleaved gradient
// noise) so a small tap count stays band-free even at large radii.

struct Uniforms {
    resolution: vec2<f32>,
    progress: f32,
    time: f32,
    tex_a_size: vec2<f32>,
    tex_b_size: vec2<f32>,
    intro_scale: f32,
    max_blur: f32,
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
    var p = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>( 3.0, -1.0),
        vec2<f32>(-1.0,  3.0),
    );
    let xy = p[vid];
    var o: VsOut;
    o.pos = vec4<f32>(xy, 0.0, 1.0);
    o.uv = vec2<f32>((xy.x + 1.0) * 0.5, 1.0 - (xy.y + 1.0) * 0.5);
    return o;
}

// Evaluate a CSS cubic-bezier(x1, y1, x2, y2) curve at input x in [0,1].
// P0 = (0,0), P3 = (1,1) are implicit. Coefficients come from the monomial
// expansion of the bezier polynomial; Newton-Raphson inverts B_x(t) = x.
fn cubic_bezier(x: f32, x1: f32, y1: f32, x2: f32, y2: f32) -> f32 {
    let ax: f32 = 3.0 * x1;
    let bx: f32 = 3.0 * x2 - 6.0 * x1;
    let cx: f32 = 3.0 * x1 - 3.0 * x2 + 1.0;
    let ay: f32 = 3.0 * y1;
    let by: f32 = 3.0 * y2 - 6.0 * y1;
    let cy: f32 = 3.0 * y1 - 3.0 * y2 + 1.0;

    var t: f32 = clamp(x, 0.0, 1.0);
    for (var i: i32 = 0; i < 8; i = i + 1) {
        let bxt = t * (ax + t * (bx + t * cx));
        let bxd = max(ax + t * (2.0 * bx + 3.0 * cx * t), 1e-6);
        let delta = (bxt - x) / bxd;
        t = t - delta;
        if (abs(delta) < 1e-6) {
            break;
        }
    }
    t = clamp(t, 0.0, 1.0);
    return t * (ay + t * (by + t * cy));
}

// Standard "ease" material curve — gentle in, decisive settle.
fn ease(x: f32) -> f32 {
    return cubic_bezier(x, 0.4, 0.0, 0.2, 1.0);
}

// Fit texture to screen preserving aspect ratio (cover), with an extra
// centered zoom for the focus-pull scale.
fn cover_uv(uv: vec2<f32>, tex_size: vec2<f32>, screen: vec2<f32>, zoom: f32) -> vec2<f32> {
    let centered = (uv - vec2<f32>(0.5)) / max(zoom, 1e-4) + vec2<f32>(0.5);
    let sr = screen.x / max(screen.y, 1.0);
    let tr = tex_size.x / max(tex_size.y, 1.0);
    var scale = vec2<f32>(1.0, 1.0);
    if (tr > sr) {
        scale.x = sr / tr;
    } else {
        scale.y = tr / sr;
    }
    let offset = (1.0 - scale) * 0.5;
    return centered * scale + offset;
}

// Interleaved gradient noise — cheap per-pixel value in [0,1) used to rotate
// the sampling spiral so disc-blur banding turns into imperceptible grain.
fn ign(pix: vec2<f32>) -> f32 {
    return fract(52.9829189 * fract(dot(pix, vec2<f32>(0.06711056, 0.00583715))));
}

const TAPS: i32 = 16;
const GOLDEN_ANGLE: f32 = 2.39996323;

// Disc blur: golden-angle spiral over TAPS samples, radius in UV units.
// sqrt-distributed radii give uniform disc coverage (bokeh-like softness).
fn disc_blur(tex: texture_2d<f32>, uv: vec2<f32>, radius: f32, rot: f32) -> vec3<f32> {
    if (radius < 1e-5) {
        return textureSampleLevel(tex, samp, uv, 0.0).rgb;
    }
    let aspect = vec2<f32>(u.resolution.y / max(u.resolution.x, 1.0), 1.0);
    var acc = textureSampleLevel(tex, samp, uv, 0.0).rgb;
    for (var i: i32 = 0; i < TAPS; i = i + 1) {
        let fi = f32(i) + 0.5;
        let r = radius * sqrt(fi / f32(TAPS));
        let a = fi * GOLDEN_ANGLE + rot;
        let offset = vec2<f32>(cos(a), sin(a)) * r * aspect;
        acc = acc + textureSampleLevel(tex, samp, uv + offset, 0.0).rgb;
    }
    return acc / f32(TAPS + 1);
}

fn transition_color(uv: vec2<f32>, pix: vec2<f32>) -> vec3<f32> {
    let t = clamp(u.progress, 0.0, 1.0);
    let te = ease(t);

    // Outgoing defocuses over the first ~70%; incoming resolves over the
    // last ~70%. The overlap keeps the midpoint fully soft — no hard frame.
    let blur_out = u.max_blur * ease(clamp(t / 0.7, 0.0, 1.0));
    let blur_in = u.max_blur * (1.0 - ease(clamp((t - 0.3) / 0.7, 0.0, 1.0)));

    // Crossfade weighted toward the middle so the sharp→sharp endpoints hold.
    let fade = cubic_bezier(te, 0.3, 0.0, 0.7, 1.0);

    // Subtle focus-pull zoom: outgoing drifts forward, incoming settles back.
    let zoom_a = 1.0 + 0.04 * te;
    let zoom_b = 1.04 - 0.04 * te;

    let rot = ign(pix) * 6.2831853;

    let uv_a = cover_uv(uv, u.tex_a_size, u.resolution, zoom_a);
    let uv_b = cover_uv(uv, u.tex_b_size, u.resolution, zoom_b);

    // Skip whichever layer is fully faded — halves the tap cost at the ends.
    var col_a = vec3<f32>(0.0);
    var col_b = vec3<f32>(0.0);
    if (fade < 0.999) {
        col_a = disc_blur(tex_a, uv_a, blur_out, rot);
    }
    if (fade > 0.001) {
        col_b = disc_blur(tex_b, uv_b, blur_in, rot + 2.399);
    }
    return mix(col_a, col_b, fade);
}

fn shade(in: VsOut) -> vec3<f32> {
    // Intro letterbox: scale the whole output around center.
    // intro_scale == 1.0 is a no-op (local == uv).
    let centered = in.uv - vec2<f32>(0.5, 0.5);
    let local = centered / max(u.intro_scale, 1e-4) + vec2<f32>(0.5, 0.5);
    if (local.x < 0.0 || local.x > 1.0 || local.y < 0.0 || local.y > 1.0) {
        return vec3<f32>(0.0, 0.0, 0.0);
    }
    return transition_color(local, in.pos.xy);
}

// For sRGB surface formats: the hardware encodes on store.
@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return vec4<f32>(shade(in), 1.0);
}

fn srgb_encode(c: vec3<f32>) -> vec3<f32> {
    let lo = c * 12.92;
    let hi = 1.055 * pow(max(c, vec3<f32>(0.0)), vec3<f32>(1.0 / 2.4)) - 0.055;
    return select(hi, lo, c <= vec3<f32>(0.0031308));
}

// For non-sRGB surface formats (what crownshell configures): encode the
// linear result manually so the compositor still receives sRGB bytes.
@fragment
fn fs_main_srgb_encode(in: VsOut) -> @location(0) vec4<f32> {
    return vec4<f32>(srgb_encode(shade(in)), 1.0);
}
