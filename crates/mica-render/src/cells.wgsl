// Instanced quads, one per cell. Instance: 4 × vec4f (64B stride):
//   (0) [x_px, y_px, u, v]  (1) [w_px, h_px, uw, uvh]
//   (2) fg.rgb,_            (3) bg.rgb,_
// 空白格(空格/spacer)uv 尺寸为 0:vs 输出 ink_mask=0,fragment 不采样出墨。
const CORNERS = array<vec2f, 6>(
    vec2f(0.0, 0.0), vec2f(1.0, 0.0), vec2f(0.0, 1.0),
    vec2f(0.0, 1.0), vec2f(1.0, 0.0), vec2f(1.0, 1.0),
);

struct Globals {
    viewport: vec2f,
    _pad: vec2f,
};
@group(0) @binding(0) var<uniform> globals: Globals;
@group(0) @binding(1) var glyphs: texture_2d<f32>;
@group(0) @binding(2) var glyph_sampler: sampler;

struct VsOut {
    @builtin(position) pos: vec4f,
    @location(0) uv: vec2f,
    @location(1) fg: vec3f,
    @location(2) bg: vec3f,
    @location(3) ink_mask: f32,
};

@vertex
fn vs(
    @builtin(vertex_index) vi: u32,
    @location(0) pos_uv: vec4f,
    @location(1) size_uv: vec4f,
    @location(2) fg: vec4f,
    @location(3) bg: vec4f,
) -> VsOut {
    let corner = CORNERS[vi];
    let x = pos_uv.x + corner.x * size_uv.x;
    let y = pos_uv.y + corner.y * size_uv.y;
    var out: VsOut;
    out.pos = vec4f(x / globals.viewport.x * 2.0 - 1.0, 1.0 - y / globals.viewport.y * 2.0, 0.0, 1.0);
    out.uv = pos_uv.zw + corner * size_uv.zw;
    out.fg = fg.rgb;
    out.bg = bg.rgb;
    out.ink_mask = select(0.0, 1.0, size_uv.z > 0.0);
    return out;
}

@fragment
fn fs(inp: VsOut) -> @location(0) vec4f {
    let ink = textureSample(glyphs, glyph_sampler, inp.uv).r * inp.ink_mask;
    return vec4f(mix(inp.bg, inp.fg, ink), 1.0);
}
