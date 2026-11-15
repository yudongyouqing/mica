//! Background bitmap decoding. Decoder budgets are separate from GPU atlas budgets.

use image::{DynamicImage, ImageDecoder, ImageReader, RgbaImage};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

#[derive(Clone, Copy)]
struct ImageBudget {
    encoded: u64,
    pixels: u64,
    decoded: u64,
    side: u32,
}

const DEFAULT: ImageBudget = ImageBudget {
    encoded: 64 * 1024 * 1024,
    pixels: 64 * 1024 * 1024,
    decoded: 256 * 1024 * 1024,
    side: 32_768,
};

pub(crate) fn load(path: &Path) -> Result<RgbaImage, String> {
    load_with_budget(path, DEFAULT)
}

fn load_with_budget(path: &Path, budget: ImageBudget) -> Result<RgbaImage, String> {
    let error = |e: &dyn std::fmt::Display| format!("背景图 {}: {e}", path.display());
    let file = File::open(path).map_err(|e| error(&e))?;
    let metadata = file.metadata().map_err(|e| error(&e))?;
    if !metadata.is_file() || metadata.len() > budget.encoded {
        return Err(error(&"文件不是普通文件或超过编码文件大小预算"));
    }
    let mut reader = ImageReader::new(BufReader::new(file))
        .with_guessed_format()
        .map_err(|e| error(&e))?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(budget.side);
    limits.max_image_height = Some(budget.side);
    limits.max_alloc = Some(budget.decoded);
    reader.limits(limits.clone());
    let mut decoder = reader.into_decoder().map_err(|e| error(&e))?;
    let (width, height) = decoder.dimensions();
    // Check both original format bytes (including 16-bit formats) and RGBA8
    // conversion before allocating decoded pixels. The codec's internal
    // max_alloc is best-effort, not a bound on all resize/driver memory.
    let pixels = u64::from(width) * u64::from(height);
    if width == 0
        || height == 0
        || pixels > budget.pixels
        || decoder.total_bytes() > budget.decoded
        || pixels.saturating_mul(4) > budget.decoded
    {
        return Err(error(&"超过像素数或解码输出字节预算"));
    }
    // Match ImageReader::decode's accounting: reserve the output before
    // giving the remaining best-effort allocation allowance to the codec.
    limits
        .reserve(decoder.total_bytes())
        .map_err(|e| error(&e))?;
    decoder.set_limits(limits).map_err(|e| error(&e))?;
    let img = DynamicImage::from_decoder(decoder).map_err(|e| error(&e))?;
    let (w, h) = (img.width(), img.height());
    let scale = 4096.0 / w.max(h) as f32;
    Ok(if scale < 1.0 {
        image::imageops::resize(
            &img,
            ((w as f32 * scale) as u32).max(1),
            ((h as f32 * scale) as u32).max(1),
            image::imageops::FilterType::Lanczos3,
        )
    } else {
        img.into_rgba8()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Fixture(PathBuf);
    impl Fixture {
        fn png(w: u32, h: u32) -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "mica-background-{}-{}-{}.png",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            RgbaImage::from_pixel(w, h, image::Rgba([12, 34, 56, 255]))
                .save(&path)
                .unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn normal_png_preserves_pixels() {
        let input = Fixture::png(2, 2);
        let image = load(&input.0).unwrap();
        assert_eq!(image.dimensions(), (2, 2));
        assert_eq!(image.get_pixel(0, 0).0, [12, 34, 56, 255]);
    }

    #[test]
    fn encoded_budget_rejects_a_valid_image() {
        let input = Fixture::png(2, 2);
        assert!(
            load_with_budget(
                &input.0,
                ImageBudget {
                    encoded: 8,
                    ..DEFAULT
                }
            )
            .is_err()
        );
    }

    #[test]
    fn pixel_budget_rejects_a_valid_image() {
        let input = Fixture::png(2, 2);
        assert!(
            load_with_budget(
                &input.0,
                ImageBudget {
                    pixels: 3,
                    ..DEFAULT
                }
            )
            .is_err()
        );
    }

    #[test]
    fn decoded_bytes_budget_rejects_a_valid_image() {
        let input = Fixture::png(2, 2);
        assert!(
            load_with_budget(
                &input.0,
                ImageBudget {
                    decoded: 8,
                    ..DEFAULT
                }
            )
            .is_err()
        );
    }

    #[test]
    fn side_budget_rejects_a_valid_image() {
        let input = Fixture::png(2, 2);
        assert!(load_with_budget(&input.0, ImageBudget { side: 1, ..DEFAULT }).is_err());
    }

    #[test]
    fn extreme_aspect_ratio_never_produces_a_zero_sized_texture() {
        let input = Fixture::png(1, 8192);
        assert_eq!(load(&input.0).unwrap().dimensions(), (1, 4096));
    }
}
