//! Profile 体系(M3a,spec §3):可启动的 shell 集合 = 静态项 + WSL 枚举。
//!
//! 纯逻辑 + 一次性子进程 IO(`wsl.exe --list --all`);WSL 缺失/无发行版/
//! 解析失败一律静默降级为空列表,终端绝不因枚举失败报错。

use std::process::Command;

/// 一个可启动的终端 profile。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    /// 显示名(标签占位标题/jump list 项)
    pub name: String,
    /// 启动命令行(整串,交 pty)
    pub command: String,
}

fn powershell() -> Profile {
    Profile {
        name: "PowerShell".into(),
        command: "powershell.exe -NoLogo".into(),
    }
}

fn cmd() -> Profile {
    Profile {
        name: "Command Prompt".into(),
        command: "cmd.exe".into(),
    }
}

/// 静态项(任何 Windows 都有;顺序即 jump list 顺序)。
pub fn scan_static() -> Vec<Profile> {
    vec![powershell(), cmd()]
}

/// wsl.exe --list --all 的 stdout → 发行版名列表。
///
/// 真机实证(2026-10-01):输出 **UTF-16LE 无 BOM**;表头随系统语言本地化
/// (中文"适用于 Linux 的 Windows 子系统:");默认发行版标记是本地化后缀
/// "(默认)"而非文档宣称的 `*` 前缀(两者都容错);行尾 \r\n。
/// 解码启发:前 64 字节的奇数索引位置大量 0x00 → UTF-16LE,否则 UTF-8
/// (新 wsl 版本有 UTF-8 输出形态)。
pub fn parse_wsl_output(raw: &[u8]) -> Vec<String> {
    let text = decode_wsl_bytes(raw);
    text.lines().filter_map(parse_distrow_line).collect()
}

/// 按启发解码 wsl.exe 输出(UTF-16LE / UTF-8 双形态)。
fn decode_wsl_bytes(raw: &[u8]) -> String {
    let probe = &raw[..raw.len().min(64)];
    let zeros_odd = probe.iter().skip(1).step_by(2).filter(|&&b| b == 0).count();
    // BOM 直接判定(FF FE/FE FF);启发:奇数索引大量 0x00 = UTF-16LE
    let has_bom = raw.starts_with(&[0xff, 0xfe]) || raw.starts_with(&[0xfe, 0xff]);
    let likely_utf16 = has_bom || (!probe.is_empty() && zeros_odd * 2 >= probe.len() / 2);
    if likely_utf16 {
        // 去 BOM(FF FE);奇数长度容忍截断
        let raw = raw.strip_prefix(&[0xff, 0xfe]).unwrap_or(raw);
        let (pairs, _) = raw.as_chunks::<2>();
        let units: Vec<u16> = pairs.iter().map(|c| u16::from_le_bytes(*c)).collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(raw).into_owned()
    }
}

/// 单行 → 发行版名:跳过表头(含 "Linux" 的本地化标题行 / 冒号结尾行 /
/// "distribut"),剥 `* ` 前缀与 "(默认)/(Default)" 类后缀。
fn parse_distrow_line(line: &str) -> Option<String> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    // 表头:本地化标题总含 "Linux" 字样或以冒号结尾;英文表头含 distribut
    let lower = line.to_lowercase();
    if lower.contains("linux") || lower.contains("distribut") || line.ends_with(':') {
        return None;
    }
    let line = line.strip_prefix("* ").unwrap_or(line);
    // 默认后缀:剥最后一个括号段(仅当括号内像"默认/Default"标记——
    // 发行版名自身含括号的情况极罕见,且剥了也只是显示名少一段,无功能损失)
    let name = match (line.rfind('('), line.ends_with(')')) {
        (Some(i), true) => &line[..i],
        _ => line,
    }
    .trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// 全量扫描:静态项 + WSL(枚举失败/超时 → 静默跳过)。
pub fn scan_all() -> Vec<Profile> {
    let mut out = scan_static();
    for name in list_wsl_distributions() {
        out.push(Profile {
            name: name.clone(),
            command: format!("wsl.exe -d {name}"),
        });
    }
    out
}

