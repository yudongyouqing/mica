//! 字形图集:R8 纹素、shelf 行分配、放不下自动倍增高并重排。
//! 仅在主线程访问(渲染路径),无锁。

/// 待排布的字形位图(R8,每像素 1 字节灰度)。
#[derive(Clone)]
pub struct GlyphBitmap {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
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
    /// 重排保真用条目表(M1 字形量级百级,克隆成本可忽略)
    entries: Vec<(GlyphBitmap, Rect)>,
    /// 仅 grow(重排搬动全部条目)递增。DwriteRouter 据此清 UV 缓存;
    /// 普通插入不动它,否则每个新字形都白清一次缓存。
    version: u64,
    /// 每次内容写入(普通 insert 与 grow)都递增。`Renderer::set_atlas`
    /// 据此决定是否重传纹理——普通插入也得重传,漏了字形就隐形。
    revision: u64,
}

impl GlyphAtlas {
    pub fn new(width: u32, height: u32) -> Self {
        let (width, height) = (width.max(1), height.max(1));
        Self {
            width,
            height,
            data: vec![0; (width * height) as usize],
            shelves: Vec::new(),
            entries: Vec::new(),
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
        loop {
            if let Some(rect) = self.try_place(bmp.width, bmp.height) {
                self.blit(bmp, rect);
                self.entries.push((bmp.clone(), rect));
                self.revision += 1;
                return rect;
            }
            self.grow(bmp.width);
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

    fn grow(&mut self, pending_w: u32) {
        // 单字形超宽:宽度提到能容纳它的 2 的幂——既保证本轮必能放下,
        // 也保持纹理边长为 2 的幂(GPU 纹理友好,UV 归一化稳定)。
        if pending_w > self.width {
            self.width = pending_w.next_power_of_two();
        }
        // 高度倍增
        self.height = self.height.saturating_mul(2).max(1);
        self.data = vec![0; (self.width * self.height) as usize];
        self.shelves.clear();
        self.version += 1;
        self.revision += 1;
        let entries = std::mem::take(&mut self.entries);
        for (bmp, _) in entries {
            let rect = self
                .try_place(bmp.width, bmp.height)
                .expect("doubled atlas must fit prior entries");
            self.blit(&bmp, rect);
            self.entries.push((bmp, rect));
        }
    }

    fn blit(&mut self, bmp: &GlyphBitmap, r: Rect) {
        for y in 0..r.h {
            let dst = ((r.v + y) * self.width + r.u) as usize;
            let src = (y * bmp.width) as usize;
            self.data[dst..dst + r.w as usize]
                .copy_from_slice(&bmp.pixels[src..src + r.w as usize]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bmp(w: u32, h: u32, tag: u8) -> GlyphBitmap {
        GlyphBitmap {
            width: w,
            height: h,
            pixels: vec![tag; (w * h) as usize],
        }
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
        assert_eq!(a.texture().len(), (a.width() * a.height()) as usize);
    }

    #[test]
    fn pixels_land_at_their_rect() {
        let mut a = GlyphAtlas::new(64, 64);
        let r = a.insert(&bmp(3, 2, 7));
        for y in 0..r.h {
            for x in 0..r.w {
                assert_eq!(a.texture()[((r.v + y) * a.width() + r.u + x) as usize], 7);
            }
        }
    }

    #[test]
    fn growth_repacks_and_preserves_pixels() {
        let mut a = GlyphAtlas::new(32, 32);
        let r1 = a.insert(&bmp(16, 16, 9));
        assert_eq!(a.version(), 0);
        assert_eq!(a.revision(), 1);
        // 高度 30 的字形在 32 高图集放不下 → 倍增重排
        let r2 = a.insert(&bmp(16, 30, 5));
        assert_eq!(a.height(), 64);
        assert!(a.version() >= 1);
        assert!(a.revision() >= 2);
        // 两个矩形的内容都还在(位置可能变了)
        for (r, tag) in [(r1, 9u8), (r2, 5u8)] {
            for y in 0..r.h {
                for x in 0..r.w {
                    assert_eq!(
                        a.texture()[((r.v + y) * a.width() + r.u + x) as usize],
                        tag,
                        "{r:?} tag{tag}"
                    );
                }
            }
        }
    }

    #[test]
    fn glyph_wider_than_atlas_grows_width_and_terminates() {
        let mut a = GlyphAtlas::new(16, 16);
        let r = a.insert(&bmp(40, 8, 3)); // 比图集宽
        assert!(r.u + r.w <= a.width() && r.v + r.h <= a.height());
        assert!(a.width() >= 40);
        // 内容落位
        for y in 0..r.h {
            for x in 0..r.w {
                assert_eq!(a.texture()[((r.v + y) * a.width() + r.u + x) as usize], 3);
            }
        }
        // 后续常规插入继续正常
        let r2 = a.insert(&bmp(5, 5, 4));
        assert!(r2.u + r2.w <= a.width() && r2.v + r2.h <= a.height());
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
        assert_eq!(a.texture().len(), (a.width() * a.height()) as usize);
        for (i, r) in rects.iter().enumerate() {
            assert!(
                r.u + r.w <= a.width() && r.v + r.h <= a.height(),
                "#{i} {r:?}"
            );
        }
    }
}
