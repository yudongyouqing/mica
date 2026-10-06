// Instanced quads, one per cell. Instance: 4 × vec4f (64B stride):
//   (0) [x_px, y_px, u, v]  (1) [w_px, h_px, uw, uvh]
//   (2) fg.rgb,_            (3) bg.rgb,_
// 空白格(空格/spacer)uv 尺寸为 0:vs 输出 ink_mask=0,fragment 不采样出墨。
// 圆角扩展(2026-10 UI 对标):无字形 quad 的 fg/bg alpha 槽复用为
//   bg.w = 上圆角半径,fg.w = 下圆角半径(px,0=方角);fg.rgb 同时复用为
//   底衬色——blend:None 下无 alpha 混合,圆角 AA 只能 premix 已知底色。
// 字形 quad 的 fg 是墨色,不得启用圆角(半径恒 0)。
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
    @location(4) local: vec2f,
    @location(5) geo: vec4f,
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
    out.local = corner * size_uv.xy;
    out.geo = vec4f(size_uv.x, size_uv.y, bg.w, fg.w);
    return out;
}

// 圆角矩形 SDF 覆盖率(上下两组半径,取片段所在纵向半区的半径;clamp 防
// 尺寸小于 2r 的退化)。cov 混底衬与面色 —— premix 即无混合 AA。
fn rounded_coverage(local: vec2f, size_px: vec2f, r_top: f32, r_bot: f32) -> f32 {
    let half = size_px * 0.5;
    var r = select(r_bot, r_top, local.y < size_px.y * 0.5);
    r = min(r, min(size_px.x, size_px.y) * 0.5);
    let q = abs(local - half) - (half - vec2f(r));
    let d = length(max(q, vec2f(0.0))) + min(max(q.x, q.y), 0.0) - r;
    return 1.0 - smoothstep(-0.7, 0.7, d);
}

// RGBA 图集(D19):灰度字形 texel = (白, coverage) → rgb*fg 染色、a 做 ink,
// 与 M1 的 R8 单通道行为逐位等价;彩色字形 texel = (原色, 不透明度),渲染
// 实例对彩色字形 fg 填白即得原色——一条公式覆盖两种格式。
@fragment
fn fs(inp: VsOut) -> @location(0) vec4f {
    if (inp.geo.z > 0.0 || inp.geo.w > 0.0) {
        let cov = rounded_coverage(inp.local, inp.geo.xy, inp.geo.z, inp.geo.w);
        return vec4f(mix(inp.fg, inp.bg, cov), 1.0);
    }
    let tex = textureSample(glyphs, glyph_sampler, inp.uv);
    let ink = tex.a * inp.ink_mask;
    return vec4f(mix(inp.bg, tex.rgb * inp.fg, ink), 1.0);
}