/// 子进程枚举(3s 超时;任何失败 → 空)。CREATE_NO_WINDOW 由 pty 层管不到
/// 这里——CLI `mica list-profiles` 允许闪控制台,GUI 路径在 app 侧首帧前
/// 调用一次,窗口已存在不感知闪烁;记为已知瑕疵,M3c 打磨票。
pub fn list_wsl_distributions() -> Vec<String> {
    let output = Command::new("wsl.exe")
        .args(["--list", "--all"])
        .output()
        .ok()
        .filter(|o| o.status.success());
    let Some(output) = output else {
        return Vec::new();
    };
    parse_wsl_output(&output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真机字节(2026-10-01,中文系统):无 BOM、中文表头、"(默认)"后缀。
    const REAL_ZH: &[u8] = &[
        0x02, 0x90, 0x28, 0x75, 0x8e, 0x4e, 0x20, 0x00, 0x4c, 0x00, 0x69, 0x00, 0x6e, 0x00, 0x75,
        0x00, 0x78, 0x00, 0x20, 0x00, 0x84, 0x76, 0x20, 0x00, 0x57, 0x00, 0x69, 0x00, 0x6e, 0x00,
        0x64, 0x00, 0x6f, 0x00, 0x77, 0x00, 0x73, 0x00, 0x20, 0x00, 0x50, 0x5b, 0xfb, 0x7c, 0xdf,
        0x7e, 0x06, 0x52, 0xd1, 0x53, 0x3a, 0x00, 0x0d, 0x00, 0x0a, 0x00, 0x55, 0x00, 0x62, 0x00,
        0x75, 0x00, 0x6e, 0x00, 0x74, 0x00, 0x75, 0x00, 0x20, 0x00, 0x28, 0x00, 0xd8, 0x9e, 0xa4,
        0x8b, 0x3c, 0x50, 0x29, 0x00, 0x0d, 0x00, 0x0a, 0x00, 0x64, 0x00, 0x6f, 0x00, 0x63, 0x00,
        0x6b, 0x00, 0x65, 0x00, 0x72, 0x00, 0x2d, 0x00, 0x64, 0x00, 0x65, 0x00, 0x73, 0x00, 0x6b,
        0x00, 0x74, 0x00, 0x6f, 0x00, 0x70, 0x00, 0x0d, 0x00, 0x0a, 0x00,
    ];

    #[test]
    fn real_machine_chinese_output_parses() {
        assert_eq!(
            parse_wsl_output(REAL_ZH),
            vec!["Ubuntu".to_string(), "docker-desktop".to_string()],
            "中文表头跳过、(默认)后缀剥除"
        );
    }

    #[test]
    fn english_output_with_asterisk_default() {
        let text: Vec<u8> = "Windows Subsystem for Linux Distributions:\r\n\
             * Ubuntu-22.04\r\n\
             Debian\r\n"
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        assert_eq!(
            parse_wsl_output(&text),
            vec!["Ubuntu-22.04".to_string(), "Debian".to_string()],
            "* 前缀剥除"
        );
    }

    #[test]
    fn utf8_output_also_supported() {
        assert_eq!(
            parse_wsl_output(b"Linux Distributions:\r\nArch\r\n"),
            vec!["Arch".to_string()],
            "新 wsl 的 UTF-8 形态(无奇数位 NUL → 按UTF-8)"
        );
    }

    #[test]
    fn empty_and_garbage_degrade_silently() {
        assert_eq!(parse_wsl_output(&[]), Vec::<String>::new());
        assert_eq!(
            parse_wsl_output(b"\xff\xfe"),
            Vec::<String>::new(),
            "纯 BOM"
        );
        assert_eq!(
            parse_wsl_output(&[0x55, 0x00]), // 奇数截断的单 u16 "U"
            vec!["U".to_string()],
            "截断容忍"
        );
    }

    #[test]
    fn header_only_yields_empty() {
        let text: Vec<u8> = "Windows Subsystem for Linux Distributions:\r\n"
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        assert_eq!(parse_wsl_output(&text), Vec::<String>::new());
    }

    #[test]
    fn static_profiles_cover_both_shells() {
        let s = scan_static();
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].name, "PowerShell");
        assert!(s[1].command.starts_with("cmd.exe"));
    }

    #[test]
    fn scan_all_extends_static_with_wsl() {
        let all = scan_all();
        // 真机有 Ubuntu/docker-desktop;无 WSL 环境也能过(静态 2 项保底)
        assert!(all.len() >= 2);
        assert!(all.iter().any(|p| p.name == "PowerShell"));
    }
}
