# Mica 设计文档

- 日期:2026-09-20
- 状态:设计已经所有者逐节确认
- 命名:**Mica** 于 2026-09-21 定稿(原工作名 winGhostty,已弃用);名字取自 Windows 11 招牌材质——本项目原生身份的直接宣示,渲染层真实使用(`DWMWA_SYSTEMBACKDROP_TYPE`)

## 1. 目标与定位

做一个 **Windows 原生终端模拟器**,对标 Ghostty 的三大支柱:**性能、功能、原生 UI**。

- 定位:开源社区项目 + 潜在商业化(open-core)
- 团队:独立开发者主导,AI 辅助 + 社区贡献
- 工期预期:以年计的长期项目,按里程碑分阶段交付
- 对标参照:上游 Ghostty 源码克隆于 `../ghostty`(只读参照)

### 参照目录约定

- 上游 Ghostty 克隆在 `../ghostty`(shallow clone),用途:**行为对比、协议语义参考、跑分对照**。
- Ghostty 为 MIT 许可,法律上允许在保留版权声明下复用代码;但本项目**默认只参考行为与协议语义,不搬代码**(语言不同也无法直搬;保持商业资产干净)。
- 协议行为的权威依据:Ghostty 的测试用例 + 相关协议规范(kitty 协议文档、VT 规范)。

### 非目标(Non-Goals)

- 不做 Electron/WebView 壳。
- 不做跨平台 UI 抽象层。Windows-first;核心层保持平台无关,壳层未来按需扩展。
- v1 不做 GUI 设置面板(Ghostty 同款哲学:配置文件驱动);M4 后按需求评估。
- v1 不做远程会话管理界面(通过 profile 支持 ssh 即可)。

## 2. 关键决策记录

| # | 决策 | 选择 | 被否选项与理由 |
|---|------|------|----------------|
| D1 | 语言 | **Rust** | 单人维护、内存安全、终端生态最厚(vte/portable-pty/wgpu) |
| D2 | UI 技术 | **纯 Rust + windows-rs 裸 Win32 + DWM** | 否决 WinUI 3:双语言维护是独立开发者的复利成本 + 40-80MB 磁盘/30-60MB 内存额外占用;否决 Electron:与"轻"人设冲突 |
| D3 | 核心来源 | **自建 Rust 核心 crate,站在 vte + alacritty_terminal 上** | 否决嵌入 libghostty:API 未稳定、Zig 0.x 变动频繁、上游战略绑架,与商业化冲突;否决从零自写状态机:VT 兼容长尾是十年坑,独立开发者最大杠杆是复用战果 |
| D4 | PTY | **portable-pty(ConPTY 封装)** | Windows 唯一正解,WezTerm 出品久经验证 |
| D5 | 渲染 | **wgpu(D3D12 后端,WARP 兜底)** | GPU 不可用自动降级,永不黑屏 |
| D6 | 配置语法 | **兼容 Ghostty `key = value` 语法** | Ghostty 主题生态即配置片段,语法兼容 = 主题库近乎免费迁移,开源传播钩子 |
| D7 | 许可 | **MIT OR Apache-2.0 双许可** | Rust 生态标准,商业友好;商业化走 open-core |

### 对标 Ghostty 必保的优点清单

1. 原生性能:GPU 渲染、SIMD 级解析吞吐、低输入延迟
2. 原生 UI:Win11 Mica/暗色/圆角、原生菜单、符合 Windows 交互习惯
3. 轻量:无 WebView、无 XAML 框架税,Alacritty 量级内存占用
4. 现代协议:kitty keyboard protocol、synchronized output、OSC 8/52、kitty graphics(预研)
5. 配置文件驱动 + 主题生态 + 热重载
6. Quick Terminal(quake 下拉)
7. 分屏 + 标签 + 快捷键体系
8. shell integration(OSC 133)
9. 崩溃报告仅本地、绝不上传(信任资产)
10. 连字(ligatures)支持

## 3. 总体架构

