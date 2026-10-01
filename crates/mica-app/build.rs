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
    let mut dir: Vec<u8> = vec![0, 0, 1, 0, SIZES.len() as u8, 0]; // reserved, type=1(icon), count
    let mut images: Vec<Vec<u8>> = Vec::new();
    for (i, &sz) in SIZES.iter().enumerate() {
        let bmp = bmp_entry(sz);
        // ICONDIRENTRY: w,h,colorcount=0,reserved,planes=1,bpp=32,size,offset(稍后回填)
        let off = 6 + SIZES.len() * 16 + images.iter().map(|v: &Vec<u8>| v.len()).sum::<usize>();
        let b = sz.min(255) as u8; // 256 存 0
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
            (off & 0xff) as u8,
            ((off >> 8) & 08) as u8,
            ((off >> 16) & 0xff) as u8,
            ((off >> 24) & 0xff) as u8,
        ]);
        let _ = i;
        images.push(bmp);
    }
    // 修正:上面 entry 里 offset 高位错位(0x08 笔误),重写一遍干净实现
    let mut dir: Vec<u8> = vec![0, 0, 1, 0, SIZES.len() as u8, 0];
    let mut total = 6 + SIZES.len() * 16;
    for &sz in &SIZES {
        total += bmp_entry(sz).len();
    }
    let mut cur = 6 + SIZES.len() * 16;
    for &sz in &SIZES {
        let bmp = bmp_entry(sz);
        let b = sz.min(255) as u8;
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
    let _ = total;
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
    let row = (n * 4).max(((n + 31) / 32) * 4 * 1); // BGRA 行已 4 对齐;AND 每像素 1 bit
    let and_row = ((n + 31) / 32) * 4;
    let mut out = Vec::with_capacity(40 + row * n + and_row * n);
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
    out.extend(std::iter::repeat(0u8).take(and_row * n));
    out
}
