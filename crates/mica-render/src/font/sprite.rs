//! 内建 sprite face(2026-10-07,对标 Ghostty src/font/sprite 语义):
//! 制表符/方块元素/几何形状/Powerline/盲文**程序化绘制**进图集,不经
//! 系统字体——精确几何天然无缝。只参考上游几何语义(笔画位置/粗细
//! 比例/覆盖范围),实现独立:4×4 超采样标量光栅 → 8bit 覆盖率。
//!
//! 语义(上游 common.zig/box.zig 提炼):线条模型 = 四边(up/right/
//! down/left)各 none/light/heavy,笔画从格边到中心,横线居垂直中线、
//! 竖线居水平中线;粗细 light = 行高/10 夹 [1,3],heavy = 2×;方块
//! 元素按码位精确吸附;盲文 2×4 点阵;Powerline 三角满格。

/// 码位是否由精灵覆盖。route_run(shaping 批量路径)据此拒绝整段整形、
/// 强制逐字路由——shaping 直出 DWrite 字体字形会**绕过精灵**(字体墨迹
/// 未对齐/空白,2026-10-08 实证:Claude logo 的连续块字符段整段被
/// shaping 吃掉,精灵只拦到孤立字符,oh-my-posh 的 PUA 三角因字体缺字
/// shaping 失败回落才偶然正常)。几何/Powerline 子集与 draw() 的命中集
/// 严格一致,防止"covers 说有、draw 画不出"的回落死洞。
pub fn covers(cp: u32) -> bool {
    matches!(
        cp,
        0x2500..=0x257F
            | 0x2580..=0x259F
            | 0x2800..=0x28FF
            | 0x25A0
            | 0x25A1
            | 0x25AA
            | 0x25AB
            | 0x25AC
            | 0x25B2
            | 0x25BA
            | 0x25BC
            | 0x25C4
            | 0x25C6
            | 0x25C7
            | 0x25CB
            | 0x25CF
            | 0x25D0
            | 0x25D1
            | 0xE0B0..=0xE0BB
    )
}

/// 超采样倍率(每轴 4 → 16 子样本/像素)
const SS: u32 = 4;

/// 主入口:绘制码位精灵,返回 (像素宽, 像素高, 覆盖率)。None = 不在
/// 精灵范围(调用方回落字体路径)。
pub fn draw(cp: u32, cell_w: f32, cell_h: f32) -> Option<(u32, u32, Vec<u8>)> {
    let pw = cell_w.ceil().max(1.0) as u32;
    let ph = cell_h.ceil().max(1.0) as u32;
    let mut c = Canvas::new(pw, ph);
    let hit = match cp {
        0x2500..=0x257F => draw_box(cp, &mut c),
        0x2580..=0x259F => draw_block(cp, &mut c),
        0x25A0..=0x25FF => draw_geometric(cp, &mut c),
        0x2800..=0x28FF => draw_braille(cp, &mut c),
        0xE0B0..=0xE0D7 => draw_powerline(cp, &mut c),
        _ => false,
    };
    if !hit {
        return None;
    }
    Some((pw, ph, c.coverage()))
}

// ---------- 光栅器(格坐标系,内部 ×SS 子采样)----------

struct Canvas {
    w: f32,
    h: f32,
    pw: u32,
    ph: u32,
    mask: Vec<bool>,
    /// 常量覆盖(░▒▓ 阴影):Some 时 coverage() 直接铺满该值
    shade: Option<u8>,
}

