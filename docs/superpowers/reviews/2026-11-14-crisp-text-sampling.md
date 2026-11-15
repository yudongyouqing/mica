# Text sampling and restrained chrome

User feedback identified fuzzy typography and an excessively prominent title
badge. Investigation found a real sampling defect, not only a preview-size issue.

## Sampling contract

Reference: [Microsoft pixel/texel coordinate systems](https://learn.microsoft.com/en-us/windows/win32/direct3d10/d3d10-graphics-programming-guide-resources-coordinates),
checked 2026-11-14. A screen-aligned texture maps full texel edges to geometry;
rasterization evaluates pixel centers. The old vertex shader instead mapped the
first/last texel centers to the quad edges, compressing the UV domain by one
texel and causing linear resampling throughout every glyph.

A GPU readback regression reproduced an opaque 1-pixel white column as **143**
instead of 255. Full-domain UV mapping now reproduces alternating opaque/clear
columns exactly. Fragment-side clamping to first/last texel centers retains
atlas-gutter protection at fractional/scaled sprite edges without shrinking the
interior. A 16-tile, 7.75-pixel solid-sprite test verifies no dark seams. Flat
per-instance UV limits cover single-texel and atlas-edge cases; RGBA/color-glyph
and rounded-quad paths preserve their existing formulas.

UI bitmap origins are snapped to physical pixels after proportional layout;
the unrounded pen is retained so advance rounding cannot accumulate. The old
UI regression exposed x.fract() = 0.5. Terminal grid coordinates are unchanged.

## Appearance

Keep the readable 11pt/600 UI font, replace the strong automatic accent badge
with a low-contrast text-color mix (0.06, radius 4), and reduce focused/inactive
bar wash to 0.08/0.12. Explicit user highlight colors remain supported. This
removes the default purple sticker appearance without claiming a universal
industry aesthetic or another terminal's exact style.

The actual DirectWrite/wgpu exporter now supports `--single`; a 1000x80 native
pixel preview was inspected at source resolution. No foreground desktop window
or game input is needed. Independent shader review found no blocking issues.
Targeted sampling and UI alignment tests were observed red before the fixes,
then green; final workspace/clippy/build results are recorded in the PR.
