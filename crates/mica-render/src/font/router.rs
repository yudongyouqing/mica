//! 字形路由:字符+样式 → 图集几何。DirectWrite 实现见 dwrite.rs;
//! 测试用 Fake(见 frame::tests)。

use super::GlyphStyle;

/// 一个已就位的字形:uv 为图集纹素坐标,offset 相对格左上。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GlyphInfo {
    pub uv: [f32; 4],
    pub size_px: [f32; 2],
    pub offset_px: [f32; 2],
    /// 彩色字形(COLR,M2b/T3):位图带原色,渲染实例 fg 必须填白让原色
    /// 直通(shader: mix(bg, tex.rgb * fg, tex.a))。灰度字形恒 false。
    pub color: bool,
}

/// shaping 的逐 cluster 布局(见 GlyphRouter::route_run)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClusterLayout {
    /// 该 cluster 吞掉的字符数
    pub len: usize,
    /// Some = 合成字形(连字);None = 常规字符(len 必为 1)
    pub glyph: Option<GlyphInfo>,
}

pub trait GlyphRouter {
    /// 整段 shaping(M2b/T4 连字):同 style 的连续字符段一次性整形,
    /// 返回逐 cluster 布局(长度恒等于 chars;sum(len) == chars.len())。
    /// `len == 1 且 glyph == None` = 常规字符,调用方走逐字路由(缓存路径);
    /// `len >= 2 且 Some` = 连字 cluster,合成字形由首格承载、宽为 cluster 总宽。
    /// 返回 None(外层)= 不支持 shaping,整段逐字。
    fn route_run(
        &mut self,
        _chars: &[char],
        _style: super::GlyphStyle,
    ) -> Option<Vec<ClusterLayout>> {
        None
    }

    /// 路由一个字符+样式到图集条目;同 (char, style) 必须命中缓存同值。
    /// 空格与未知字符由实现者决定(通常路由到空白或 .notdef)。
    fn route(&mut self, ch: char, style: GlyphStyle) -> GlyphInfo;
}

/// Byte-accounted LRU. Oversized values are used by the caller but never retained.
/// Capacity is charged conservatively by callers, including key/value allocations.
pub(crate) struct BoundedCache<K, V> {
    entries: std::collections::HashMap<K, (V, u64, usize)>,
    tick: u64,
    limit: usize,
    max_entries: usize,
    permit: super::atlas::BytePermit,
}

impl<K: Eq + std::hash::Hash, V> BoundedCache<K, V> {
    pub(crate) fn new(
        limit: usize,
        max_entries: usize,
        budget: std::sync::Arc<super::atlas::ByteBudget>,
    ) -> Self {
        Self {
            entries: std::collections::HashMap::new(),
            tick: 0,
            limit,
            max_entries,
            permit: budget.reserve(0).expect("zero-byte reservation"),
        }
    }
    pub(crate) fn get(&mut self, key: &K) -> Option<&V> {
        self.tick = self.tick.wrapping_add(1);
        let (value, tick, _) = self.entries.get_mut(key)?;
        *tick = self.tick;
        Some(value)
    }
    pub(crate) fn insert(&mut self, key: K, value: V, bytes: usize) {
        if bytes > self.limit || self.max_entries == 0 {
            return;
        }
        if let Some((_, _, old_bytes)) = self.entries.remove(&key) {
            self.permit.shrink(old_bytes);
        }
        while self.entries.len() >= self.max_entries || self.permit.bytes() + bytes > self.limit {
            self.evict_oldest();
        }
        while !self.permit.grow(bytes) {
            if self.entries.is_empty() {
                return;
            }
            self.evict_oldest();
        }
        self.tick = self.tick.wrapping_add(1);
        self.entries.insert(key, (value, self.tick, bytes));
    }
    fn evict_oldest(&mut self) {
        let oldest = self.entries.values().map(|(_, tick, _)| *tick).min();
        if let Some(oldest) = oldest {
            self.entries.retain(|_, (_, tick, bytes)| {
                if *tick == oldest {
                    self.permit.shrink(*bytes);
                    false
                } else {
                    true
                }
            });
            if self.entries.capacity() > self.entries.len().saturating_mul(2) {
                self.entries.shrink_to_fit();
            }
        }
    }
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.entries.shrink_to_fit();
        self.permit.shrink(self.permit.bytes());
    }
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;
    use crate::font::atlas::ByteBudget;
    #[test]
    fn lru_hits_protect_hot_keys_and_process_quota_is_returned() {
        let budget = ByteBudget::new(1024);
        let mut cache = BoundedCache::new(1024, 2, budget.clone());
        cache.insert(1, "hot", 512);
        cache.insert(2, "cold", 512);
        assert_eq!(cache.get(&1), Some(&"hot"));
        cache.insert(3, "new", 512);
        assert_eq!(cache.get(&2), None);
        assert_eq!(cache.get(&1), Some(&"hot"));
        assert!(budget.used() <= 1024);
        let mut other = BoundedCache::new(1024, 2, budget.clone());
        other.insert(4, "uncached", 512);
        assert_eq!(other.len(), 0);
        cache.clear();
        assert_eq!(budget.used(), 0);
        other.insert(4, "retained", 512);
        assert_eq!(other.get(&4), Some(&"retained"));
        drop(other);
        assert_eq!(budget.used(), 0);
    }
    #[test]
    fn unique_large_values_obey_bytes_even_below_entry_limit() {
        let budget = ByteBudget::new(4096);
        let mut cache = BoundedCache::new(4096, 1024, budget.clone());
        for key in 0..100 {
            cache.insert(key, vec![key; 700], 1212);
        }
        assert!(cache.len() <= 3);
        assert!(budget.used() <= 4096);
        cache.insert(200, vec![0; 9000], 9512);
        assert_eq!(cache.get(&200), None);
    }
}