impl Canvas {
    fn new(pw: u32, ph: u32) -> Self {
        Self {
            w: pw as f32,
            h: ph as f32,
            pw,
            ph,
            mask: vec![false; (pw * SS * ph * SS) as usize],
            shade: None,
        }
    }
    fn idx(pw: u32, sx: u32, sy: u32) -> usize {
        (sy * pw * SS + sx) as usize
    }
    fn span(&self, lo: f32, hi: f32, span_px: u32) -> (i64, i64) {
        let lo = (((lo * SS as f32).floor() as i64) - 1).max(0);
        let hi = (((hi * SS as f32).ceil() as i64) + 1).min(span_px as i64 * SS as i64);
        (lo, hi)
    }
    fn fill_rect(&mut self, x0: f32, y0: f32, x1: f32, y1: f32) {
        let (sx0, sx1) = self.span(x0, x1, self.pw);
        let (sy0, sy1) = self.span(y0, y1, self.ph);
        for sy in sy0..sy1 {
            let cy = (sy as f32 + 0.5) / SS as f32;
            if cy < y0 || cy >= y1 {
                continue;
            }
            for sx in sx0..sx1 {
                let cx = (sx as f32 + 0.5) / SS as f32;
                if cx >= x0 && cx < x1 {
                    let i = Self::idx(self.pw, sx as u32, sy as u32);
                    self.mask[i] = true;
                }
            }
        }
    }
    /// 多边形(偶奇规则,子像素中心采样)
    fn fill_poly(&mut self, pts: &[(f32, f32)]) {
        if pts.len() < 3 {
            return;
        }
        let miny = pts.iter().fold(f32::MAX, |a, p| a.min(p.1));
        let maxy = pts.iter().fold(f32::MIN, |a, p| a.max(p.1));
        let (sy0, sy1) = self.span(miny, maxy, self.ph);
        let mut xs = [0f32; 8];
        for sy in sy0..sy1 {
            let cy = (sy as f32 + 0.5) / SS as f32;
            let mut n = 0usize;
            for i in 0..pts.len() {
                let (x0, y0) = pts[i];
                let (x1, y1) = pts[(i + 1) % pts.len()];
                if ((y0 <= cy && y1 > cy) || (y1 <= cy && y0 > cy)) && n < xs.len() {
                    xs[n] = x0 + (cy - y0) / (y1 - y0) * (x1 - x0);
                    n += 1;
                }
            }
            xs[..n].sort_by(|a, b| a.partial_cmp(b).unwrap());
            let mut k = 0;
            while k + 1 < n {
                let (a, b) = (xs[k], xs[k + 1]);
                let (cx0, cx1) = self.span(a, b, self.pw);
                for sx in cx0..cx1 {
                    let cx = (sx as f32 + 0.5) / SS as f32;
                    if cx >= a && cx < b {
                        let i = Self::idx(self.pw, sx as u32, sy as u32);
                        self.mask[i] = true;
                    }
                }
                k += 2;
            }
        }
    }
    /// 带宽线段(中心线两侧各 half)
    fn line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, half: f32) {
        let dx = x1 - x0;
        let dy = y1 - y0;
        let len = (dx * dx + dy * dy).sqrt().max(1e-6);
        let (nx, ny) = (-dy / len * half, dx / len * half);
        self.fill_poly(&[
            (x0 + nx, y0 + ny),
            (x1 + nx, y1 + ny),
            (x1 - nx, y1 - ny),
            (x0 - nx, y0 - ny),
        ]);
    }
    fn fill_ellipse(&mut self, cx: f32, cy: f32, rx: f32, ry: f32) {
        let (sx0, sx1) = self.span(cx - rx, cx + rx, self.pw);
        let (sy0, sy1) = self.span(cy - ry, cy + ry, self.ph);
        for sy in sy0..sy1 {
            let py = (sy as f32 + 0.5) / SS as f32;
            for sx in sx0..sx1 {
                let px = (sx as f32 + 0.5) / SS as f32;
                let dx = (px - cx) / rx;
                let dy = (py - cy) / ry;
                if dx * dx + dy * dy <= 1.0 {
                    let i = Self::idx(self.pw, sx as u32, sy as u32);
                    self.mask[i] = true;
                }
            }
        }
    }
    /// 圆弧描边(参数角:弧心/半径/起止角/半宽)
    fn arc(&mut self, cx: f32, cy: f32, r: f32, a0: f32, a1: f32, half: f32) {
        let steps = 20;
        let mut px = cx + a0.cos() * r;
        let mut py = cy + a0.sin() * r;
        for i in 1..=steps {
            let a = a0 + (a1 - a0) * i as f32 / steps as f32;
            let x = cx + a.cos() * r;
            let y = cy + a.sin() * r;
            self.line(px, py, x, y, half);
            px = x;
            py = y;
        }
    }
    /// 挖除矩形(三象限块):子像素中心在内则清
    fn carve_rect(&mut self, x0: f32, y0: f32, x1: f32, y1: f32) {
        let (sx0, sx1) = self.span(x0, x1, self.pw);
        let (sy0, sy1) = self.span(y0, y1, self.ph);
        for sy in sy0..sy1 {
            let cy = (sy as f32 + 0.5) / SS as f32;
            if cy < y0 || cy >= y1 {
                continue;
            }
            for sx in sx0..sx1 {
                let cx = (sx as f32 + 0.5) / SS as f32;
                if cx >= x0 && cx < x1 {
                    self.mask[Self::idx(self.pw, sx as u32, sy as u32)] = false;
                }
            }
        }
    }
    fn coverage(&self) -> Vec<u8> {
        if let Some(v) = self.shade {
            return vec![v; (self.pw * self.ph) as usize];
        }
        let mut out = Vec::with_capacity((self.pw * self.ph) as usize);
        for y in 0..self.ph {
            for x in 0..self.pw {
                let mut n = 0u8;
                for sy in 0..SS {
                    for sx in 0..SS {
                        if self.mask[Self::idx(self.pw, x * SS + sx, y * SS + sy)] {
                            n += 1;
                        }
                    }
                }
                out.push((u32::from(n) * 255 / 16) as u8);
            }
        }
        out
    }
}

