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

**1.0 发布** — 协议补全(kitty keyboard 四 flag、OSC 8/133、DECSET 2026)、健壮性(崩溃本地 minidump、GPU 丢失自动重建、进程退出态与一键重开、per-monitor DPI v2、IME 候选窗跟随)、最小 UIA 无障碍(Narrator 可读)、渲染吞吐基准进 CI 防退化,真机验收通过(2026-10-03)。

| 里程碑 | 内容 | 状态 |
| 里程碑 | 内容 | 状态 |
|---|---|---|
| M0 骨架 | 三 crate 架构 + 核心层 + 渲染管线 + Win32 壳 + CI | ✅ |
| M1 渲染补全 | DirectWrite 字形、CJK 宽字符、配置与主题 | ✅ |
| M4 协议补全 | kitty keyboard、OSC 8/133、DECSET 2026、graphics 预研 | ✅ |
| M5 1.0 | 打磨、健壮性、发布(崩溃档案/DPI v2/UIA/winget) | ✅ |
| M2 窗口体验 | 多标签、分屏、快捷键体系 | ✅ |
| M3 系统集成 | 默认终端注册、WSL、Quick Terminal、安装包 | ✅ |
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

## 安装 / Install

- **winget**(推荐):`winget install mica`(1.0 清单投递后可用)
- **MSI**:GitHub Releases 的 `mica.msi`(CI 产出,未签名——SmartScreen
  提示选"仍要运行";个人项目暂不购代码签名证书,详见边界说明)
- **源码**:`cargo build --release`(需 Rust stable + Windows)

## 崩溃档案

崩溃时自动在 `%LOCALAPPDATA%\mica\crashes\` 落 minidump(`.dmp`)+
panic 详情(`.txt`),弹窗提示路径。**永不上传**——发 issue 时可自行
决定是否附上 dmp 与对应版本 exe/pdb。

## 已知边界(如实陈述)

ConPTY 在 Windows 侧消费以下协议序列,终端侧管线正确但收不到源头
(直连 pty 场景即生效;边界移动由金丝雀测试看守):

- DECSET 2026(同步输出)—— ConPTY 自带缓冲兜底
- OSC 133(shell integration 标记,jump/退出码点 UI 已备)
- kitty graphics 的 APC 载荷(kitty graphics 不实现,预研文档在库)
- OSC 52(剪贴板透传,疑被拦截,tmux 复验在票)

OSC 8 超链接是唯一实证透传的例外:下划线样式、Ctrl+Click 打开、
悬停显示 URL 均可用。
