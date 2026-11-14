//! Export actual DirectWrite + wgpu chrome without opening/focusing a window.
//! cargo run -p mica-render --example titlebar_preview -- output.png
#[cfg(not(windows))]
fn main() {
    eprintln!("DirectWrite preview requires Windows");
}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use mica_core::config::settings::resolve;
    use mica_render::{
        font::dwrite::DwriteRouter,
        frame::{StripExtras, StripMetrics, strip_quads},
        pipeline::{Renderer, create_context},
    };
    let output = std::env::args_os()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("output PNG path required"))?;
    let settings = resolve("theme = Dracula\nbackground = #0c0c0c")
        .map_err(|error| anyhow::anyhow!("{error:?}"))?;
    let scale = 1.5;
    let sm = StripMetrics::from_scale_cfg(scale, &settings.strip);
    let mut router = DwriteRouter::new(
        settings.strip.font_size_pt * scale,
        &[
            "Segoe UI",
            "Microsoft YaHei UI",
            "Segoe UI Emoji",
            "Segoe Fluent Icons",
        ],
    )?;
    let font = router.metrics();
    let (width, height) = (2032u32, 600u32);
    let rows = [
        (vec![("Mica — yudongyouqing".into(), true)], 1.0),
        (
            vec![
                ("PowerShell 7".into(), false),
                ("项目 / mica".into(), true),
                ("Claude".into(), false),
            ],
            1.0,
        ),
        (
            vec![(
                "C:\\Project folder\\项目\\mica\\长目录标题/".repeat(12),
                true,
            )],
            1.0,
        ),
        (vec![("Mica — yudongyouqing".into(), true)], 0.0),
    ];
    let mut quads = Vec::new();
    for (index, (titles, focus)) in rows.into_iter().enumerate() {
        let mut row = strip_quads(
            &titles,
            &mut router,
            &font,
            &settings.palette,
            &sm,
            StripExtras {
                caption_x: Some(width as f32 - 3.0 * sm.caption_w),
                focus,
                ..Default::default()
            },
        );
        for quad in &mut row {
            quad.pos_uv[1] += index as f32 * 150.0;
        }
        quads.extend(row);
    }
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let ctx = pollster::block_on(create_context(&instance, None))?;
    let mut renderer = Renderer::new(&ctx, wgpu::TextureFormat::Bgra8Unorm)?;
    renderer.set_clear_color(&settings.palette);
    let size = wgpu::Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
    };
    let texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("titlebar-preview"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Bgra8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    renderer.set_atlas(&router.atlas());
    renderer.begin_frame_offscreen(&view, (width, height));
    renderer.draw_batch(&quads, None);
    renderer.end_frame();
    let stride = (width * 4).div_ceil(256) * 256;
    let buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("titlebar-readback"),
        size: u64::from(stride) * u64::from(height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = ctx
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("titlebar-copy"),
        });
    encoder.copy_texture_to_buffer(
        texture.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(stride),
                rows_per_image: Some(height),
            },
        },
        size,
    );
    ctx.queue.submit([encoder.finish()]);
    let (tx, rx) = std::sync::mpsc::channel();
    buffer
        .slice(..)
        .map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
    ctx.device.poll(wgpu::PollType::Wait {
        submission_index: None,
        timeout: Some(std::time::Duration::from_secs(30)),
    })?;
    rx.recv_timeout(std::time::Duration::from_secs(30))??;
    let mapped = buffer.slice(..).get_mapped_range()?;
    let mut pixels = Vec::with_capacity((width * height * 4) as usize);
    for row in mapped.chunks_exact(stride as usize) {
        for pixel in row[..(width * 4) as usize].as_chunks::<4>().0 {
            pixels.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
        }
    }
    drop(mapped);
    buffer.unmap();
    image::save_buffer(output, &pixels, width, height, image::ColorType::Rgba8)?;
    Ok(())
}
