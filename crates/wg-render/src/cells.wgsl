// One instanced quad per cell. Vertex positions derive from vertex_index;
// per-cell data is the 48-byte instance (frame::CellInstance):
// location(0) = [x_px, y_px, glyph_index, 0], (1) = fg.rgb, (2) = bg.rgb.
const CORNERS = array<vec2f, 6>(
    vec2f(0.0, 0.0), vec2f(1.0, 0.0), vec2f(0.0, 1.0),
    vec2f(0.0, 1.0), vec2f(1.0, 0.0), vec2f(1.0, 1.0),
);

struct Globals {
    viewport: vec2f, // client size in px
    cell: vec2f,     // cell size in px (8, 16)
    atlas: vec2f,    // atlas size in px
    pad: vec2f,
};

@group(0) @binding(0) var<uniform> globals: Globals;
@group(0) @binding(1) var glyphs: texture_2d<f32>;
@group(0) @binding(2) var glyph_sampler: sampler;

struct VsOut {
    @builtin(position) pos: vec4f,
    @location(0) uv: vec2f,
    @location(1) fg: vec3f,
    @location(2) bg: vec3f,
};

@vertex
fn vs(
    @builtin(vertex_index) vi: u32,
    @location(0) pos_glyph: vec4f,
    @location(1) fg: vec4f,
    @location(2) bg: vec4f,
) -> VsOut {
    let corner = CORNERS[vi];
    let x = pos_glyph.x + corner.x * globals.cell.x;
    let y = pos_glyph.y + corner.y * globals.cell.y;
    var out: VsOut;
    out.pos = vec4f(
        x / globals.viewport.x * 2.0 - 1.0,
        1.0 - y / globals.viewport.y * 2.0,
        0.0,
        1.0,
    );
    out.uv = vec2f(
        (f32(u32(pos_glyph.z)) + corner.x) * globals.cell.x / globals.atlas.x,
        corner.y * globals.cell.y / globals.atlas.y,
    );
    out.fg = fg.rgb;
    out.bg = bg.rgb;
    return out;
}

@fragment
fn fs(inp: VsOut) -> @location(0) vec4f {
    let ink = textureSample(glyphs, glyph_sampler, inp.uv).r;
    return vec4f(mix(inp.bg, inp.fg, ink), 1.0);
}
