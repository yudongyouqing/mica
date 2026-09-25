# Mica

**A native Windows terminal emulator, built in Rust.** Fast like the tools you envy on other platforms, native like the ones you already use.

> Mica [ˈmaɪkə] — named after Windows 11's signature material, which the window really is made of (`DWMWA_SYSTEMBACKDROP_TYPE`). Native down to the name.

[English intro below中文]

## 这是什么

Mica 是一个 **Windows 原生**终端模拟器,目标是把 Ghostty 在 macOS 上验证过的三大支柱——**性能、功能、原生 UI**——完整带到 Windows:

- **性能**:Rust + wgpu(D3D12)GPU 渲染,ConPTY 直连,无 Electron/无 XAML 框架税
- **功能**:现代终端协议(kitty keyboard、synchronized output、OSC 8/52)、分屏、标签、Quick Terminal、Ghostty 语法兼容的配置与主题生态
- **原生**:裸 Win32 + DWM——Mica 材质、暗色标题栏、系统级 IME(中文输入一等公民)

受到 [Ghostty](https://github.com/ghostty-org/ghostty) 的架构哲学启发(核心库 + 平台原生壳),与 Ghostty 项目**无隶属关系**;仅参考其行为与协议语义,不复用其代码。

## What is this

Mica is a **Windows-native** terminal emulator aiming to bring the three pillars Ghostty proved on macOS — **speed, features, native UI** — to Windows: Rust + wgpu (D3D12) rendering, ConPTY, bare Win32 + DWM chrome (real Mica backdrop, dark titlebar, first-class IME), and Ghostty-compatible `key = value` config syntax. Inspired by Ghostty's architecture; not affiliated with it.

## 状态 / Status

**M1(渲染补全)完成** — DirectWrite 字形管线、CJK 宽字符、630 主题库 + 热重载配置、调色板/OSC 应答收敛、DECCKM、事件驱动渲染与行级脏区,真机验收通过(2026-09-25),双平台 CI 全绿。下一个里程碑:M2 窗口体验(多标签、分屏、连字)。

| 里程碑 | 内容 | 状态 |
|---|---|---|
| M0 骨架 | 三 crate 架构 + 核心层 + 渲染管线 + Win32 壳 + CI | ✅ |
| M1 渲染补全 | DirectWrite 字形、CJK 宽字符、配置与主题 | ✅ |
| M2 窗口体验 | 多标签、分屏、快捷键体系 | |
| M3 系统集成 | 默认终端注册、WSL、Quick Terminal、安装包 | |
| M4 协议补全 | kitty keyboard、OSC 8/133、图形协议预研 | |
| M5 1.0 | 无障碍、公开跑分、正式发布 | |

## 构建 / Build

```
# Windows 10 1809+ / Windows 11, Rust stable
cargo run --bin mica

# 核心与渲染层跨平台,任意 OS 可测
cargo test -p mica-core -p mica-render
```

## 架构 / Architecture

| crate | 职责 |
|---|---|
| `mica-core` | 终端状态机(alacritty_terminal)+ PTY(portable-pty/ConPTY)+ 键位编码,零 GUI 依赖 |
| `mica-render` | wgpu 渲染:字形图集、颜色解析、grid → GPU instances |
| `mica-app` | Win32 窗口壳:消息循环、输入路由、DWM、系统集成 |

设计文档与实施计划见 `docs/superpowers/`。

## 许可 / License

`MIT OR Apache-2.0` 双许可,见 [LICENSE-MIT](LICENSE-MIT) 与 [LICENSE-APACHE](LICENSE-APACHE)。