```
Mica/
├── docs/superpowers/           # 设计文档与实施计划
├── crates/
│   ├── mica-core/                # 终端核心:解析、状态机、PTY、配置、协议(纯 Rust,零 GUI 依赖)
│   │   ├── parser/             # VT 序列解析(vte 起步)
│   │   ├── term/               # 屏幕状态机(grid/scrollback/cursor/modes,alacritty_terminal 起步)
│   │   ├── pty/                # PTY 抽象(portable-pty/ConPTY 实现)
│   │   ├── config/             # Ghostty 语法配置解析 + 热重载
│   │   ├── protocol/           # kitty keyboard、OSC 52、2026、OSC 8 等增量协议
│   │   └── surface/            # Surface:一个终端实例 = Pty + Term + Config 的门面
│   ├── mica-render/              # 渲染器:wgpu 管线、字形图集、脏区跟踪(只认 grid,不认窗口)
│   │   ├── atlas/              # 字形图集管理
│   │   ├── pipeline/           # 着色器、脏区、呈现
│   │   └── font/               # DirectWrite 光栅化 + harfbuzz shaping
│   └── mica-app/                 # Windows 壳(bin)
│       ├── win32/              # 窗口、DWM、输入、剪贴板、IME
│       ├── ui/                 # 标签栏、分屏布局、快捷键、quick terminal
│       └── platform/           # 默认终端注册、jump list、右键菜单、单实例 IPC
```

三原则:

1. **核心是独立 crate**:不碰任何 Windows GUI。可单独 fuzz、可单独开源、未来可接其他壳(WinUI/Linux)。
2. **渲染器只认网格**:输入 `Grid + 样式`,输出三角形。不知道标签页/窗口的存在。
3. **壳层最薄**:只做窗口管理与输入路由,不含终端逻辑。

## 4. 核心层 mica-core

- **解析**:起步用 `vte` crate(SIMD、久经 fuzz)。自研 SIMD 解析器为远期优化项,前期 YAGNI。
- **状态机**:起步用 `alacritty_terminal` crate,全部访问收口在 `Surface` trait 后,保证未来可替换/可自研。注意 Alacritty **没有** kitty keyboard protocol——这是我们在 `protocol/` 的自有增量。
- **PTY**:`portable-pty`,藏于 `Pty` trait。ConPTY 要求 Windows 10 1809+。
- **配置**:Ghostty 同款 `key = value` 语法;`notify` crate 监听热重载;坏配置保留旧值继续运行。配置位置:`%APPDATA%\Mica\config`。
- **协议**(集中在 `protocol/`,按里程碑交付):kitty keyboard protocol(渐进增强 flags)、OSC 52 剪贴板、bracketed paste、DECSET 2026 同步输出、OSC 8 超链接、OSC 133 shell integration、主题变更通知(OSC 10/11 查询)。

## 5. 渲染层 mica-render

- wgpu,D3D12 为主后端;枚举不到合适适配器时降级 WARP 软渲染。
- 字形:DirectWrite 光栅化(原生 COLR 彩色 emoji);**连字用 harfbuzz**(M2 交付)。
- CJK 宽字符与字体回退链为一等公民(中文用户是核心用户群)。
- 性能:脏区跟踪只重绘变化行;输入到上屏延迟目标 <5ms;吞吐目标 `cat` 大文件不丢帧。
- 每个版本发布公开跑分(对照 Windows Terminal / Alacritty / WezTerm)——验收手段兼营销手段。

## 6. 壳层 mica-app(windows-rs 裸 Win32)

- **窗口**:Win32 创建;`DWMWA_SYSTEMBACKDROP_TYPE` 出 Mica;`DWMWA_USE_IMMERSIVE_DARK_MODE` 暗色标题栏;`WM_NCHITTEST` 自定义标题栏,标签栏绘于客户区顶部(Windows Terminal 同款布局)。Mica 在 Win10 优雅降级为纯色。
- **标签**:自绘 tab strip;每标签一个独立 `TerminalSurface`;**后台标签 PTY 持续收数据**,脏标记,激活时切渲染视口。
- **分屏**:布局树,每 pane 一个 surface;焦点路由;快捷键以 Ghostty 键位为蓝本做 Windows 适配。
- **Quick Terminal**:顶部下拉 quake 窗口,`RegisterHotKey` 全局热键,可失焦自动收起。
- **IME**:Imm32 全套,候选框跟随光标(中文输入一等公民)。
- **剪贴板**:`CF_UNICODETEXT` + OSC 52;右键菜单用 `TrackPopupMenu`(原生白给)。
- **系统集成**:Win11 默认终端注册(defterm)、WSL profile、jump list、"在此处打开"资源管理器右键菜单;单实例 + named pipe IPC(`mica new-tab` 在已有窗口开标签)。
- 无障碍(UIA)按里程碑后置(M5)。

## 7. 数据流

```
输入:Win32 消息(WM_KEYDOWN/WM_CHAR/IME)
      → 快捷键分发(用户绑定 → kitty 协议编码 → 传统编码兜底)
      → PTY 写

输出:PTY 读线程(每 surface 一条,阻塞读)
      → vte 解析 → grid 变更 + 脏标记
      → 唤醒渲染 → wgpu 绘制 → Present

配置:notify 监听 → 重解析 → 广播所有 surface → 图集失效重建 → 全量重绘

resize:WM_SIZE → 字体度量算新网格尺寸 → ConPTY resize → 渲染缓冲重分配
```