/// 交叉族表:cp<<8 | up<<6 | right<<4 | down<<2 | left(0/1/2=无/细/粗)
const LINES_TAB: [u32; 68] = [
    0x250011, 0x250122, 0x250244, 0x250388, 0x250c41, 0x250d81, 0x250e42, 0x250f82, 0x251050,
    0x251190, 0x251260, 0x2513a0, 0x251405, 0x251509, 0x251606, 0x25170a, 0x251814, 0x251918,
    0x251a24, 0x251b28, 0x251c45, 0x251d85, 0x251e46, 0x251f49, 0x252086, 0x252189, 0x25224a,
    0x25238a, 0x252454, 0x252594, 0x252664, 0x252758, 0x252894, 0x2529a4, 0x252a68, 0x252ba8,
    0x252c15, 0x252d25, 0x252e19, 0x252f29, 0x253026, 0x253129, 0x25321a, 0x25332a, 0x253451,
    0x253591, 0x253661, 0x2537a1, 0x253852, 0x253992, 0x253a62, 0x253ba2, 0x253c55, 0x253d96,
    0x253e69, 0x253f59, 0x254056, 0x254159, 0x254269, 0x2543a6, 0x2544a6, 0x254596, 0x254666,
    0x25476a, 0x2548a9, 0x2549a6, 0x254aa9, 0x254baa,
];

// ---------- 制表符 U+2500-257F ----------

