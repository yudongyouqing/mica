//! Append-only RGBA atlas with stable pixel UVs inside a generation.
//! Content revision tracks inserts/growth; layout version changes only at an
//! explicit frame boundary. CPU bitmap capacity has pane/process byte leases.
//! A pinned visible marker handles quota/invalid-size failures without moving
//! old UVs or silently dropping ordinary glyphs.

/// 位图格式:Coverage = 灰度覆盖(进 A 通道);ColorRgba = 彩色原色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

use std::collections::VecDeque;
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};

pub const PANE_ATLAS_BYTES: usize = 16 * 1024 * 1024;
pub const PROCESS_ATLAS_BYTES: usize = 128 * 1024 * 1024;
pub const WINDOW_GPU_ATLAS_BYTES: usize = 32 * 1024 * 1024;

/// Live owned resource bytes, shared by every window/thread. Reservations precede
/// allocation; Drop returns the quota. This excludes allocator/driver overhead.
pub struct ByteBudget {
    limit: usize,
    used: AtomicUsize,
    released: AtomicU64,
}
impl ByteBudget {
    pub fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            limit,
            used: AtomicUsize::new(0),
            released: AtomicU64::new(0),
        })
    }
    pub fn used(&self) -> usize {
        self.used.load(Ordering::Relaxed)
    }
    pub fn released_epoch(&self) -> u64 {
        self.released.load(Ordering::Relaxed)
    }
    pub fn reserve(self: &Arc<Self>, bytes: usize) -> Option<BytePermit> {
        let mut permit = BytePermit {
            budget: self.clone(),
            bytes: 0,
        };
        permit.grow(bytes).then_some(permit)
    }
}
pub struct BytePermit {
    budget: Arc<ByteBudget>,
    bytes: usize,
}
impl BytePermit {
    pub fn bytes(&self) -> usize {
        self.bytes
    }
    pub fn grow(&mut self, bytes: usize) -> bool {
        if self
            .budget
            .used
            .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |used| {
                used.checked_add(bytes).filter(|&n| n <= self.budget.limit)
            })
            .is_err()
        {
            return false;
        }
        self.bytes += bytes;
        true
    }
    pub fn shrink(&mut self, bytes: usize) {
        assert!(bytes <= self.bytes);
        if bytes != 0 {
            self.bytes -= bytes;
            self.budget.used.fetch_sub(bytes, Ordering::AcqRel);
            self.budget.released.fetch_add(1, Ordering::Relaxed);
        }
    }
}
impl Drop for BytePermit {
    fn drop(&mut self) {
        self.shrink(self.bytes);
    }
}

pub fn cpu_atlas_budget() -> Arc<ByteBudget> {
    static BUDGET: OnceLock<Arc<ByteBudget>> = OnceLock::new();
    BUDGET
        .get_or_init(|| ByteBudget::new(PROCESS_ATLAS_BYTES))
        .clone()
}
pub fn gpu_atlas_budget() -> Arc<ByteBudget> {
    static BUDGET: OnceLock<Arc<ByteBudget>> = OnceLock::new();
    BUDGET
        .get_or_init(|| ByteBudget::new(PROCESS_ATLAS_BYTES))
        .clone()
}
pub(crate) fn shaping_budget() -> Arc<ByteBudget> {
    static BUDGET: OnceLock<Arc<ByteBudget>> = OnceLock::new();
    BUDGET
        .get_or_init(|| ByteBudget::new(16 * 1024 * 1024))
        .clone()
}
pub(crate) fn character_budget() -> Arc<ByteBudget> {
    static BUDGET: OnceLock<Arc<ByteBudget>> = OnceLock::new();
    BUDGET
        .get_or_init(|| ByteBudget::new(32 * 1024 * 1024))
        .clone()
}

#[derive(Clone, Copy)]
struct AtlasEntry {
    hash: u64,
    rect: Rect,
}
fn index_budget() -> Arc<ByteBudget> {
    static BUDGET: OnceLock<Arc<ByteBudget>> = OnceLock::new();
    BUDGET
        .get_or_init(|| ByteBudget::new(16 * 1024 * 1024))
        .clone()
}

