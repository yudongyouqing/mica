import io
p = 'crates/mica-render/src/frame.rs'
s = io.open(p, encoding='utf-8').read()
def rep(o, n, cnt=1):
    global s
    assert s.count(o) == cnt, (s.count(o), o[:60])
    s = s.replace(o, n)

rep('''pub fn strip_quads(
    titles: &[(String, bool)],
    router: &mut dyn GlyphRouter,
    metrics: &FontMetrics,
    palette: &Palette,
    sm: &StripMetrics,
    extras: StripExtras,
) -> Vec<CellInstance> {''',
'''pub fn strip_quads(
    titles: &[(String, bool)],
    router: &mut dyn GlyphRouter,
    metrics: &FontMetrics,
    palette: &Palette,
    sm: &StripMetrics,
    ui: &mut dyn GlyphRouter,
    ui_m: &FontMetrics,
    extras: StripExtras,
) -> Vec<CellInstance> {
    let _ = metrics; // 终端度量仅余留兼容(等宽 UI 退役;文字全走 ui)''')

s = s.replace('let text_y = (sm.h - metrics.line_height).max(0.0) / 2.0;',
              'let text_y = (sm.h - ui_m.line_height).max(0.0) / 2.0;')

rep('''        let max_chars = ((avail / metrics.cell_width) as usize).saturating_sub(2).max(1);
        let text_w = title.chars().count().min(max_chars) as f32 * metrics.cell_width;
        let start = ((avail - text_w) / 2.0).max(0.0);
        for (ci, ch) in title.chars().take(max_chars).enumerate() {
            let g = router.route(ch, GlyphStyle::PLAIN);
            if g.size_px[0] <= 0.0 {
                continue;
            }
            out.push(CellInstance {
                pos_uv: [
                    start + ci as f32 * metrics.cell_width + g.offset_px[0],
                    text_y + g.offset_px[1],
                    g.uv[0],
                    g.uv[1],
                ],
                size_uv: [g.size_px[0], g.size_px[1], g.uv[2], g.uv[3]],
                fg: bg_of(white),
                bg: bg_of(bar),
            });
        }
    } else {''',
'''        let mut text_w = 0.0;
        for ch in title.chars() {
            let g = ui.route(ch, GlyphStyle::PLAIN);
            text_w += g.size_px[0] + STRIP_TRACKING;
        }
        let mut start = ((avail - text_w) / 2.0).max(0.0);
        for ch in title.chars() {
            let g = ui.route(ch, GlyphStyle::PLAIN);
            if g.size_px[0] <= 0.0 {
                continue;
            }
            out.push(CellInstance {
                pos_uv: [
                    start + g.offset_px[0],
                    text_y + g.offset_px[1],
                    g.uv[0],
                    g.uv[1],
                ],
                size_uv: [g.size_px[0], g.size_px[1], g.uv[2], g.uv[3]],
                fg: bg_of(white),
                bg: bg_of(bar),
            });
            start += g.size_px[0] + STRIP_TRACKING;
        }
    } else {''')
io.open(p, 'w', encoding='utf-8').write(s)
print('ok')