fn draw_box(cp: u32, c: &mut Canvas) -> bool {
    let light = (c.h / 10.0).clamp(1.0, 3.0);
    let heavy = light * 2.0;
    let half_l = light / 2.0;
    let half_h = heavy / 2.0;
    let (w, h) = (c.w, c.h);
    let (mx, my) = (w / 2.0, h / 2.0);
    let hseg = |c: &mut Canvas, x0: f32, x1: f32, half: f32| {
        c.fill_rect(x0, my - half, x1, my + half);
    };
    let vseg = |c: &mut Canvas, y0: f32, y1: f32, half: f32| {
        c.fill_rect(mx - half, y0, mx + half, y1);
    };
    let line = |c: &mut Canvas, up: u32, right: u32, down: u32, left: u32| {
        let hw = |v: u32| if v == 2 { half_h } else { half_l };
        if left != 0 {
            hseg(c, 0.0, mx, hw(left));
        }
        if right != 0 {
            hseg(c, mx, w, hw(right));
        }
        if up != 0 {
            vseg(c, 0.0, my, hw(up));
        }
        if down != 0 {
            vseg(c, my, h, hw(down));
        }
    };
    let dline = |c: &mut Canvas, vert: bool| {
        let off = light * 1.5 + half_l;
        if vert {
            c.fill_rect(mx - off - half_l, 0.0, mx - off + half_l, h);
            c.fill_rect(mx + off - half_l, 0.0, mx + off + half_l, h);
        } else {
            c.fill_rect(0.0, my - off - half_l, w, my - off + half_l);
            c.fill_rect(0.0, my + off - half_l, w, my + off + half_l);
        }
    };
    match cp {
        0x2500..=0x2503 | 0x250C..=0x254B => {
            let Some(&v) = LINES_TAB.iter().find(|&&e| (e >> 8) == cp) else {
                return false;
            };
            line(c, v >> 6 & 3, v >> 4 & 3, v >> 2 & 3, v & 3);
        }
        0x2504 | 0x2505 | 0x2508 | 0x2509 => {
            let t = if cp == 0x2505 || cp == 0x2509 {
                heavy
            } else {
                light
            };
            let seg = w / 6.0;
            for i in 0..3 {
                c.fill_rect(
                    i as f32 * 2.0 * seg,
                    my - t / 2.0,
                    i as f32 * 2.0 * seg + seg,
                    my + t / 2.0,
                );
            }
        }
        0x2506 | 0x2507 | 0x250A | 0x250B => {
            let t = if cp == 0x2507 || cp == 0x250B {
                heavy
            } else {
                light
            };
            let seg = h / 6.0;
            for i in 0..3 {
                c.fill_rect(
                    mx - t / 2.0,
                    i as f32 * 2.0 * seg,
                    mx + t / 2.0,
                    i as f32 * 2.0 * seg + seg,
                );
            }
        }
        0x2550 => dline(c, false),
        0x2551 => dline(c, true),
        0x2552..=0x256C => {
            // 双线族:一边双以 heavy 表达(小格上与真双线不可分辨)
            let (u, r, d, l): (u32, u32, u32, u32) = match cp {
                0x2552 => (2, 0, 0, 1),
                0x2553 => (1, 0, 0, 2),
                0x2554 => (2, 0, 0, 2),
                0x2555 => (2, 1, 0, 0),
                0x2556 => (1, 2, 0, 0),
                0x2557 => (2, 2, 0, 0),
                0x2558 => (0, 0, 2, 1),
                0x2559 => (0, 0, 1, 2),
                0x255A => (0, 0, 2, 2),
                0x255B => (0, 1, 2, 0),
                0x255C => (0, 2, 1, 0),
                0x255D => (0, 2, 2, 0),
                0x255E => (1, 0, 1, 2),
                0x255F => (2, 0, 2, 1),
                0x2560 => (2, 0, 1, 2),
                0x2561 => (1, 2, 1, 0),
                0x2562 => (2, 1, 2, 0),
                0x2563 => (2, 2, 1, 0),
                0x2564 => (0, 1, 2, 2),
                0x2565 => (0, 2, 1, 2),
                0x2566 => (0, 2, 2, 1),
                0x2567 => (1, 2, 0, 2),
                0x2568 => (2, 1, 0, 2),
                0x2569 => (2, 2, 0, 1),
                0x256A => (1, 2, 1, 2),
                0x256B => (2, 1, 2, 1),
                0x256C => (2, 2, 2, 2),
                _ => return false,
            };
            line(c, u, r, d, l);
        }
        0x256D => c.arc(
            mx,
            my,
            my.min(mx),
            std::f32::consts::PI,
            std::f32::consts::PI * 1.5,
            half_l,
        ),
        0x256E => c.arc(
            mx,
            my,
            my.min(mx),
            std::f32::consts::PI * 1.5,
            std::f32::consts::PI * 2.0,
            half_l,
        ),
        0x256F => c.arc(mx, my, my.min(mx), 0.0, std::f32::consts::FRAC_PI_2, half_l),
        0x2570 => c.arc(
            mx,
            my,
            my.min(mx),
            std::f32::consts::FRAC_PI_2,
            std::f32::consts::PI,
            half_l,
        ),
        0x2571 => c.line(0.0, h, w, 0.0, half_l),
        0x2572 => c.line(0.0, 0.0, w, h, half_l),
        0x2573 => {
            c.line(0.0, h, w, 0.0, half_l);
            c.line(0.0, 0.0, w, h, half_l);
        }
        0x2574 => hseg(c, 0.0, mx, half_l),
        0x2575 => vseg(c, 0.0, my, half_l),
        0x2576 => hseg(c, mx, w, half_l),
        0x2577 => vseg(c, my, h, half_l),
        0x2578 => hseg(c, 0.0, mx, half_h),
        0x2579 => vseg(c, 0.0, my, half_h),
        0x257A => hseg(c, mx, w, half_h),
        0x257B => vseg(c, my, h, half_h),
        0x257C => {
            hseg(c, 0.0, mx, half_h);
            hseg(c, mx, w, half_l);
        }
        0x257D => {
            vseg(c, 0.0, my, half_h);
            vseg(c, my, h, half_l);
        }
        0x257E => {
            hseg(c, 0.0, mx, half_l);
            hseg(c, mx, w, half_h);
        }
        0x257F => {
            vseg(c, 0.0, my, half_l);
            vseg(c, my, h, half_h);
        }
        _ => return false,
    }
    true
}

