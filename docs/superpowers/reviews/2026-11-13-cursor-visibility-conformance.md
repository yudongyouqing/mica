# 光标显隐：协议与成熟终端行为对照

核对日期：2026-11-13。关联 #116 / #117。

## 基线与来源

本次遵循 DECTCEM（DEC 私有模式 25）的通行 VT 兼容语义。它是 DEC 协议及成熟终端生态支持的扩展，不把它误称为 ECMA-48 基础命令。

| 来源 | 核对内容 | 链接 / 固定版本 |
| --- | --- | --- |
| DEC VT510 原始手册 | DECTCEM 控制显隐，默认可见 | [原始手册存档](https://vt100.net/mirror/mds-199909/cd3/term/vt510rmb.pdf) |
| Microsoft Windows Console 官方协议文档 | `CSI ? 25 l` 隐藏，`CSI ? 25 h` 显示；显隐和形状、闪烁分别定义 | [Cursor Visibility](https://learn.microsoft.com/en-us/windows/console/console-virtual-terminal-sequences#cursor-visibility) |
| Windows Terminal / Console 共享适配器 | DECTCEM 更新 cursor 的 visible 状态，查询模式读取该状态 | [adaptDispatch.cpp，533408607aa0c9cf1d9bf1f2dda6497bb073f80c](https://github.com/microsoft/terminal/blob/533408607aa0c9cf1d9bf1f2dda6497bb073f80c/src/terminal/adapter/adaptDispatch.cpp) |
| xterm.js 官方兼容表 | DECSET 25 显示、DECRST 25 隐藏；核对页面版本 6.0.0 | [Supported Terminal Sequences](https://xtermjs.org/docs/api/vtfeatures/) |
| 当前锁定的 alacritty_terminal 0.26.0 | 普通终端模式下 SHOW_CURSOR 清除时提供 Hidden 渲染形态 | [term/mod.rs，94e7c8874e526b1e67b349d9ba30ddf81669119e](https://github.com/alacritty/alacritty/blob/94e7c8874e526b1e67b349d9ba30ddf81669119e/alacritty_terminal/src/term/mod.rs) |

Alacritty 的固定 commit 来自本机锁定 crate 的 `.cargo_vcs_info.json`，不是猜测当前主分支。以上仅参考协议与行为。

## 本次对齐

| 行为 | Mica 现状 / 回归 |
| --- | --- |
| 默认光标可见 | Surface 状态回归 |
| 隐藏时三种形状都不绘制 | Block、Beam、Underline 全量与增量回归 |
| 隐藏时仍维护逻辑坐标、应答 DSR | 隐藏后移动到 4;7，DSR 仍返回 4;7 |
| 隐藏不改写已有形状 | Beam 在隐藏、重新显示后仍为 Beam |
| 隐藏期间移动不留下旧位置或新位置的可见光标 | 行缓存连续移动与清旧行回归 |
| show 恢复当前坐标和形状 | 同行显隐与跨行移动后的恢复回归 |
| 本地闪烁关闭相位保留 | 调用方 cursor=None 仍不绘制；闪烁唤醒不能覆盖应用隐藏状态 |
| ConPTY 到核心的状态传递 | 专用 PowerShell 以文件栅栏分开 hide/move/show，逐阶段验证 |

生产逻辑直接读取终端核心的 SHOW_CURSOR，两条 renderer 入口统一约束绘制；主窗与 Quick Terminal 均使用该公共行缓存路径。隐藏仅影响光标呈现，定位、文本和查询处理继续由终端核心执行。

修前两个渲染回归失败，修后通过。完整门禁与远端结果以 PR 检查记录为准。本次不据此宣称所有 VT 协议、全部形状/闪烁策略或所有 TUI 行为已经完成一致性认证。
