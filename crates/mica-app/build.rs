//! 构建脚本(M3c/D26):程序化生成占位图标(主题色底 + 白 M 方块,
//! 7 分辨率)写 `mica.ico`,winres 嵌入 exe 资源 + 版本元数据。
//! 非 Windows 构建直接跳过(图标无意义)。

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
    let ico_path = std::path::Path::new(&out_dir).join("mica.ico");
    write_placeholder_ico(&ico_path);

    // 仅当宿主也是 Windows 时嵌资源(winres 调 Windows 工具链;
    // mac 交叉 check 时跳过——图标只在真机构建时需要)
    if std::env::var("HOST")
        .map(|h| h.contains("windows"))
        .unwrap_or(false)
    {
        let mut res = winres::WindowsResource::new();
        // per-monitor DPI v2(M5b/T4):manifest 声明前 WM_DPICHANGED 根本
        // 不投递(进程 DPI-unaware 时被系统虚拟化,DPI 恒 96)
        res.set_manifest(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2, PerMonitor</dpiAwareness>
    </windowsSettings>
  </application>
</assembly>"#,
        );
        res.set_icon(ico_path.to_str().expect("path utf8"));
        res.set("FileDescription", "Mica — Windows-native terminal");
        res.set("ProductName", "Mica");
        res.set("LegalCopyright", "MIT OR Apache-2.0");
        if let Err(e) = res.compile() {
            // 旧 SDK 缺 rc.exe 时降级:无图标可接受,不让构建红
            println!("cargo:warning=winres 嵌资源失败(无图标构建): {e}");
        }
    }
    println!("cargo:rerun-if-changed=build.rs");
}

/// 纯字节 ICO:BMP(PNG 复杂;BGRA DIB 头方案,小尺寸可直接铺)。
/// 格式:ICONDIR + 每尺寸 ICONDIRENTRY + BITMAPINFOHEADER(高×2,含 AND 掩码)+ 像素。
fn write_placeholder_ico(path: &std::path::Path) {
    const SIZES: [u32; 7] = [16, 24, 32, 48, 64, 128, 256];
    // ICONDIR:reserved=0, type=1(icon), count;entry 含各尺寸的像素偏移
    let mut dir: Vec<u8> = vec![0, 0, 1, 0, SIZES.len() as u8, 0];
    let sizes: Vec<Vec<u8>> = SIZES.iter().map(|&sz| bmp_entry(sz)).collect();
    let mut cur = 6 + SIZES.len() * 16;
    for (&sz, bmp) in SIZES.iter().zip(&sizes) {
        let b = sz.min(255) as u8; // 256 按格式存 0
        dir.extend([
            b,
            b,
            0,
            0,
            1,
            0,
            32,
            0,
            (bmp.len() & 0xff) as u8,
            ((bmp.len() >> 8) & 0xff) as u8,
            ((bmp.len() >> 16) & 0xff) as u8,
            ((bmp.len() >> 24) & 0xff) as u8,
            (cur & 0xff) as u8,
            ((cur >> 8) & 0xff) as u8,
            ((cur >> 16) & 0xff) as u8,
            ((cur >> 24) & 0xff) as u8,
        ]);
        cur += bmp.len();
    }
    let mut ico = dir;
    for sz in SIZES {
        ico.extend(bmp_entry(sz));
    }
    std::fs::write(path, ico).expect("write ico");
}

/// 单尺寸 DIB:BITMAPINFOHEADER(height = 2*sz:像素 + AND 掩码行集)
/// + BGRA 像素(BMP 自底向上)+ 全零 AND 掩码(alpha 通道已承载透明)。
fn bmp_entry(sz: u32) -> Vec<u8> {
    let n = sz as usize;
    // BGRA 行天然 4 字节对齐;AND 掩码每像素 1 bit,按 32bit 打包行对齐
    let and_row = n.div_ceil(32) * 4;
    let mut out = Vec::with_capacity(40 + n * 4 * n + and_row * n);
    out.extend_from_slice(&40u32.to_le_bytes()); // biSize
    out.extend_from_slice(&sz.to_le_bytes()); // biWidth
    out.extend_from_slice(&(sz * 2).to_le_bytes()); // biHeight(双倍)
    out.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    out.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
    out.extend_from_slice(&0u32.to_le_bytes()); // biCompression(BI_RGB)
    out.extend_from_slice(&(0u32).to_le_bytes()); // biSizeImage(可 0)
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    // 像素(BGRA 自底向上):主题底 #1e1e1e,中央白 M 方块(约 60% 高)
    let bg: [u8; 4] = [0x1e, 0x1e, 0x1e, 0xff];
    let fg: [u8; 4] = [0xff, 0xff, 0xff, 0xff];
    let inset = n * 2 / 10;
    let arm = (n - inset * 2).max(1);
    let stem_w = (arm / 5).max(1);
    for y in (0..n).rev() {
        for x in 0..n {
            // M:左竖 + 左斜 + 右斜 + 右竖(以 (inset..n-inset) 为画布,vy 画布内 y 自顶向下)
            let vy = n - 1 - y; // 画布坐标(自顶向下)
            let cx = x.saturating_sub(inset);
            let cy = vy.saturating_sub(inset);
            let in_canvas = x >= inset && x < n - inset && vy >= inset && vy < n - inset;
            let px = if in_canvas {
                let diag_w = stem_w;
                let on_left = cx < stem_w;
                let on_right = cx >= arm - stem_w;
                let half = arm / 2;
                let on_l_diag = cx <= half && cy <= half && (half - cx) < diag_w && cy < half;
                let on_r_diag = cx > half && cy <= half && (cx - half) < diag_w && cy < half;
                if (on_left || on_right || on_l_diag || on_r_diag) && cy < arm {
                    fg
                } else {
                    bg
                }
            } else {
                bg
            };
            out.extend_from_slice(&px);
        }
    }
    // AND 掩码全零(alpha 负责)
    out.extend(std::iter::repeat_n(0u8, and_row * n));
    out
}