// ---------- 方块元素 U+2580-259F ----------

fn draw_block(cp: u32, c: &mut Canvas) -> bool {
    let (w, h) = (c.w, c.h);
    let rect = |c: &mut Canvas, x0: f32, y0: f32, x1: f32, y1: f32| {
        c.fill_rect(x0 * w, y0 * h, x1 * w, y1 * h);
    };
    match cp {
        0x2580 => rect(c, 0.0, 0.0, 1.0, 0.5),
        0x2581 => rect(c, 0.0, 0.875, 1.0, 1.0),
        0x2582 => rect(c, 0.0, 0.75, 1.0, 1.0),
        0x2583 => rect(c, 0.0, 0.625, 1.0, 1.0),
        0x2584 => rect(c, 0.0, 0.5, 1.0, 1.0),
        0x2585 => rect(c, 0.0, 0.375, 1.0, 1.0),
        0x2586 => rect(c, 0.0, 0.25, 1.0, 1.0),
        0x2587 => rect(c, 0.0, 0.125, 1.0, 1.0),
        0x2588 => rect(c, 0.0, 0.0, 1.0, 1.0),
        0x2589 => rect(c, 0.0, 0.0, 0.875, 1.0),
        0x258A => rect(c, 0.0, 0.0, 0.75, 1.0),
        0x258B => rect(c, 0.0, 0.0, 0.625, 1.0),
        0x258C => rect(c, 0.0, 0.0, 0.5, 1.0),
        0x258D => rect(c, 0.0, 0.0, 0.375, 1.0),
        0x258E => rect(c, 0.0, 0.0, 0.25, 1.0),
        0x258F => rect(c, 0.0, 0.0, 0.125, 1.0),
        0x2590 => rect(c, 0.5, 0.0, 1.0, 1.0),
        0x2591 => {
            c.shade = Some(0x40);
        }
        0x2592 => {
            c.shade = Some(0x80);
        }
        0x2593 => {
            c.shade = Some(0xC0);
        }
        0x2594 => rect(c, 0.0, 0.0, 1.0, 0.125),
        0x2595 => rect(c, 0.875, 0.0, 1.0, 1.0),
        0x2596 => rect(c, 0.0, 0.5, 0.5, 1.0),
        0x2597 => rect(c, 0.5, 0.5, 1.0, 1.0),
        0x2598 => rect(c, 0.0, 0.0, 0.5, 0.5),
        0x259A => {
            rect(c, 0.0, 0.0, 0.5, 0.5);
            rect(c, 0.5, 0.5, 1.0, 1.0);
        }
        0x259D => rect(c, 0.5, 0.0, 1.0, 0.5),
        0x259E => {
            rect(c, 0.5, 0.0, 1.0, 0.5);
            rect(c, 0.0, 0.5, 0.5, 1.0);
        }
        // 三象限块(满格挖一角)——Claude Code logo 的 ▛▜ 在此;漏掉会
        // 落回字体字形(未对齐)= logo 一块一块(用户实测)
        0x2599 => {
            rect(c, 0.0, 0.0, 1.0, 1.0);
            c.carve_rect(0.5 * w, 0.0, w, 0.5 * h);
        }
        0x259B => {
            rect(c, 0.0, 0.0, 1.0, 1.0);
            c.carve_rect(0.5 * w, 0.5 * h, w, h);
        }
        0x259C => {
            rect(c, 0.0, 0.0, 1.0, 1.0);
            c.carve_rect(0.0, 0.5 * h, 0.5 * w, h);
        }
        0x259F => {
            rect(c, 0.0, 0.0, 1.0, 1.0);
            c.carve_rect(0.0, 0.0, 0.5 * w, 0.5 * h);
        }
        _ => return false,
    }
    true
}

