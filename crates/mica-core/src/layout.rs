//! 分屏布局树(M2b,spec §6):叶子 = pane,内部节点 = 二分。
//! 纯逻辑零窗口概念——矩形是 f32 抽象区,壳层负责像素换算与边框绘制。

/// 客户区矩形(抽象单位;壳层传入像素)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }
}

/// 分屏方向:Horizontal = 垂直分割线、左右并排;Vertical = 水平分割线、上下。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDir {
    Horizontal,
    Vertical,
}

/// 布局树。pane id 由壳层全局分配(与 tab id 空间共用单调计数亦可)。
#[derive(Debug, Clone, PartialEq)]
pub enum Layout {
    Leaf(u64),
    Split {
        dir: SplitDir,
        /// first 占比(0.1..=0.9,clamp 于此)
        ratio: f32,
        first: Box<Layout>,
        second: Box<Layout>,
    },
}

const RATIO_MIN: f32 = 0.1;
const RATIO_MAX: f32 = 0.9;
fn clamp_ratio(r: f32) -> f32 {
    r.clamp(RATIO_MIN, RATIO_MAX)
}

impl Layout {
    pub fn leaf(id: u64) -> Self {
        Self::Leaf(id)
    }

    pub fn panes(&self) -> Vec<u64> {
        let mut out = Vec::new();
        self.collect(&mut out);
        out
    }

    fn collect(&self, out: &mut Vec<u64>) {
        match self {
            Self::Leaf(id) => out.push(*id),
            Self::Split { first, second, .. } => {
                first.collect(out);
                second.collect(out);
            }
        }
    }

    /// 把 `target` 叶子替换为 Split{first=target 叶, second=新叶}。
    /// target 不存在 = no-op(壳层已保证存在,防御静默)。
    pub fn split(&mut self, target: u64, dir: SplitDir, new_id: u64) {
        match self {
            Self::Leaf(id) if *id == target => {
                *self = Self::Split {
                    dir,
                    ratio: 0.5,
                    first: Box::new(Self::Leaf(target)),
                    second: Box::new(Self::Leaf(new_id)),
                };
            }
            Self::Split { first, second, .. } => {
                first.split(target, dir, new_id);
                second.split(target, dir, new_id);
            }
            Self::Leaf(_) => {}
        }
    }

    /// 摘除 `target` 叶子;其父 Split 单子提升(塌缩)。返回是否摘到。
    pub fn remove(&mut self, target: u64) -> bool {
        match self {
            Self::Leaf(id) => {
                if *id == target {
                    // 根叶子由调用方处理(标签关闭),树内不该出现
                    false
                } else {
                    false
                }
            }
            Self::Split { first, second, .. } => {
                if matches!(**first, Self::Leaf(t) if t == target) {
                    *self = (**second).clone(); // second 顶上
                    true
                } else if matches!(**second, Self::Leaf(t) if t == target) {
                    *self = (**first).clone();
                    true
                } else {
                    first.remove(target) || second.remove(target)
                }
            }
        }
    }

    /// 递归二分求每 pane 的矩形。
    pub fn rects(&self, area: Rect) -> Vec<(u64, Rect)> {
        let mut out = Vec::new();
        self.rects_into(area, &mut out);
        out
    }

    fn rects_into(&self, area: Rect, out: &mut Vec<(u64, Rect)>) {
        match self {
            Self::Leaf(id) => out.push((*id, area)),
            Self::Split {
                dir,
                ratio,
                first,
                second,
            } => {
                let r = clamp_ratio(*ratio);
                match dir {
                    SplitDir::Horizontal => {
                        let (w1, w2) = (area.w * r, area.w * (1.0 - r));
                        first.rects_into(Rect::new(area.x, area.y, w1, area.h), out);
                        second.rects_into(Rect::new(area.x + w1, area.y, w2, area.h), out);
                    }
                    SplitDir::Vertical => {
                        let (h1, h2) = (area.h * r, area.h * (1.0 - r));
                        first.rects_into(Rect::new(area.x, area.y, area.w, h1), out);
                        second.rects_into(Rect::new(area.x, area.y + h1, area.w, h2), out);
                    }
                }
            }
        }
    }

    /// 调整包含 `target` 的最近 Split 的 ratio。
    /// delta 语义:正 = target 所在侧变**大**——target 在 first 则 ratio+delta,
    /// 在 second 则 ratio-delta(方向语义对用户稳定)。
    pub fn resize(&mut self, target: u64, delta: f32) {
        let _ = self.resize_inner(target, delta);
    }

    /// 正 delta = target 所在侧变大:递归进包含 target 的子树,更深节点
    /// 优先调整;到叶子的路径上的最近 Split 才动手。
    fn resize_inner(&mut self, target: u64, delta: f32) -> bool {
        match self {
            Self::Leaf(_) => false,
            Self::Split {
                ratio,
                first,
                second,
                ..
            } => {
                if first.contains(target) {
                    if first.resize_inner(target, delta) {
                        return true;
                    }
                    *ratio = clamp_ratio(*ratio + delta);
                    true
                } else if second.contains(target) {
                    if second.resize_inner(target, delta) {
                        return true;
                    }
                    *ratio = clamp_ratio(*ratio - delta);
                    true
                } else {
                    false
                }
            }
        }
    }