struct Shelf {
    y: u32,
    height: u32,
    used_x: u32,
}

/// Stable identity survives growth and generation changes. Layout only changes
/// in prepare_frame, before any row/overlay instances are built.
pub struct GlyphAtlas {
    id: u64,
    life: Arc<()>,
    width: u32,
    height: u32,
    initial_height: u32,
    max_height: u32,
    max_bytes: usize,
    data: Vec<u8>,
    permit: BytePermit,
    shelves: Vec<Shelf>,
    // Sorted metadata can move; bitmap pixel UVs never do. Full byte comparison
    // resolves hash collisions, so an LRU miss cannot append duplicate ink.
    index: Vec<AtlasEntry>,
    index_permit: BytePermit,
    index_max_bytes: usize,
    version: u64,
    revision: u64,
    dirty: VecDeque<(u64, Rect)>,
    exhausted: bool,
    last_limit_log: Option<std::time::Instant>,
    recovery_key: Option<u64>,
    blocked_extra: Option<usize>,
    failed_available: usize,
    blocked_budget: Option<Arc<ByteBudget>>,
}
impl GlyphAtlas {
    pub fn new(width: u32, height: u32) -> Self {
        Self::try_new(width, height).expect("atlas initial resource budget")
    }
    pub fn try_new(width: u32, height: u32) -> anyhow::Result<Self> {
        Self::with_budget(width, height, PANE_ATLAS_BYTES, cpu_atlas_budget())
    }
    pub fn with_budget(
        width: u32,
        height: u32,
        max_bytes: usize,
        budget: Arc<ByteBudget>,
    ) -> anyhow::Result<Self> {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let (width, height) = (width.max(8), height.max(8));
        anyhow::ensure!(
            width <= 8192 && height <= 8192,
            "atlas initial dimensions exceed portable device limit"
        );
        let bytes =
            Self::byte_len(width, height).ok_or_else(|| anyhow::anyhow!("atlas size overflow"))?;
        anyhow::ensure!(bytes <= max_bytes, "atlas initial pane budget exceeded");
        let permit = budget
            .reserve(bytes)
            .ok_or_else(|| anyhow::anyhow!("process CPU atlas budget exhausted"))?;
        let mut data = Vec::new();
        data.try_reserve_exact(bytes)?;
        data.resize(bytes, 0);
        let mut permit = permit;
        anyhow::ensure!(
            data.capacity() <= max_bytes && permit.grow(data.capacity() - bytes),
            "atlas allocated capacity exceeds resource budget"
        );
        let mut atlas = Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            life: Arc::new(()),
            width,
            height,
            initial_height: height,
            max_height: 8192,
            max_bytes,
            data,
            permit,
            shelves: Vec::new(),
            index: Vec::new(),
            index_permit: index_budget()
                .reserve(0)
                .expect("zero-byte index reservation"),
            index_max_bytes: 2 * 1024 * 1024,
            version: 0,
            revision: 0,
            dirty: VecDeque::new(),
            exhausted: false,
            last_limit_log: None,
            recovery_key: None,
            blocked_extra: None,
            failed_available: 0,
            blocked_budget: None,
        };
        atlas.seed_fallback();
        Ok(atlas)
    }
    fn byte_len(width: u32, height: u32) -> Option<usize> {
        (width as usize)
            .checked_mul(height as usize)?
            .checked_mul(4)
    }
    pub fn identity(&self) -> u64 {
        self.id
    }
    pub fn lifetime(&self) -> std::sync::Weak<()> {
        Arc::downgrade(&self.life)
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
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn exhausted(&self) -> bool {
        self.exhausted
    }
    pub fn fallback_rect(&self) -> Rect {
        Rect {
            u: 0,
            v: 0,
            w: 3,
            h: 5,
        }
    }
    fn seed_fallback(&mut self) {
        // A pinned, visible question mark. Ordinary allocation failure must never
        // become a silent blank, nor overwrite UVs still referenced this frame.
        let coverage = vec![255, 255, 255, 0, 0, 255, 0, 255, 0, 0, 0, 0, 0, 255, 0];
        self.blit(
            &GlyphBitmap::from_coverage(3, 5, coverage),
            self.fallback_rect(),
        );
        self.shelves.push(Shelf {
            y: 0,
            height: 6,
            used_x: 4,
        });
    }
    fn changed(&mut self, rect: Rect) {
        self.revision += 1;
        self.dirty.push_back((self.revision, rect));
        if self.dirty.len() > 256 {
            self.dirty.pop_front();
        }
    }
    /// Every consumer tracks its own committed revision. If its bounded journal
    /// history expired (or a generation changed), it must upload the whole image.
    pub fn dirty_since(&self, revision: u64) -> Option<Rect> {
        if revision == self.revision {
            return None;
        }
        if revision > self.revision || self.dirty.front().is_none_or(|(r, _)| revision + 1 < *r) {
            return Some(Rect {
                u: 0,
                v: 0,
                w: self.width,
                h: self.height,
            });
        }
        self.dirty
            .iter()
            .filter(|(r, rect)| *r > revision && rect.w != 0 && rect.h != 0)
            .map(|(_, r)| *r)
            .reduce(|a, b| {
                let u = a.u.min(b.u);
                let v = a.v.min(b.v);
                Rect {
                    u,
                    v,
                    w: (a.u + a.w).max(b.u + b.w) - u,
                    h: (a.v + a.h).max(b.v + b.h) - v,
                }
            })
    }
    /// Called once at the frame boundary, including hidden-to-visible transitions.
    /// One failed working set gets one retry. Unchanged oversized viewports do not
    /// cause generation/rebuild storms; quota release or changed content retries.
    pub fn prepare_frame(&mut self, fingerprint: u64, max_dimension: u32) -> bool {
        self.max_height = max_dimension.min(8192).max(self.initial_height);
        let budget = self.blocked_budget.as_ref().unwrap_or(&self.permit.budget);
        let available = budget.limit.saturating_sub(budget.used());
        let quota_recovered = self
            .blocked_extra
            .is_some_and(|extra| available >= extra && available > self.failed_available);
        let dimension_changed = self.height > self.max_height;
        if !dimension_changed
            && (!self.exhausted || (self.recovery_key == Some(fingerprint) && !quota_recovered))
        {
            return false;
        }
        let initial_bytes =
            Self::byte_len(self.width, self.initial_height).expect("validated dimensions");
        self.data.truncate(initial_bytes);
        self.data.shrink_to_fit();
        self.permit
            .shrink(self.permit.bytes() - self.data.capacity());
        self.height = self.initial_height;
        self.data.fill(0);
        self.shelves.clear();
        self.index.clear();
        self.index.shrink_to_fit();
        self.index_permit.shrink(self.index_permit.bytes());
        self.seed_fallback();
        self.version += 1;
        self.dirty.clear();
        self.changed(Rect {
            u: 0,
            v: 0,
            w: self.width,
            h: self.height,
        });
        self.exhausted = false;
        self.recovery_key = Some(fingerprint);
        self.blocked_extra = None;
        self.failed_available = 0;
        self.blocked_budget = None;
        true
    }
    pub fn insert(&mut self, bmp: &GlyphBitmap) -> Rect {
        if let Some(rect) = self.try_insert(bmp) {
            return rect;
        }
        if !self.exhausted
            && self
                .last_limit_log
                .is_none_or(|last| last.elapsed() >= std::time::Duration::from_secs(5))
        {
            self.last_limit_log = Some(std::time::Instant::now());
            eprintln!(
                "Mica: atlas {} resource limit ({} bytes, {}x{} glyph); using visible replacement until frame-boundary recovery",
                self.id,
                self.data.len(),
                bmp.width,
                bmp.height
            );
        }
        self.exhausted = true;
        self.fallback_rect()
    }
    fn try_insert(&mut self, bmp: &GlyphBitmap) -> Option<Rect> {
        use std::hash::{Hash, Hasher};
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        (bmp.width, bmp.height, bmp.format).hash(&mut hash);
        bmp.pixels.hash(&mut hash);
        self.try_insert_with_hash(bmp, hash.finish())
    }
    fn same_bitmap(&self, rect: Rect, bmp: &GlyphBitmap) -> bool {
        if rect.w != bmp.width || rect.h != bmp.height {
            return false;
        }
        (0..rect.h).all(|y| {
            let dst = (((rect.v + y) * self.width + rect.u) * 4) as usize;
            let src = (y * bmp.width * 4) as usize;
            let len = (rect.w * 4) as usize;
            self.data[dst..dst + len] == bmp.pixels[src..src + len]
        })
    }
    fn reserve_index_entry(&mut self) -> bool {
        if self.index.len() < self.index.capacity() {
            return true;
        }
        let size = std::mem::size_of::<AtlasEntry>();
        let max_entries = self.index_max_bytes / size;
        if self.index.len() >= max_entries {
            return false;
        }
        let target = (self.index.len() + 1)
            .next_multiple_of(256)
            .min(max_entries);
        let extra = (target - self.index.capacity()) * size;
        if !self.index_permit.grow(extra) {
            self.blocked_extra = Some(extra);
            self.blocked_budget = Some(self.index_permit.budget.clone());
            self.failed_available = self
                .index_permit
                .budget
                .limit
                .saturating_sub(self.index_permit.budget.used());
            return false;
        }
        if self
            .index
            .try_reserve_exact(target - self.index.len())
            .is_err()
        {
            self.index_permit.shrink(extra);
            return false;
        }
        let actual = self.index.capacity() * size;
        if actual > self.index_max_bytes
            || !self
                .index_permit
                .grow(actual.saturating_sub(self.index_permit.bytes()))
        {
            self.index.shrink_to_fit();
            self.index_permit
                .shrink(self.index_permit.bytes() - self.index.capacity() * size);
            return false;
        }
        true
    }
    fn try_insert_with_hash(&mut self, bmp: &GlyphBitmap, hash: u64) -> Option<Rect> {
        if Self::byte_len(bmp.width, bmp.height)? != bmp.pixels.len() {
            return None;
        }
        let w = bmp.width.checked_add(1)?;
        let h = bmp.height.checked_add(1)?;
        if w > self.width || h > self.max_height {
            return None;
        }
        let mut slot = self.index.partition_point(|entry| entry.hash < hash);
        while let Some(entry) = self.index.get(slot).filter(|entry| entry.hash == hash) {
            if self.same_bitmap(entry.rect, bmp) {
                return Some(entry.rect);
            }
            slot += 1;
        }
        if !self.reserve_index_entry() {
            return None;
        }
        loop {
            if let Some(mut rect) = self.try_place(w, h) {
                rect.w = bmp.width;
                rect.h = bmp.height;
                self.blit(bmp, rect);
                self.index.insert(slot, AtlasEntry { hash, rect });
                self.changed(rect);
                return Some(rect);
            }
            let height = self.height.checked_add(256)?.min(self.max_height);
            if height <= self.height {
                return None;
            }
            let bytes = Self::byte_len(self.width, height)?;
            let extra = bytes - self.data.len();
            if bytes > self.max_bytes {
                return None;
            }
            if !self.permit.grow(extra) {
                self.blocked_budget = Some(self.permit.budget.clone());
                self.blocked_extra = Some(extra);
                self.failed_available = self
                    .permit
                    .budget
                    .limit
                    .saturating_sub(self.permit.budget.used());
                return None;
            }
            if self.data.try_reserve_exact(extra).is_err() {
                self.permit.shrink(extra);
                return None;
            }
            let capacity = self.data.capacity();
            if capacity > self.max_bytes
                || !self
                    .permit
                    .grow(capacity.saturating_sub(self.permit.bytes()))
            {
                self.data.shrink_to_fit();
                self.permit
                    .shrink(self.permit.bytes() - self.data.capacity());
                return None;
            }
            let old_height = self.height;
            self.data.resize(bytes, 0);
            self.height = height;
            self.changed(Rect {
                u: 0,
                v: old_height,
                w: 0,
                h: 0,
            });
        }
    }
    fn try_place(&mut self, w: u32, h: u32) -> Option<Rect> {
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
        let bottom = self.shelves.last().map_or(0, |s| s.y + s.height);
        if bottom + h > self.height {
            return None;
        }
        self.shelves.push(Shelf {
            y: bottom,
            height: h,
            used_x: w,
        });
        Some(Rect {
            u: 0,
            v: bottom,
            w,
            h,
        })
    }
    fn blit(&mut self, bmp: &GlyphBitmap, r: Rect) {
        for y in 0..r.h {
            let dst = (((r.v + y) * self.width + r.u) * 4) as usize;
            let src = (y * bmp.width * 4) as usize;
            let span = (r.w * 4) as usize;
            self.data[dst..dst + span].copy_from_slice(&bmp.pixels[src..src + span]);
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn recovery_tracks_latest_denied_budget_after_metadata_then_pixel_pressure() {
        let pixels = ByteBudget::new(64 * 320 * 4);
        let pixel_blocker = pixels.reserve(64 * 64 * 4).unwrap();
        let mut a = GlyphAtlas::with_budget(64, 64, 64 * 320 * 4, pixels).unwrap();
        let size = std::mem::size_of::<AtlasEntry>();
        let metadata = ByteBudget::new(size);
        let metadata_blocker = metadata.reserve(size).unwrap();
        a.index_permit = metadata.reserve(0).unwrap();
        a.index_max_bytes = size;
        let glyph = bmp(40, 100, 200);
        a.insert(&glyph);
        assert!(a.prepare_frame(11, 8192));
        a.insert(&glyph);
        assert!(!a.prepare_frame(11, 8192));
        drop(metadata_blocker);
        assert_eq!(
            a.insert(&glyph),
            a.fallback_rect(),
            "index can now fit, but bitmap page still cannot"
        );
        drop(pixel_blocker);
        assert!(
            a.prepare_frame(11, 8192),
            "recovery must now inspect pixel quota rather than the old index quota"
        );
        assert_ne!(a.insert(&glyph), a.fallback_rect());
    }

    #[test]
    fn hash_collisions_compare_full_bitmap_before_reusing_uv() {
        let mut a = GlyphAtlas::new(64, 64);
        let first = bmp(10, 10, 40);
        let second = bmp(10, 10, 90);
        let r1 = a.try_insert_with_hash(&first, 7).unwrap();
        let r2 = a.try_insert_with_hash(&second, 7).unwrap();
        assert_ne!(r1, r2);
        let revision = a.revision();
        assert_eq!(a.try_insert_with_hash(&first, 7), Some(r1));
        assert_eq!(a.try_insert_with_hash(&second, 7), Some(r2));
        assert_eq!(a.revision(), revision);
        assert_eq!(tex_alpha(&a, r1.u, r1.v), 40);
        assert_eq!(tex_alpha(&a, r2.u, r2.v), 90);
    }
    #[test]
    fn metadata_capacity_is_bounded_and_released_with_generation_and_drop() {
        let size = std::mem::size_of::<AtlasEntry>();
        let budget = ByteBudget::new(2 * size);
        let mut a = GlyphAtlas::new(64, 64);
        a.index_permit = budget.reserve(0).unwrap();
        a.index_max_bytes = 2 * size;
        let first = a.insert(&bmp(10, 10, 40));
        a.insert(&bmp(10, 10, 50));
        assert_eq!(budget.used(), a.index.capacity() * size);
        assert_eq!(a.insert(&bmp(10, 10, 60)), a.fallback_rect());
        assert_eq!(tex_alpha(&a, first.u, first.v), 40);
        assert!(a.prepare_frame(1, 8192));
        assert_eq!(budget.used(), 0);
        assert_ne!(a.insert(&bmp(10, 10, 60)), a.fallback_rect());
        drop(a);
        assert_eq!(budget.used(), 0);
        // A live atlas with an unchanged working set also recovers when another
        // atlas releases the *metadata* process quota, not just pixel quota.
        let shared = ByteBudget::new(size);
        let mut active = GlyphAtlas::new(64, 64);
        active.index_permit = shared.reserve(0).unwrap();
        active.index_max_bytes = size;
        let mut other = GlyphAtlas::new(64, 64);
        other.index_permit = shared.reserve(0).unwrap();
        other.index_max_bytes = size;
        other.insert(&bmp(10, 10, 70));
        let glyph = bmp(10, 10, 80);
        assert_eq!(active.insert(&glyph), active.fallback_rect());
        assert!(active.prepare_frame(10, 8192));
        active.insert(&glyph);
        assert!(!active.prepare_frame(10, 8192));
        drop(other);
        assert!(active.prepare_frame(10, 8192));
        assert_ne!(active.insert(&glyph), active.fallback_rect());
        assert_eq!(shared.used(), active.index.capacity() * size);
        drop(active);
        assert_eq!(shared.used(), 0);
    }

    #[test]
    fn repeated_bitmaps_do_not_consume_space_after_router_cache_eviction() {
        let mut a = GlyphAtlas::new(64, 64);
        let glyph = bmp(20, 20, 255);
        let first = a.insert(&glyph);
        let revision = a.revision();
        for _ in 0..10000 {
            assert_eq!(a.insert(&glyph), first);
        }
        assert_eq!(a.revision(), revision);
        assert_eq!(a.height(), 64);
        assert!(!a.exhausted());
    }

    #[test]
    fn process_quota_recovery_requires_enough_newly_available_bytes() {
        let budget = ByteBudget::new(64 * 320 * 4);
        let mut a = GlyphAtlas::with_budget(64, 64, 64 * 320 * 4, budget.clone()).unwrap();
        let other = GlyphAtlas::with_budget(64, 64, 64 * 320 * 4, budget.clone()).unwrap();
        let tall = bmp(40, 100, 255);
        a.insert(&tall);
        assert!(a.prepare_frame(101, 8192));
        a.insert(&tall);
        assert!(!a.prepare_frame(101, 8192));
        drop(other);
        assert!(a.prepare_frame(101, 8192));
        assert_ne!(a.insert(&tall), a.fallback_rect());
    }
    #[test]
    fn static_oversized_panes_cannot_trigger_each_others_rebuilds() {
        let budget = ByteBudget::new(2 * 64 * 320 * 4);
        let mut a = GlyphAtlas::with_budget(64, 64, 64 * 320 * 4, budget.clone()).unwrap();
        let mut b = GlyphAtlas::with_budget(64, 64, 64 * 320 * 4, budget).unwrap();
        for pane in [&mut a, &mut b] {
            for tag in 0..4 {
                pane.insert(&bmp(40, 100, tag));
            }
        }
        for pane in [&mut a, &mut b] {
            assert!(pane.prepare_frame(1, 8192));
            for tag in 0..4 {
                pane.insert(&bmp(40, 100, tag));
            }
        }
        for _ in 0..30 {
            assert!(!a.prepare_frame(1, 8192));
            assert!(!b.prepare_frame(1, 8192));
        }
        assert_eq!((a.version(), b.version()), (1, 1));
    }

    #[test]
    fn device_rebuild_with_lower_dimension_invalidates_layout_at_frame_boundary() {
        let budget = ByteBudget::new(8 * 1024 * 1024);
        let mut a = GlyphAtlas::with_budget(512, 512, 8 * 1024 * 1024, budget).unwrap();
        for n in 0..1000u32 {
            let mut glyph = bmp(40, 40, 2);
            glyph.pixels[..4].copy_from_slice(&n.to_le_bytes());
            a.insert(&glyph);
        }
        assert!(a.height() > 2048);
        assert!(!a.exhausted());
        assert!(a.prepare_frame(1, 2048));
        assert!(a.height() <= 2048);
        assert_eq!(a.version(), 1);
    }

    #[test]
    fn generations_preserve_uv_until_boundary_and_recover_without_storm() {
        let budget = ByteBudget::new(2 * 64 * 64 * 4);
        let mut a = GlyphAtlas::with_budget(64, 64, 64 * 64 * 4, budget.clone()).unwrap();
        let other = GlyphAtlas::with_budget(64, 64, 64 * 64 * 4, budget.clone()).unwrap();
        let first = a.insert(&bmp(40, 40, 80));
        let revision = a.revision();
        assert_eq!(a.insert(&bmp(40, 40, 90)), a.fallback_rect());
        assert_eq!(a.revision(), revision);
        assert_eq!(tex_alpha(&a, first.u, first.v), 80);
        assert!(a.prepare_frame(101, 8192));
        assert_eq!(a.version(), 1);
        a.insert(&bmp(40, 40, 90));
        a.insert(&bmp(40, 40, 100));
        for _ in 0..30 {
            assert!(!a.prepare_frame(101, 8192));
        }
        assert_eq!(a.version(), 1);
        drop(other);
        assert!(
            !a.prepare_frame(101, 8192),
            "pane limit cannot recover by unrelated process release"
        );
        assert!(
            a.prepare_frame(102, 8192),
            "changed visible working set retries"
        );
        assert_ne!(a.insert(&bmp(20, 20, 100)), a.fallback_rect());
        assert_eq!(budget.used(), a.data.capacity());
        drop(a);
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn dirty_history_is_independent_for_consumers_and_bounded() {
        let mut a = GlyphAtlas::new(512, 512);
        a.insert(&bmp(6, 9, 1));
        let revision = a.revision();
        let r = a.insert(&bmp(3, 2, 2));
        assert_eq!(a.dirty_since(revision), Some(r));
        assert_eq!(
            a.dirty_since(revision),
            Some(r),
            "one renderer cannot clear another's dirty state"
        );
        assert_eq!(a.dirty_since(a.revision()), None);
        for n in 0..300u32 {
            let mut glyph = bmp(1, 1, 1);
            glyph.pixels.copy_from_slice(&n.to_le_bytes());
            a.insert(&glyph);
        }
        assert!(a.dirty.len() <= 256);
        assert_eq!(
            a.dirty_since(revision),
            Some(Rect {
                u: 0,
                v: 0,
                w: a.width(),
                h: a.height()
            })
        );
    }

    #[test]
    fn process_bitmap_budget_repeated_fill_drop_releases_exact_capacity() {
        let budget = ByteBudget::new(64 * 64 * 4);
        for _ in 0..20 {
            let a = GlyphAtlas::with_budget(64, 64, 64 * 64 * 4, budget.clone()).unwrap();
            assert_eq!(budget.used(), a.data.capacity());
            assert!(GlyphAtlas::with_budget(64, 64, 64 * 64 * 4, budget.clone()).is_err());
            drop(a);
            assert_eq!(budget.used(), 0);
        }
    }

    #[test]
    fn atlas_default_stops_at_device_safe_budget() {
        let mut atlas = GlyphAtlas::new(512, 512);
        let mut glyph = bmp(40, 40, 255);
        for n in 0..10000u32 {
            glyph.pixels[..4].copy_from_slice(&n.to_le_bytes());
            atlas.insert(&glyph);
        }
        assert!(
            atlas.height() <= 8192,
            "unbounded atlas height {}",
            atlas.height()
        );
        assert!(atlas.texture().len() <= 16 * 1024 * 1024);
    }

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
        let r2 = a.insert(&bmp(10, 10, 0xfe));
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
    fn glyph_wider_than_atlas_has_visible_fallback() {
        let mut a = GlyphAtlas::new(16, 16);
        let rect = a.insert(&bmp(40, 8, 3));
        assert_eq!(rect, a.fallback_rect());
        assert!(a.exhausted());
        assert!(a.texture().iter().any(|&v| v != 0));
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
