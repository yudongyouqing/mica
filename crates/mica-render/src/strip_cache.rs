//! One chrome entry per window. Atlas appends retain pixel UVs.
use crate::{
    font::{metrics::FontMetrics, router::GlyphRouter},
    frame::{CellInstance, StripExtras, StripMetrics, strip_quads},
};
use mica_core::config::palette::Palette;

#[derive(Default)]
pub struct StripCache {
    entry: Option<Entry>,
    animation_quads: Vec<CellInstance>,
}
struct Entry {
    titles: Option<Vec<(String, bool)>>,
    extras: StripExtras,
    metrics: FontMetrics,
    palette: Palette,
    strip: StripMetrics,
    atlas: (u64, u64),
    quads: Vec<CellInstance>,
}
impl StripCache {
    pub fn clear(&mut self) {
        self.entry = None;
        self.animation_quads.clear();
    }

    #[allow(clippy::too_many_arguments)]
    pub fn get(
        &mut self,
        titles: &[(String, bool)],
        router: &mut dyn GlyphRouter,
        metrics: &FontMetrics,
        palette: &Palette,
        strip: &StripMetrics,
        extras: StripExtras,
        atlas: (u64, u64),
    ) -> &[CellInstance] {
        // Continuously changing geometry cannot reuse a previous frame.
        if (0.0 < extras.focus && extras.focus < 1.0)
            || extras.capsule_dx != 0.0
            || extras.drag.is_some()
            || extras.tab_dx.iter().any(|x| *x != 0.0)
        {
            self.animation_quads = strip_quads(titles, router, metrics, palette, strip, extras);
            return &self.animation_quads;
        }
        let hit = self.entry.as_ref().is_some_and(|e| {
            e.titles.as_deref() == Some(titles)
                && e.extras == extras
                && e.metrics == *metrics
                && e.palette == *palette
                && e.strip == *strip
                && e.atlas == atlas
        });
        if !hit {
            let quads = strip_quads(titles, router, metrics, palette, strip, extras.clone());
            // Keep only bounded copies of untrusted OSC titles. Oversized
            // inputs render normally and deliberately miss on the next frame.
            let titles = if titles.iter().map(|(s, _)| s.len()).sum::<usize>() <= 64 * 1024 {
                Some(titles.to_vec())
            } else {
                None
            };
            self.entry = Some(Entry {
                titles,
                extras,
                metrics: *metrics,
                palette: *palette,
                strip: *strip,
                atlas,
                quads,
            });
        }
        &self.entry.as_ref().expect("entry built above").quads
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::font::{GlyphStyle, router::GlyphInfo};
    struct Router(usize);
    impl GlyphRouter for Router {
        fn route(&mut self, _: char, _: GlyphStyle) -> GlyphInfo {
            self.0 += 1;
            GlyphInfo {
                uv: [1.0, 1.0, 4.0, 8.0],
                size_px: [4.0, 8.0],
                offset_px: [0.0, 0.0],
                color: false,
            }
        }
    }
    #[test]
    fn unchanged_chrome_reuses_quads_and_all_visual_inputs_invalidate() {
        let mut cache = StripCache::default();
        let mut router = Router(0);
        let mut titles = vec![("PowerShell".into(), true)];
        let mut sm = StripMetrics::default();
        let mut p = Palette::DEFAULT;
        let mut m = FontMetrics {
            cell_width: 8.0,
            line_height: 16.0,
            ascent: 13.0,
            descent: 3.0,
        };
        let mut e = StripExtras {
            focus: 1.0,
            caption_x: Some(500.0),
            ..Default::default()
        };
        let mut atlas = (1, 0);
        let original = cache
            .get(&titles, &mut router, &m, &p, &sm, e.clone(), atlas)
            .to_vec();
        let calls = router.0;
        assert_eq!(
            cache.get(&titles, &mut router, &m, &p, &sm, e.clone(), atlas),
            original
        );
        assert_eq!(router.0, calls);
        for step in 0..9 {
            match step {
                0 => titles[0].0.push('!'),
                1 => e.hover.plus = true,
                2 => e.focus = 0.5,
                3 => e.caption_x = Some(600.0),
                4 => sm.title_h += 1.0,
                5 => p.fg.r = 123,
                6 => m.line_height += 1.0,
                7 => atlas.0 += 1,
                _ => atlas.1 += 1,
            }
            let before = router.0;
            let cached = cache
                .get(&titles, &mut router, &m, &p, &sm, e.clone(), atlas)
                .to_vec();
            assert!(router.0 > before, "step {step} must rebuild");
            assert_eq!(
                cached,
                strip_quads(&titles, &mut router, &m, &p, &sm, e.clone())
            );
        }
        cache.clear();
        let before = router.0;
        cache.get(&titles, &mut router, &m, &p, &sm, e, atlas);
        assert!(router.0 > before);

        let long = vec![("界".repeat(25_000), true)];
        let before = router.0;
        cache.get(
            &long,
            &mut router,
            &m,
            &p,
            &sm,
            StripExtras::default(),
            atlas,
        );
        assert!(
            router.0 - before < 1000,
            "invisible title suffixes must not be routed"
        );
        assert!(cache.entry.as_ref().unwrap().titles.is_none());
        assert_eq!(
            cache.get(&[], &mut router, &m, &p, &sm, StripExtras::default(), atlas),
            strip_quads(&[], &mut Router(0), &m, &p, &sm, StripExtras::default())
        );
    }
}
