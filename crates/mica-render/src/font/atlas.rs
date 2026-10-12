//! 字形图集(2026-10-08 页追加式根治重构):RGBA8 纹素、shelf 行分配、
//! 放不下**只向下追加整页**——条目一旦落位永不位移。这是结构性契约:
//! 历史版本 grow 时全表重排,所有烘焙了 UV 的跨帧缓存(路由缓存/行级
//! 实例缓存)同时作废,漏一处监听就是"一块一块/乱码"(同类四连发,
//! 见 0608b73 复盘);页追加后这类 bug 从结构上不可能发生。
//! 配套:GlyphInfo 的 UV 语义改为**纹素坐标**,shader 用 globals.
//! atlas_size 归一——高度追加不改变既有条目的归一化结果。
//! 仅在主线程访问(渲染路径),无锁。
//! 存储非预乘:灰度字形 RGB 恒白、A=coverage(染色交给 shader 的 fg 乘);
//! 彩色字形(COLR)RGB=原色、A=不透明度,渲染时 fg 填白即得原色。

/// 位图格式:Coverage = 灰度覆盖(进 A 通道);ColorRgba = 彩色原色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlyphFormat {
    Coverage,
    ColorRgba,
}

/// 待排布的字形位图(RGBA8 非预乘,行主序,每像素 4 字节)。
#[derive(Clone)]
pub struct GlyphBitmap {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
    pub format: GlyphFormat,
}

impl GlyphBitmap {
    /// 灰度 coverage(R8 逐像素)→ RGBA:RGB 白、A=coverage。
    pub fn from_coverage(width: u32, height: u32, coverage: Vec<u8>) -> Self {
        assert_eq!(coverage.len(), (width * height) as usize);
        let mut pixels = vec![0u8; coverage.len() * 4];
        for (i, &v) in coverage.iter().enumerate() {
            pixels[i * 4] = 255;
            pixels[i * 4 + 1] = 255;
            pixels[i * 4 + 2] = 255;
            pixels[i * 4 + 3] = v;
        }
        Self {
            width,
            height,
            pixels,
            format: GlyphFormat::Coverage,
        }
    }
}

/// 图集内矩形(像素坐标,UV 换算由消费方做)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub u: u32,
    pub v: u32,
    pub w: u32,
    pub h: u32,
}

struct Shelf {
    y: u32,
    height: u32,
    used_x: u32,
}

pub struct GlyphAtlas {
    width: u32,
    height: u32,
    data: Vec<u8>,
    shelves: Vec<Shelf>,
    /// 历史遗留兼容字段:页追加式下**永不再变**(无重排)。保留是
    /// 为了消费方(DwriteRouter/行缓存)的版本对比代码零改动——
    /// 恒 0 意味着永不触发清缓存,语义安全。
    version: u64,
    /// 每次内容写入(普通 insert 与页追加)都递增。`Renderer::set_atlas`
    /// 据此决定是否重传纹理——普通插入也得重传,漏了字形就隐形。
    revision: u64,
}

