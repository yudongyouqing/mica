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
- `mica-render`:只认 grid 不认窗口。纯逻辑层(`color`/`frame` + `font/` 模块:样式与宽度判定、度量数学、字形图集、路由 trait,全部可 TDD)+ wgpu 管线(`pipeline.rs`)+ `#[cfg(windows)]` 的 DirectWrite 光栅化(`font/dwrite.rs`)。`CellInstance` 是 **64 字节** `#[repr(C)]`(pos_uv/size_uv/fg/bg 四个 vec4f,offset 0/16/32/48),与 WGSL 四个 location 锁死,有 size_of 测试兜底。**两遍发射契约**:`build_instances` 先产全部整格背景 quad、再产全部字形 quad(blend:None 下保证宽字形不被后续格背景擦除);空白格单实例(uv 尺寸 0 → ink_mask 0)。`GlyphRouter` trait 是"字符+样式→图集几何"的接缝,dwrite 与测试 Fake 各为其实现;图集**双计数**:`revision`(每次 insert/grow 递增)门控 `Renderer::set_atlas` 重传,`version`(仅 grow 重排递增)只做 `DwriteRouter` 清缓存信号——两者不可合并。**draw 前必须 set_atlas**(panic 契约)。dwrite 纹理类型必须匹配渲染模式:NATURAL_SYMMETRIC 配 `CLEARTYPE_3x1` 取 R 通道做灰度墨,配 `ALIASED_1x1` 则 bounds 恒空(黑屏,有回归测试)。
- `mica-app`:Win32 壳,保持最薄——窗口/输入路由/系统集成,不含终端逻辑。持 `DwriteRouter`(T7 已接线),窗口尺寸/网格换算由真度量驱动;WM_CHAR 重组 UTF-16 代理对(emoji/扩展表意文字才进得来)。

**数据流**:WM_CHAR/WM_KEYDOWN → `input::encode`(Ctrl+字母走 WM_CHAR 原始字节,不经 encode)→ pty 写;pty 读线程 → channel drain → `Surface::feed` → grid → `build_instances(surface, router, metrics)`(两遍发射;光标格与 SGR7 反色格都是 fg/bg 互换;WIDE_CHAR 天然双宽、SPACER 空白)→ `Renderer::draw`。

**度量单一来源**:格子尺寸由 `font::metrics::FontMetrics`(DirectWrite 设计单位换算)产出,流入 app 网格换算、`Surface::set_cell_metrics`(查询应答)与 blank 实例尺寸;8/16 硬编码常量已退役。调色板仍三处定义(surface 的 `default_rgb`、render 的 `BASE16[0]`、`frame::DEFAULT_BG`),计划 B 收敛为单一来源。

## 硬约束

- **许可证**:MIT OR Apache-2.0;**禁止引入 GPL 依赖**(商业资产自持,加依赖前查 license)。
- **配置语法**:兼容 Ghostty 的 `key = value` 语法是硬需求(spec D6)——主题生态靠它免费迁移。
- **wgpu surface 格式必须非 sRGB**:shader 颜色路径无 gamma 转换,`Renderer::new` 有 assert 兜底(选格式是壳层的责任)。
- **第三方 API 先核实再写**:wgpu/windows-rs/alacritty_terminal 都在快速变动,凭记忆写 API 必挂。写之前查 docs.rs 当前版本签名;`docs/superpowers/plans/` 里记录了 2026-09 已核实的签名与漂移点(wgpu 30 的 `Queue::present`、`CurrentSurfaceTexture` 枚举等)。

## 文档体系(权威顺序:spec > plan > 代码注释)

- `docs/superpowers/specs/2026-09-20-win-ghostty-design.md` — 设计文档,所有决策的依据(§2 决策记录含被否选项)。
- `docs/superpowers/plans/2026-09-20-m0-skeleton.md` — M0 实施计划(已完成,含复查附录)。
- `docs/superpowers/specs/2026-09-22-m1-render-config-design.md` — M1 设计(D8-D12 决策)。
- `docs/superpowers/plans/2026-09-22-m1-fonts-cjk.md` — M1-A 计划(字体与中文,T1-T7 完成、2026-09-22 真机冒烟通过;文末终审附录含 C1-I3 处方与留观清单);配置/主题/DECCKM/事件化/脏区为 M1-B(待写计划)。
- 里程碑:M0 骨架(✅)→ M1 渲染补全(A:字体与中文 ✅;B:配置与体验 待启动)→ M2 窗口体验 → M3 系统集成 → M4 协议 → M5 1.0。

## 当前边界(M1-A 后的声明欠账,不是 bug——修它们要过设计,别当 bug 顺手改)

- 半纹素渗色(终审 I2,处方:insert 右/下 +1px 零填充 padding;冒烟未观察到明显症状,留观)
- 快速拖动窗口有瞬态白块(DXGI 重建 backbuffer 未初始化内容 + 8ms 轮询丢帧窗口;M2 事件化渲染一并解决)
- 鼠标选择/复制(M2);WM_CHAR 无代理对之外的 IME 组合窗口(M3 系统级 IME)
- 不用 damage API,每帧全量重绘(计划 B 做脏区)
- DECCKM 应用光标模式未实现;Alt 修饰符前缀未实现(计划 B)
- 渲染循环是 8ms 轮询(计划 B 事件化)
- 调色板 OSC 4/12 查询全答白色;调色板三处定义(计划 B 收敛)
- 连字/COLR 彩色 emoji/光标样式变体(M2);bold-italic 复用 bold 面(M2);DPI 写死 96(spec §11)

## 提交规范

Conventional commits(`feat:`/`fix:`/`test:`/`docs:`/`chore:`),正文讲"为什么"。所有贡献者提交以本人身份署名。
