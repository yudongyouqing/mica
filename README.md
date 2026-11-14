# Mica

**Windows 原生终端模拟器,Rust 打造。** 快得像你别平台羡慕的那些工具,原生得像你每天在用的那些。

![screenshot](docs/screenshot.png)

> Mica [ˈmaɪkə] —— 得名于 Windows 11 的招牌材质,而窗口真的用它
> (`DWMWA_SYSTEMBACKDROP_TYPE`)。原生到名字里。

[English intro below 中文]

## 这是什么

Mica 是一个 **Windows 原生**终端模拟器,目标是把 Ghostty 在 macOS 上
验证过的三大支柱——**性能、功能、原生 UI**——完整带到 Windows:

- **性能**:Rust + wgpu(D3D12)GPU 渲染,ConPTY 直连,无 Electron /
  无 XAML 框架税;渲染吞吐有基准与 CI 门禁防退化
- **功能**:现代终端协议(kitty keyboard、synchronized output、
  OSC 8/52/133)、分屏、多标签、多窗口、Quick Terminal、
  Ghostty 语法兼容的配置与 630+ 主题生态
- **原生**:裸 Win32 + DWM——Mica 材质、暗色标题栏、系统级 IME
  (中文输入一等公民)、per-monitor DPI v2

受 [Ghostty](https://github.com/ghostty-org/ghostty) 架构哲学启发
(核心库 + 平台原生壳),与该项目**无隶属关系**;仅参考其行为与
协议语义,不复用其代码。

## What is this

A **Windows-native** terminal emulator bringing the three pillars Ghostty
proved on macOS — speed, features, native UI — to Windows: Rust + wgpu
(D3D12) rendering over ConPTY, bare Win32 + DWM chrome (real Mica
backdrop, dark titlebar, first-class IME, per-monitor DPI v2), modern
terminal protocols (kitty keyboard / synchronized output / OSC 8/52/133),
splits, tabs, multi-window and a Quick Terminal, with Ghostty-compatible
`key = value` config and its 630+ theme ecosystem. Inspired by Ghostty's
architecture; not affiliated with it.

## 窗口与标签 / Window & Tabs

- **双行标签栏**:第一行窗口标题 + 系统按钮,第二行标签 + `+`;
  单标签时标签行自动隐藏(上游 `hasTabBar` 同款)
- **标签拖拽换序**:按住标签横拖,手粘光标、邻位滑动让位、松手落定
  (挤压式,Chrome 语义)
- **tear-off**:把标签拖出标签行,撕成独立窗口(续同 shell / 工作目录)
- **多窗口**:Ctrl+Shift+N 随时开新窗,进程活到最后一窗
- **动画**:标签切换滑移、聚焦/失焦整条渐变洗白(时长可配,可关)
- **溢出滚动**:标签放不下时两端箭头 + 滚轮滚动,活跃标签自动跟随
- **Quick Terminal**:Win+\` 顶部下拉,失焦收起,exit 后横幅提示、
  唤出即重开
- **程序化驱动**:`mica send` 向任意标签 / Quick Terminal 注入输入
  (tmux `send-keys` 同义)——外部工具即可驱动跑在 Mica 里的
  Claude Code / Codex 等 agent

## 常用键位(Windows Terminal 兼容 + 浏览器直觉)

| 键 | 动作 |
|---|---|
| `Ctrl+T` / `Ctrl+Shift+T` | 新标签 |
| `Ctrl+Shift+W` | 关标签/分屏 |
| `Ctrl+Shift+1..9` | 跳标签 |
| `Ctrl+Tab` / `Ctrl+Shift+Tab` | 下一/上一标签 |
| `Ctrl+Shift+N` | 新窗口 |
| `Alt+方向` | 分屏间跳焦点 |
| `Ctrl+Shift+R` | 重开 shell(保留 cwd) |
| `Ctrl+Shift+↑/↓` | 上一/下一 prompt(OSC 133) |

## 程序化驱动 / Scripting

```bash
mica send --tab=0 "echo IPC_OK\r"      # 指定标签注入 + 回车
mica send --qt "claude 帮我看看这个"    # 注入 Quick Terminal
mica send --literal "C:\\raw\\path"     # 关闭转义(\r \e \)
```

pane 开了 bracketed paste 会自动包边界——多行 prompt 注入
coding agent 不会炸。连不上实例时报错退出,绝不误启新窗。

## 配置

用户配置位于 `%APPDATA%\mica\config`，采用 **Ghostty 的 `key = value` 语法**。
全部默认值和 34 条默认快捷键集中在
[`crates/mica-core/config/defaults`](crates/mica-core/config/defaults)，用户文件只需写覆盖项：

```ini
theme = Dracula
font-family = JetBrains Mono
font-size = 13

# 默认 shell(已安装 PowerShell 7 时)
command = pwsh.exe -NoLogo

# 标签栏外观全量可配(strip-* 键集)
strip-font-size = 9.5
strip-bar-wash = 0.18
strip-tab-height = 36
strip-animation-duration = 120
```

注释必须独占一行，值后不能追加注释。查看完整选项、配置位置和校验结果：

```powershell
mica +show-config --default
mica +show-config --path
mica +validate-config
```

默认值从同一份文件生成内置兜底，无配置文件也能启动；加载顺序为默认值、主题、用户覆盖。
不要把全部默认颜色直接粘成用户覆盖项，否则会盖住选中的主题；参考完整选项时可先注释掉。
`+validate-config` 校验语法和值范围，失败返回非零；未知键保持兼容性容忍，
不会校验 shell 程序、字体和图片文件是否已经安装或存在。

| 配置 | 生效范围 |
| --- | --- |
| 普通窗口字体、调色板、背景、鼠标行为、快捷键、标题栏、光标闪烁 | 保存后热重载 |
| `command` | 新标签、分屏、新窗口；已有会话保持原 shell |
| `window-width` / `window-height` / `window-position` | 新建窗口；宽高单位为终端列、行 |
| `window-size-pixels = 宽,高` | 新窗口的精确物理像素外框；优先于列/行配置，置空恢复列/行 |
| `window-padding-x` / `window-padding-y` | 逻辑像素、随 DPI 缩放，保存后热重载 |
| `window-title-font-family` / `strip-font-family` | 同一项 UI 字体配置，默认 Segoe UI，独立于终端字体；热重载 |
| `strip-title-align` | `center`（整窗居中）或 `left`，长标题省略并避开按钮 |
| `strip-title-highlight` / `-color` / `-opacity` / `-radius` | 标题圆角底的开关、颜色、强度与半径，支持热重载 |
| `quick-terminal-key` | 热键可重绑或设 `none` 禁用 |
| Quick Terminal 的 shell、字体、配色、`quick-terminal-height` | 完全退出 Mica 后重启 |

首次创建配置文件后需重启以开启监听；坏配置热重载会保留当前设置。

例如按现有窗口的物理像素大小启动，并设置居中标题：

```ini
window-size-pixels = 2032,1041
window-title-font-family = Segoe UI
strip-title-align = center
strip-title-highlight = true
```

像素尺寸是 Mica 的扩展，不随 DPI 换算；未设置时仍采用列/行配置。
标题底色默认从主题强调色推导，`strip-title-highlight-color = auto` 恢复自动配色。

`command` 用于之后新建的窗口、标签、分屏和 Quick Terminal。显式选择
`--profile` 时使用该 profile；空 `command =` 恢复 Windows PowerShell 默认值。
保存后已有 shell 会继续运行，Quick Terminal 要完全退出 Mica 后重开才会换 shell。
若桌面启动的 PATH 与命令行不同，使用带引号的绝对路径，例如：

```ini
command = "C:\Program Files\PowerShell\7\pwsh.exe" -NoLogo
```

这沿用 [Ghostty 的 `command` 配置键](https://ghostty.org/docs/config/reference#command)
及“用于新终端”的范围；Windows 命令按 CRT 引号规则直接执行，不展开 `~`、
环境变量、管道或 `shell:`/`direct:` 前缀。PowerShell 初始化脚本放在其 `$PROFILE`
中，需加载 Oh My Posh 时请保留 profile 加载，不加 `-NoProfile`。

## 状态 / Status

**1.1 已发布**(tag [v1.1.0](https://github.com/yudongyouqing/mica/releases/tag/v1.1.0),
Release 附 MSI):标签栏重制、多窗口、tear-off、拖拽换序、动画、
QT 闭环、strip-* 全量可配、`mica send` 程序化驱动。

| 里程碑 | 内容 | 状态 |
|---|---|---|
| M0 骨架 | 三 crate 架构 + 核心层 + 渲染管线 + Win32 壳 + CI | ✅ |
| M1 渲染与配置 | DirectWrite 字形、CJK、配置与主题生态 | ✅ |
| M2 窗口体验 | 多标签、分屏、选择、快捷键体系 | ✅ |
| M3 系统集成 | Profile/IPC/WSL、Quick Terminal、安装包 | ✅ |
| M4 协议 | kitty keyboard、OSC 8/133、2026、graphics 预研 | ✅ |
| M5 1.0 | 健壮性(崩溃档案/DPI v2/IME)、发布 | ✅ |
| 1.1 打磨 | 标签栏/多窗口/tear-off/动画/全量可配/send | ✅ |

## 安装 / Install

- **MSI**:GitHub [Releases](https://github.com/yudongyouqing/mica/releases)
  的 `mica.msi`(CI 产出,未签名——SmartScreen 提示选"仍要运行";
  个人项目暂不购代码签名证书)
- **winget**:清单投递待做(#36)
- **源码**:`cargo build --release`(Rust stable + Windows;
  `cargo run --bin mica` 直接跑)

开发提示:`mica --multi` 起隔离实例(绕过单实例,便于多开调试)。

## 构建 / Build

```bash
cargo run --bin mica          # Windows:真窗口
cargo test -p mica-core -p mica-render   # 核心与渲染层跨平台可测
cargo clippy --workspace --all-targets -- -D warnings   # CI 同款
```

## 架构 / Architecture

依赖方向严格单向:`mica-app` → `mica-render` → `mica-core`。

| crate | 职责 |
|---|---|
| `mica-core` | 终端状态机(alacritty_terminal)+ PTY(ConPTY)+ 键位编码与配置合成,零 GUI 依赖 |
| `mica-render` | 纯逻辑层(颜色/帧/字形图集/路由)+ wgpu 管线 + DirectWrite 光栅化 |
| `mica-app` | Win32 壳:消息循环、输入路由、DWM、系统集成 |

设计文档与实施计划见 `docs/superpowers/`。

## 崩溃档案

崩溃时自动在 `%LOCALAPPDATA%\mica\crashes\` 落 minidump(`.dmp`)+
panic 详情(`.txt`),弹窗提示路径。**永不上传**——发 issue 时可自行
决定是否附上 dmp 与对应版本 exe/pdb。

## 已知边界(如实陈述)

ConPTY 会影响部分协议。以下是本机与 CI 探针的已知边界,
不代表所有 Windows 版本(直连 pty 场景与 ConPTY 也可能不同):

- DECSET 2026(同步输出)—— ConPTY 自带缓冲兜底
- OSC 133(shell integration):本机 build 26300 与当前 CI Server 2025
  build 26100 均实测完整透传。此前“26100 及以下剥离”的测试依据
  受输入回显早退影响,已撤回;未知旧版本需实际探测
- kitty graphics 的 APC 载荷(不实现,预研文档在库)
- OSC 52(剪贴板,疑被拦截,tmux 复验在票)

OSC 8 超链接是唯一各版本实证透传的例外:下划线样式、Ctrl+Click
打开、悬停显示 URL 均可用。

## 许可 / License

`MIT OR Apache-2.0` 双许可,见 [LICENSE-MIT](LICENSE-MIT) 与
[LICENSE-APACHE](LICENSE-APACHE)。