## 8. 错误处理

- **核心**:解析器绝不 panic(fuzz 持续覆盖);PTY 进程退出 → surface 进入 exited 态,显示退出码与重开按钮(Ghostty 行为)。
- **渲染**:GPU device lost → 自动重建设备;字形图集满 → 增量重建;适配器移除 → 降级 WARP。
- **配置**:解析失败 → 弹错误提示但保留旧配置继续运行,绝不因坏配置杀死终端。
- **崩溃**:panic hook + 本地 minidump + 本地崩溃报告文件,**永不上传**(Ghostty "never auto-upload" 哲学)。

## 9. 测试策略

- **核心**:vttest 对齐;金样本测试(录制真实程序输出字节流 → 回放断言网格状态);`cargo-fuzz` 解析器;ConPTY 集成测试(真 shell 跑脚本断言最终网格)。
- **渲染**:WARP 软件适配器出帧 → PNG golden diff;字体回退 case 全覆盖。
- **壳**:Win32 层薄而少测;快捷键映射、分屏布局树为纯逻辑,常规单测。
- **CI**:GitHub Actions `windows-latest` 全流程;性能基准(cells/s 吞吐 + 输入延迟)进 CI 防退化。

## 10. 里程碑

| 阶段 | 内容 | 验收标准 |
|------|------|----------|
| **M0 骨架** | workspace + CI、mica-core 接 vte/alacritty_terminal/portable-pty、单窗单标签、wgpu 基础渲染、ConPTY 跑 PowerShell | 能日常敲命令 |
| **M1 渲染补全** | DirectWrite 字形图集、CJK 宽字符、truecolor、Ghostty 语法配置 + 主题 + 热重载 | 中文日常可用 |
| **M2 窗口体验** | 多标签、分屏、快捷键体系、右键菜单、OSC 52、连字 | 可替代 Windows Terminal 日常使用 |
| **M3 系统集成** | 默认终端注册、WSL profile、quick terminal、jump list、安装包(MSIX/NSIS)、品牌域名与图标(**名已定:Mica**) | 别人能装能用 |
| **M4 协议补全** | kitty keyboard、DECSET 2026、OSC 8、OSC 133 shell integration、kitty graphics 预研 | Claude Code 长输出/协议体验不输 mac Ghostty |
| **M5 1.0** | UIA 无障碍、公开跑分、文档/官网/社区基建 | 正式发布 |

依赖关系:M0→M1→M2 线性;M3 与 M4 可部分并行;M5 收尾。

## 11. 许可与商业化

- 许可:`MIT OR Apache-2.0` 双许可。
- 商业化空间(open-core):本体开源;未来团队配置同步、托管主题市场、企业支持等做付费。
- 代码资产自持:不嵌入 GPL 依赖(选型时逐一核查 license)。

## 12. 风险与缓解

| 风险 | 缓解 |
|------|------|
| Win32 UI 细节打磨耗时(独立开发者最大风险) | UI 面积刻意做小(配置文件驱动,无设置 GUI);标签栏/菜单交互按里程碑分批;布局树等纯逻辑先行 |
| ConPTY 怪癖(Win10 resize 全量重绘、行尾空格等) | 集成测试覆盖;Win11 特性优雅降级 |
| alacritty_terminal 依赖耦合 | 全部收口在 Surface trait 后,可替换 |
| wgpu/DX12 兼容性 | WARP 兜底;CI 覆盖硬件与 WARP 两套适配器 |
| 品牌商标风险(Ghostty 名称属上游) | ✅ 已解决:2026-09-21 定名 Mica(域名/图标 M3 前补齐) |
| 单人带宽 | 里程碑切小;核心依赖成熟 crate;AI 辅助开发;文档从 M0 起就写 |

## 13. 开放问题

1. ~~**品牌名**~~ 已解决:定名 **Mica**(2026-09-21,终端/dev-tool 领域查无撞车);域名与图标 M3 前补齐。
2. **Windows 10 支持深度**:建议 1809+ 尽量支持,Win11 专属特性(Mica 材质、圆角等)优雅降级——待 M1 实测后定稿。
3. **ARM64 Windows**:Rust/ConPTY 均无障碍,列入 M3 安装包矩阵,优先级待定。
4. **kitty graphics 协议实现深度**:M4 预研后定范围(完整图片协议 vs 基础内联图)。