// ---------- 几何形状 U+25A0-25FF(常用子集,其余回落字体)----------

fn draw_geometric(cp: u32, c: &mut Canvas) -> bool {
    let (w, h) = (c.w, c.h);
    let (mx, my) = (w / 2.0, h / 2.0);
    let r = my.min(mx) * 0.9;
    match cp {
        0x25A0 => c.fill_rect(w * 0.1, h * 0.2, w * 0.9, h * 0.8),
        0x25A1 => {
            c.fill_rect(w * 0.1, h * 0.2, w * 0.9, h * 0.325);
            c.fill_rect(w * 0.1, h * 0.675, w * 0.9, h * 0.8);
            c.fill_rect(w * 0.1, h * 0.2, w * 0.225, h * 0.8);
            c.fill_rect(w * 0.775, h * 0.2, w * 0.9, h * 0.8);
        }
        0x25AA => c.fill_rect(w * 0.25, h * 0.3, w * 0.75, h * 0.7),
        0x25AB => {
            c.fill_rect(w * 0.25, h * 0.3, w * 0.75, h * 0.4);
            c.fill_rect(w * 0.25, h * 0.6, w * 0.75, h * 0.7);
            c.fill_rect(w * 0.25, h * 0.3, w * 0.35, h * 0.7);
            c.fill_rect(w * 0.65, h * 0.3, w * 0.75, h * 0.7);
        }
        0x25AC => c.fill_rect(0.0, h * 0.3, w, h * 0.7),
        0x25B2 => c.fill_poly(&[(mx, h * 0.08), (w * 0.92, h * 0.88), (w * 0.08, h * 0.88)]),
        0x25BA => c.fill_poly(&[(w * 0.08, h * 0.08), (w * 0.92, my), (w * 0.08, h * 0.92)]),
        0x25BC => c.fill_poly(&[(w * 0.08, h * 0.12), (w * 0.92, h * 0.12), (mx, h * 0.92)]),
        0x25C4 => c.fill_poly(&[(w * 0.92, h * 0.08), (w * 0.08, my), (w * 0.92, h * 0.92)]),
        0x25CF => c.fill_ellipse(mx, my, r, r),
        0x25CB => c.arc(
            mx,
            my,
            r - h * 0.05,
            0.0,
            std::f32::consts::PI * 2.0,
            h * 0.06,
        ),
        0x25C6 => c.fill_poly(&[
            (mx, h * 0.08),
            (w * 0.92, my),
            (mx, h * 0.92),
            (w * 0.08, my),
        ]),
        0x25C7 => {
            c.line(mx, h * 0.08, w * 0.92, my, h * 0.055);
            c.line(w * 0.92, my, mx, h * 0.92, h * 0.055);
            c.line(mx, h * 0.92, w * 0.08, my, h * 0.055);
            c.line(w * 0.08, my, mx, h * 0.08, h * 0.055);
        }
        0x25D0 => {
            c.fill_ellipse(mx, my, r, r);
            c.fill_rect(0.0, 0.0, mx, h);
        }
        0x25D1 => {
            c.fill_ellipse(mx, my, r, r);
            c.fill_rect(mx, 0.0, w, h);
        }
        _ => return false,
    }
    true
}