    fn contains(&self, id: u64) -> bool {
        match self {
            Self::Leaf(t) => *t == id,
            Self::Split { first, second, .. } => first.contains(id) || second.contains(id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: Rect = Rect::new(0.0, 0.0, 100.0, 80.0);

    #[test]
    fn single_leaf_gets_full_area() {
        let l = Layout::leaf(1);
        assert_eq!(l.rects(A), vec![(1, A)]);
        assert_eq!(l.panes(), vec![1]);
    }

    #[test]
    fn split_halves_without_overlap() {
        let mut l = Layout::leaf(1);
        l.split(1, SplitDir::Horizontal, 2);
        let r = l.rects(A);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0], (1, Rect::new(0.0, 0.0, 50.0, 80.0)));
        assert_eq!(r[1], (2, Rect::new(50.0, 0.0, 50.0, 80.0)));
        // 面积守恒
        let total: f32 = r.iter().map(|(_, rc)| rc.w * rc.h).sum();
        assert!((total - 100.0 * 80.0).abs() < 1e-3);
    }

    #[test]
    fn vertical_split_stacks() {
        let mut l = Layout::leaf(1);
        l.split(1, SplitDir::Vertical, 2);
        let r = l.rects(A);
        assert_eq!(r[0], (1, Rect::new(0.0, 0.0, 100.0, 40.0)));
        assert_eq!(r[1], (2, Rect::new(0.0, 40.0, 100.0, 40.0)));
    }

    #[test]
    fn nested_split_keeps_conservation() {
        let mut l = Layout::leaf(1);
        l.split(1, SplitDir::Horizontal, 2);
        l.split(2, SplitDir::Vertical, 3);
        let r = l.rects(A);
        assert_eq!(r.len(), 3);
        let total: f32 = r.iter().map(|(_, rc)| rc.w * rc.h).sum();
        assert!((total - 8000.0).abs() < 1e-2);
        // 先序:1, 2, 3
        assert_eq!(l.panes(), vec![1, 2, 3]);
    }

    #[test]
    fn remove_collapses_parent() {
        let mut l = Layout::leaf(1);
        l.split(1, SplitDir::Horizontal, 2);
        assert!(l.remove(1));
        assert_eq!(l, Layout::leaf(2), "second 顶上,父塌缩");
        assert!(!l.remove(2), "根叶由调用方处理");
    }

    #[test]
    fn remove_nested_promotes_sibling() {
        let mut l = Layout::leaf(1);
        l.split(1, SplitDir::Horizontal, 2);
        l.split(2, SplitDir::Vertical, 3);
        assert!(l.remove(3));
        let r = l.rects(A);
        assert_eq!(r.len(), 2, "塌缩后回到二分");
        assert!((r[0].1.w - 50.0).abs() < 1e-3);
    }

    #[test]
    fn ratio_clamped() {
        let mut l = Layout::leaf(1);
        l.split(1, SplitDir::Horizontal, 2);
        l.resize(1, 5.0); // target 在 first:正 delta 放大 first
        let r = l.rects(A);
        assert!((r[0].1.w - 90.0).abs() < 1e-3, "clamp 到 0.9");
        assert!((r[1].1.w - 10.0).abs() < 1e-3);
        l.resize(1, -99.0);
        let r = l.rects(A);
        assert!((r[0].1.w - 10.0).abs() < 1e-3, "clamp 到 0.1");
    }

    #[test]
    fn resize_targets_own_side_semantics() {
        let mut l = Layout::leaf(1);
        l.split(1, SplitDir::Horizontal, 2);
        // target=2(在 second):正 delta 放大 second = ratio 减小
        l.resize(2, 0.3);
        let r = l.rects(A);
        assert!((r[1].1.w - 80.0).abs() < 1e-3, "second 放大到 80");
        assert!((r[0].1.w - 20.0).abs() < 1e-3);
    }

    #[test]
    fn resize_nested_targets_nearest_split() {
        // 1 | (2 / 3):调 target=3 放大 3 → 只动内层,外层不动
        let mut l = Layout::leaf(1);
        l.split(1, SplitDir::Horizontal, 2);
        l.split(2, SplitDir::Vertical, 3);
        l.resize(3, 0.2);
        let r = l.rects(A);
        assert!((r[0].1.w - 50.0).abs() < 1e-3, "外层不动");
        assert!((r[2].1.h - 0.7 * 80.0).abs() < 1e-3, "内层 3 占七成");
        assert!((r[1].1.h - 0.3 * 80.0).abs() < 1e-3);
    }

    #[test]
    fn split_missing_target_is_noop() {
        let mut l = Layout::leaf(1);
        l.split(99, SplitDir::Horizontal, 2);
        assert_eq!(l, Layout::leaf(1));
    }
}