impl GlyphAtlas {
    pub fn new(width: u32, height: u32) -> Self {
        let (width, height) = (width.max(1), height.max(1));
        Self {
            width,
            height,
            data: vec![0; (width * height) as usize * 4], // RGBA8
            shelves: Vec::new(),
            version: 0,
            revision: 0,
        }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn texture(&self) -> &[u8] {
        &self.data
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    /// 纹理内容修订号:任何写入(普通 insert 或 grow 重排)都递增。
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn insert(&mut self, bmp: &GlyphBitmap) -> Rect {
        // 宽度恒定的守卫:超宽字形无页可追加(会死循环)——512px 宽
        // 已覆盖 400pt 级字形的极限,真触发说明字体栈异常,宁可快败
        assert!(
            bmp.width < self.width,
            "字形宽 {} 超过图集固定宽 {}(字体栈异常?)",
            bmp.width,
            self.width
        );
        // I2 处方(M5a/T8):右/下各 +1px 零填充——半纹素采样越界时读到
        // 零墨而非邻居字形边缘,渗色消除。矩形保持真实尺寸,UV 不变;
        // 预留位靠 data 零初始化 + 布局推进保持零(重排路径同样 +1)
        loop {
            if let Some(mut rect) = self.try_place(bmp.width + 1, bmp.height + 1) {
                rect.w = bmp.width;
                rect.h = bmp.height;
                self.blit(bmp, rect);
                self.revision += 1;
                return rect;
            }
            self.grow();
        }
    }

    fn try_place(&mut self, w: u32, h: u32) -> Option<Rect> {
        if w > self.width {
            return None; // 单字形超宽:由 grow 加宽处理
        }
        for shelf in &mut self.shelves {
            if h <= shelf.height && shelf.used_x + w <= self.width {
                let r = Rect {
                    u: shelf.used_x,
                    v: shelf.y,
                    w,
                    h,
                };
                shelf.used_x += w;
                return Some(r);
            }
        }
        let last_bottom = self.shelves.last().map_or(0, |s| s.y + s.height);
        if last_bottom + h <= self.height {
            self.shelves.push(Shelf {
                y: last_bottom,
                height: h,
                used_x: w,
            });
            return Some(Rect {
                u: 0,
                v: last_bottom,
                w,
                h,
            });
        }
        None
    }

    /// 页追加(2026-10-08 根治):放不下时**只把高度加一整页**(256px),
    /// 数据区尾部补零,既有条目与 shelf 全部原样不动。历史版本的
    /// "倍增+全表重排"是四连发 bug 的结构病灶,已删除。
    /// 页高取 256:与初始宽一致,bytes_per_row 对齐契约不变。
    fn grow(&mut self) {
        const PAGE: u32 = 256;
        self.height += PAGE;
        self.data.resize((self.width * self.height) as usize * 4, 0);
        self.revision += 1;
    }

    fn blit(&mut self, bmp: &GlyphBitmap, r: Rect) {
        for y in 0..r.h {
            let dst = (((r.v + y) * self.width + r.u) * 4) as usize;
            let src = ((y * bmp.width) * 4) as usize;
            let span = (r.w * 4) as usize;
            self.data[dst..dst + span].copy_from_slice(&bmp.pixels[src..src + span]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试位图:A 通道 = tag,RGB 白(灰度字形真实形态)。
    fn bmp(w: u32, h: u32, tag: u8) -> GlyphBitmap {
        GlyphBitmap::from_coverage(w, h, vec![tag; (w * h) as usize])
    }

    fn tex_alpha(a: &GlyphAtlas, x: u32, y: u32) -> u8 {
        a.texture()[((y * a.width() + x) * 4 + 3) as usize]
    }

    #[test]
    fn inserts_do_not_overlap() {
        let mut a = GlyphAtlas::new(128, 128);
        let r1 = a.insert(&bmp(10, 10, 1));
        let r2 = a.insert(&bmp(20, 12, 2));
        let r3 = a.insert(&bmp(5, 40, 3));
        for (ra, rb) in [(r1, r2), (r1, r3), (r2, r3)] {
            assert!(
                ra.u + ra.w <= rb.u
                    || rb.u + rb.w <= ra.u
                    || ra.v + ra.h <= rb.v
                    || rb.v + rb.h <= ra.v,
                "{ra:?} overlaps {rb:?}"
            );
        }
    }

    /// I2 零填充(M5a/T8):右/下 1px 恒零墨,后续字形从 +1 处起。
    #[test]
    fn insert_pads_zero_border_right_bottom() {
        let mut a = GlyphAtlas::new(64, 64);
        let r1 = a.insert(&bmp(10, 10, 0xff));
        let r2 = a.insert(&bmp(10, 10, 0xff));
        assert_eq!(r2.u, r1.u + 11, "同 shelf 布局推进 = w+1(含填充)");
        assert_eq!(tex_alpha(&a, r1.u + 10, r1.v), 0, "右侧 padding 零墨");
        assert_eq!(tex_alpha(&a, r1.u, r1.v + 10), 0, "下侧 padding 零墨");
        // 页追加(grow)后填充保持:高字形触发追加页,旧条目原位
        let big = bmp(10, 200, 0xff); // 触发页追加
        let r3 = a.insert(&big);
        assert_eq!(
            tex_alpha(&a, r3.u + r3.w, r3.v),
            0,
            "追加页条目右 padding 零"
        );
        assert_eq!(
            tex_alpha(&a, r1.u + 10, r1.v),
            0,
            "旧条目原位,右 padding 仍零"
        );
        assert_eq!(
            a.texture().len(),
            (a.width() * a.height()) as usize * 4,
            "RGBA8"
        );
    }

    #[test]
    fn pixels_land_at_their_rect() {
        let mut a = GlyphAtlas::new(64, 64);
        let r = a.insert(&bmp(3, 2, 7));
        for y in 0..r.h {
            for x in 0..r.w {
                assert_eq!(tex_alpha(&a, r.u + x, r.v + y), 7, "coverage 落 A 通道");
            }
        }
    }

    /// 页追加契约(2026-10-08 根治重构):grow 后**条目原位不动**——
    /// 坐标逐位相等 + 内容原样,version 恒 0(无重排信号)。
    /// 这是四连发 bug 类的结构性死亡证明。
    #[test]
    fn growth_appends_page_and_never_moves_entries() {
        let mut a = GlyphAtlas::new(32, 32);
        let r1 = a.insert(&bmp(16, 16, 9));
        let r2 = a.insert(&bmp(16, 10, 6));
        let (v1, rev1) = (a.version(), a.revision());
        // 高度 30 的字形在 32 高图集放不下 → 追加一整页
        let r3 = a.insert(&bmp(16, 30, 5));
        assert_eq!(a.height(), 32 + 256, "追加整页 256");
        assert_eq!(a.version(), v1, "无重排:version 恒不变");
        assert!(a.revision() > rev1);
        // 旧条目坐标逐位不变:内容仍在原坐标即证(r1/r2 的 rect 在
        // 追加前后未被重算——页追加式下 insert 不触碰既有条目)
        for (r, tag) in [(r1, 9u8), (r2, 6u8), (r3, 5u8)] {
            for y in 0..r.h {
                for x in 0..r.w {
                    assert_eq!(tex_alpha(&a, r.u + x, r.v + y), tag, "{r:?} tag{tag}");
                }
            }
        }
        assert!(r3.v + r3.h > 32, "新条目必然伸入追加页(原 32 高放不下)");
    }

    /// 超宽字形守卫(页追加式):宽度恒定,超宽即快败(不静默死循环)
    #[test]
    #[should_panic(expected = "超过图集固定宽")]
    fn glyph_wider_than_atlas_panics_loudly() {
        let mut a = GlyphAtlas::new(16, 16);
        let _ = a.insert(&bmp(40, 8, 3));
    }

    /// C1 回归锁:普通插入必须递增 revision(set_atlas 门控靠它重传),
    /// 且不动 version(那是重排信号,动了会白清 dwrite 缓存)。
    #[test]
    fn plain_insert_bumps_revision_without_repack() {
        let mut a = GlyphAtlas::new(64, 64);
        a.insert(&bmp(10, 10, 1));
        a.insert(&bmp(5, 5, 2));
        assert_eq!(a.version(), 0, "未触发 grow,不应发重排信号");
        assert_eq!(a.revision(), 2, "两次写入都算内容修订");
    }

    #[test]
    fn many_inserts_stay_consistent() {
        let mut a = GlyphAtlas::new(64, 64);
        let mut rects = Vec::new();
        for i in 0..200u8 {
            rects.push(a.insert(&bmp((i % 7 + 2) as u32, (i % 5 + 2) as u32, i)));
        }
        assert_eq!(a.texture().len(), (a.width() * a.height()) as usize * 4);
        for (i, r) in rects.iter().enumerate() {
            assert!(
                r.u + r.w <= a.width() && r.v + r.h <= a.height(),
                "#{i} {r:?}"
            );
        }
    }
}
