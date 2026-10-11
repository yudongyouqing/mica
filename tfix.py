import io
p = 'crates/mica-render/src/frame.rs'
s = io.open(p, encoding='utf-8').read()
old = '''    #[test]
    fn tab_width_fits_content_with_clamp() {
        let sm = StripMetrics::default();
        let m = metrics_8x16();
        assert_eq!(tab_width("", &m, &sm), sm.tab_min_w, "空标题 = 最小宽");
        let want = sm.tab_pad * 2.0 + sm.tab_close_w + 3.0 * 8.0;
        assert_eq!(tab_width("abc", &m, &sm), want.max(sm.tab_min_w), "短标题 = max(内容宽, min)");
        assert_eq!(tab_width(&"x".repeat(500), &m, &sm), sm.tab_max_w, "长标题夹最大");
    }'''
new = '''    /// 均分填充契约(gtk-wide-tabs 默认):等宽、随可用宽放大、min 兜底
    #[test]
    fn equal_tab_w_fills_and_clamps() {
        let sm = StripMetrics::default();
        assert_eq!(equal_tab_w(0, 400.0, &sm), sm.tab_min_w, "无标签 = min");
        assert_eq!(equal_tab_w(3, 900.0, &sm), sm.tab_min_w, "极窄可用 = min 兜底(溢出滚动接管)");
        // 宽窗 2 标签:均分(减 + 与边距)
        let k = sm.h / STRIP_H_BASE;
        let body = 900.0 - 6.0 * k - sm.plus_w - 2.0 * k - sm.tab_gap * 2.0;
        assert_eq!(equal_tab_w(2, 900.0, &sm), body / 2.0, "两标签均分");
    }'''
assert s.count(old) == 1
s = s.replace(old, new)
io.open(p, 'w', encoding='utf-8').write(s)
print('ok')
