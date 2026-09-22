# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## 项目

**Mica** — Windows 原生终端模拟器(Rust),对标 Ghostty 三支柱:性能、功能、原生 UI。Windows-first;`mica-core`/`mica-render` 跨平台可测(开发主循环在 mac 上跑),`mica-app` 仅 Windows 运行(非 Windows 编译出 stub)。

**命名纪律**:项目名是 **Mica**,绝不以 Ghostty 名义呈现。上游 Ghostty 是只读参照(浅克隆约定在 `../ghostty`,不存在也不克隆即用):**只参考行为与协议语义,绝不复制代码**。

## 常用命令

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings   # CI 同款,-D warnings 必须零输出
cargo test --workspace                                  # 全量
cargo test -p mica-core poisoned                        # 单个测试(按名过滤)
cargo test -p mica-core pty                             # 模块过滤(注意 'pty' 会误中 surface 的 proxy 测试)
cargo run --bin mica                                    # 运行(仅 Windows 有真窗口;mac 打印 stub)
```

- pty 测试起**真实子进程**(mac 上 sh/cmd,Windows 上 ConPTY),首个 ConPTY 测试冷启动可达 30s——是预算不是卡死。
- **在 mac 上交叉 lint Windows 专属代码**(不用等 CI):`cargo clippy -p mica-app --target x86_64-pc-windows-msvc --all-targets`(check 不链接,无需 MSVC 工具链)。
- ConPTY 测试的隐藏契约:cmd/powershell 启动即发 `ESC[6n`(光标查询)并**阻塞等回包**——测试必须替终端应答 `ESC[1;1R`(见 `pty.rs` 的 `read_until` 与 `tests/powershell.rs`)。
- CI:GitHub Actions 双平台(Windows 全量,macOS 仅 core+render),位于 `.github/workflows/ci.yml`。

## 架构(依赖方向严格单向:mica-app → mica-render → mica-core)

**三 crate 边界是本项目最重要的纪律**(spec §3):

- `mica-core`:终端核心,**零 GUI 依赖**(Cargo.toml 里不得出现任何窗口/渲染库)。`surface.rs` 包装 alacritty_terminal(`vte::ansi::Processor::advance` 喂 `Term`,`Term` 实现的是 `Handler` 不是 `Perform`);`EventProxy` 收集应答类事件(DSR/OSC 回写、标题、尺寸查询),由 `take_pty_writes`/`take_title` 排空——**必须排空并写回 pty,否则 shell 会卡在等应答**。`pty.rs` 是 portable-pty 封装(读线程→mpsc;Drop 杀子并 reap)。`input.rs` 纯函数 xterm 键位编码。
- `mica-render`:只认 grid 不认窗口。纯逻辑层(`color`/`atlas`/`frame`,全部可 TDD)+ wgpu 管线(`pipeline.rs`)。`CellInstance` 是 48 字节 `#[repr(C)]`,与 WGSL 三个 vec4f location 锁死,有 size_of 测试兜底。
- `mica-app`:Win32 壳,保持最薄——窗口/输入路由/系统集成,不含终端逻辑。

**数据流**:WM_CHAR/WM_KEYDOWN → `input::encode`(Ctrl+字母走 WM_CHAR 原始字节,不经 encode)→ pty 写;pty 读线程 → channel drain → `Surface::feed` → grid → `build_instances`(每格一个 instance;光标格与 SGR7 反色格都是 fg/bg 互换)→ `Renderer::draw`。

**跨 crate 重复常量**:格子的 8/16 度量在 `mica-core/src/surface.rs`(u16)与 `mica-render/src/atlas.rs`(u32)各有一份,atlas.rs 里的 `const _: () = assert!` 锁死等值——改一处必改另一处。调色板目前三处定义(surface 的 `default_rgb`、render 的 `BASE16[0]`、`frame::DEFAULT_BG`),M1 收敛为单一来源。

## 硬约束

- **许可证**:MIT OR Apache-2.0;**禁止引入 GPL 依赖**(商业资产自持,加依赖前查 license)。
- **配置语法**:兼容 Ghostty 的 `key = value` 语法是硬需求(spec D6)——主题生态靠它免费迁移。
- **wgpu surface 格式必须非 sRGB**:shader 颜色路径无 gamma 转换,`Renderer::new` 有 assert 兜底(选格式是壳层的责任)。
- **第三方 API 先核实再写**:wgpu/windows-rs/alacritty_terminal 都在快速变动,凭记忆写 API 必挂。写之前查 docs.rs 当前版本签名;`docs/superpowers/plans/` 里记录了 2026-09 已核实的签名与漂移点(wgpu 30 的 `Queue::present`、`CurrentSurfaceTexture` 枚举等)。

## 文档体系(权威顺序:spec > plan > 代码注释)

- `docs/superpowers/specs/2026-09-20-win-ghostty-design.md` — 设计文档,所有决策的依据(§2 决策记录含被否选项)。
- `docs/superpowers/plans/2026-09-20-m0-skeleton.md` — M0 实施计划,**Task 7-8(Win32 壳 + 验收)待在 Windows 真机执行**,内含"执行者必读"注记与复查附录(已修/留观清单)。
- 里程碑:M0 骨架(已完成)→ M1 渲染补全(DirectWrite/CJK/配置)→ M2 窗口体验 → M3 系统集成 → M4 协议 → M5 1.0。

## M0 已知边界(是声明的欠账,不是 bug——修它们要过设计,别当 bug 顺手改)

- CJK 字形显示 `?`(M1 DirectWrite 解决)
- 不用 damage API,每帧全量重绘(M1 起做脏区)
- cell flags 除 INVERSE 外忽略(BOLD/WIDE 等);DECCKM 应用光标模式未实现(M1)
- 渲染循环是 8ms 轮询(M1 事件化)
- 调色板 OSC 4/12 查询全答白色(M1 调色板工作)

## 提交规范

Conventional commits(`feat:`/`fix:`/`test:`/`docs:`/`chore:`),正文讲"为什么"。所有贡献者提交以本人身份署名。