// ---------- 盲文 U+2800-28FF ----------

fn draw_braille(cp: u32, c: &mut Canvas) -> bool {
    let bits = cp - 0x2800;
    let (w, h) = (c.w, c.h);
    let dot = |c: &mut Canvas, bit: u32, cx: f32, cy: f32| {
        if bits & (1 << bit) != 0 {
            let r = (w / 5.5).max(1.2);
            c.fill_ellipse(cx, cy, r, r);
        }
    };
    let (l, rr) = (w * 0.28, w * 0.72);
    dot(c, 0, l, h * 0.145);
    dot(c, 1, l, h * 0.375);
    dot(c, 2, l, h * 0.605);
    dot(c, 3, l, h * 0.835);
    dot(c, 4, rr, h * 0.145);
    dot(c, 5, rr, h * 0.375);
    dot(c, 6, rr, h * 0.605);
    dot(c, 7, rr, h * 0.835);
    true
}

// ---------- Powerline U+E0B0-E0D7 ----------

fn draw_powerline(cp: u32, c: &mut Canvas) -> bool {
    let (w, h) = (c.w, c.h);
    let t = (h / 10.0).clamp(1.0, 3.0);
    let half = t / 2.0;
    match cp {
        // 三角满格无缝(斜边角到角)
        0xE0B0 => {
            c.fill_poly(&[(0.0, 0.0), (w, h / 2.0), (0.0, h)]);
            // 左缘细实条:顶点处三角薄如刀尖,中心采样漏墨=平铺露缝
            c.fill_rect(-0.5, 0.0, 0.5, h);
        }
        0xE0B1 => c.fill_poly(&[(w, 0.0), (w, h), (0.0, h / 2.0)]),
        0xE0B2 => {
            c.fill_poly(&[(w, 0.0), (0.0, h / 2.0), (w, h)]);
            c.fill_rect(w - 0.5, 0.0, w + 0.5, h);
        }
        0xE0B3 => c.fill_poly(&[(0.0, 0.0), (w, 0.0), (w / 2.0, h)]),
        // 圆帽贴边
        0xE0B4 => c.fill_ellipse(0.0, h / 2.0, w * 0.95, h / 2.0),
        0xE0B5 => c.fill_ellipse(w, h / 2.0, w * 0.95, h / 2.0),
        0xE0B6 => c.fill_ellipse(w, h / 2.0, w * 0.95, h / 2.0),
        0xE0B7 => c.fill_ellipse(0.0, h / 2.0, w * 0.95, h / 2.0),
        // 斜线分隔
        0xE0B8 => c.line(0.0, h, w, 0.0, half),
        0xE0B9 => c.line(0.0, 0.0, w, h, half),
        0xE0BA => c.line(0.0, h, w, 0.0, half),
        0xE0BB => c.line(0.0, 0.0, w, h, half),
        _ => return false,
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 格子几何契约:线形无缝(横线整行满、竖线整列满)、方块精确吸附、
    /// 三角满格——sprite face 的立身之本
    #[test]
    fn box_lines_tile_seamlessly() {
        let (w, h) = (10.0, 21.0);
        let (_, _, cov) = draw(0x2500, w, h).unwrap();
        let mid = (h / 2.0).floor() as usize * w as usize;
        for x in 0..w as usize {
            assert!(cov[mid + x] > 200, "─ 第 {x} 列中线缺墨");
        }
        let (_, _, cov) = draw(0x2502, w, h).unwrap();
        let midx = (w / 2.0).floor() as usize;
        for y in 0..h as usize {
            assert!(cov[y * w as usize + midx] > 200, "│ 第 {y} 行缺墨");
        }
    }

    #[test]
    fn corners_touch_all_edges() {
        let (w, h) = (10.0, 21.0);
        let (_, _, cov) = draw(0x250C, w, h).unwrap();
        let midx = (w / 2.0).floor() as usize;
        let midy = (h / 2.0).floor() as usize;
        assert!(cov[midx] > 200, "┌ 顶边竖笔缺墨");
        assert!(cov[midy * w as usize] > 200, "┌ 左边横笔缺墨");
        let (_, _, cov) = draw(0x2518, w, h).unwrap();
        let bottom = (h as usize - 1) * w as usize + midx;
        let right = midy * w as usize + w as usize - 1;
        assert!(cov[bottom] > 200, "┘ 底边竖笔缺墨");
        assert!(cov[right] > 200, "┘ 右边横笔缺墨");
    }

    #[test]
    fn blocks_exact_fractions() {
        let (w, h) = (10.0, 22.0);
        let (_, _, cov) = draw(0x2588, w, h).unwrap();
        assert!(cov.iter().all(|&v| v == 255), "█ 满格");
        let (_, _, cov) = draw(0x2580, w, h).unwrap();
        assert!(
            cov[0] == 255 && cov[(w as usize) * (h as usize - 1)] == 0,
            "▀ 上实下空"
        );
        let (_, _, cov) = draw(0x2584, w, h).unwrap();
        assert!(
            cov[0] == 0 && cov[(w as usize) * (h as usize - 1)] == 255,
            "▄ 下实上空"
        );
    }

    #[test]
    fn powerline_triangles_fill_cell() {
        let (w, h) = (10.0, 21.0);
        let (_, _, cov) = draw(0xE0B0, w, h).unwrap();
        let idx = |x: usize, y: usize| y * w as usize + x;
        // 左缘整列满(竖直边),斜边中点满——三角贴满格高的契约
        // 左缘细条约半像素宽:契约是"无空点"(>80),不是满墨
        for y in 0..h as usize {
            assert!(cov[idx(0, y)] > 80, "E0B0 左缘第 {y} 行缺墨");
        }
        assert!(cov[idx(9, 10)] > 200, "E0B0 斜边中点缺墨");
    }

    #[test]
    fn braille_dots_follow_bits() {
        let (w, h) = (10.0, 21.0);
        let blank = draw(0x2800, w, h).unwrap().2;
        assert!(blank.iter().all(|&v| v == 0), "空白盲文无墨");
        let one = draw(0x2801, w, h).unwrap().2;
        assert!(one.iter().any(|&v| v > 200), "Bit1 点应有墨");
    }

    #[test]
    fn three_quadrant_blocks_carve_correct_corner() {
        let (w, h) = (10.0, 21.0);
        let idx = |x: usize, y: usize| y * w as usize + x;
        // ▛(259B)= 满格挖右下:左上实、右下空
        let cov = draw(0x259B, w, h).unwrap().2;
        assert!(cov[idx(0, 0)] == 255, "259B 左上应满");
        assert!(cov[idx(9, 20)] == 0, "▛ 右下应空");
        // ▜(259C)= 满格挖左下:右上实、左下空
        let cov = draw(0x259C, w, h).unwrap().2;
        assert!(cov[idx(9, 0)] == 255, "▜ 右上应满");
        assert!(cov[idx(0, 20)] == 0, "▜ 左下应空");
    }

    #[test]
    fn out_of_range_returns_none() {
        assert!(draw(0x41, 10.0, 21.0).is_none(), "字母回落字体");
        assert!(draw(0x25FF, 10.0, 21.0).is_none(), "未覆盖几何形状回落字体");
    }
}
