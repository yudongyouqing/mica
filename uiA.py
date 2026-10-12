import io
p = 'crates/mica-render/src/frame.rs'
s = io.open(p, encoding='utf-8').read()
def rep(o, n, cnt=1):
    global s
    assert s.count(o) == cnt, (s.count(o), o[:70])
    s = s.replace(o, n)

rep('/// 超采样倍率(每轴 4 → 16 子样本/像素)',
'''/// strip 比例字距(UI 字体无格宽,墨宽 + 追字距)
pub const STRIP_TRACKING: f32 = 0.7;

/// 超采样倍率(每轴 4 → 16 子样本/像素)''')

rep('''    sm: &StripMetrics,
    extras: StripExtras,
) -> Vec<CellInstance> {
    let StripExtras {''',
'''    sm: &StripMetrics,
    ui: &mut dyn GlyphRouter,
    ui_m: &FontMetrics,
    extras: StripExtras,
) -> Vec<CellInstance> {
    let _ = metrics; // 终端度量退役(strip 文字全走 ui 比例步进)
    let StripExtras {''')

rep('    let text_y = (sm.h - metrics.line_height).max(0.0) / 2.0;',
    '    let text_y = (sm.h - ui_m.line_height).max(0.0) / 2.0;')

rep('''        let max_chars = ((avail / metrics.cell_width) as usize)
            .saturating_sub(2)
            .max(1);
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
        let mut tx = ((avail - text_w) / 2.0).max(0.0);
        for ch in title.chars() {
            let g = ui.route(ch, GlyphStyle::PLAIN);
            if g.size_px[0] <= 0.0 {
                continue;
            }
            out.push(CellInstance {
                pos_uv: [tx + g.offset_px[0], text_y + g.offset_px[1], g.uv[0], g.uv[1]],
                size_uv: [g.size_px[0], g.size_px[1], g.uv[2], g.uv[3]],
                fg: bg_of(white),
                bg: bg_of(bar),
            });
            tx += g.size_px[0] + STRIP_TRACKING;
        }
    } else {''')
io.open(p, 'w', encoding='utf-8').write(s)
print('A ok')
