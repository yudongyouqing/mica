# M2b 分屏·连字·COLR·OSC52·光标·标题栏 Implementation Plan

- 日期:2026-09-27
- 状态:待执行
- 上游:spec `2026-09-26-m2-tabs-splits-design.md`(§6 分屏/§7 连字 D16/§8 COLR D19/§9 OSC52+光标 D20/§10 标题栏)
- 前置:M2a 已合入 dev(PR #5);M2b 键位占位(SplitRight/SplitDown/ClosePane)已在 Keymap
- 验收锚点(spec §11 M2b):vim 里 `=>` `!=` 连字;😀 彩色;tmux OSC 52 远程复制;分屏双 vim 对照;Win11 Mica 材质

## Global Constraints

- fmt/clippy `-D warnings` 零输出;workspace 测试全绿;交叉 lint
- **API 已核实(本地源码)**:
  - `Term::cursor_style() -> CursorStyle`(term/mod.rs:942;DECSCUSR 落 cursor_style,未设走 default)——`CursorStyle` 枚举经 `vte::ansi` 导出(Block/Underline/Beam,带 Deprecated 混合变体)
  - `Event::ClipboardStore(ClipboardType, String)` / `ClipboardLoad(ClipboardType, Arc<dyn Fn(&str)->String>)`(event.rs:25/31)——OSC 52 两向,app 侧接 Win32 剪贴板(M2a clipboard.rs 现成)
  - `IDWriteFactory::CreateTextAnalyzer() -> IDWriteTextAnalyzer`(mod.rs:2218)
  - `IDWriteTextAnalyzer::GetGlyphs/GetGlyphPlacements`(mod.rs:9503/9512)——**textstring 是 PCWSTR:shaping 输入必须 char→u16 码位**,cluster map 产 glyph 映射;13+ 参数,执行时以编译器为准
  - `IDWriteFactory4::TranslateColorGlyphRun(基线原点, run, ..., DWRITE_GLYPH_IMAGE_FORMATS, ...)` → `IDWriteColorGlyphRunEnumerator1`(mod.rs:2986);factory 升级用 `.cast::<IDWriteFactory4>()`(interface_hierarchy 支持)
  - 自绘标题栏:WM_NCCALCSIZE/WM_NCHITTEST/`DWMWA_SYSTEMBACKDROP_TYPE`(Win11 22H2+,失败降级纯色)——签名执行时核
- **RGBA 图集迁移(T2)是 T3/T4 的地基,一次到位不做双轨**(spec D19);全量渲染对拍 + ink 回归锁迁移
- 行为红线:RGBA 迁移不改变灰度字形观感(coverage 进 alpha、RGB 白);分屏不破坏两遍发射与 damage;标题栏失败可回退系统标题栏(单 commit 粒度)

---

### Task 1: 布局树(纯逻辑,TDD)

**Files:** New `crates/mica-core/src/layout.rs`;Modify lib.rs

- [ ] **Step 1: 类型与操作**

```rust
pub enum Layout { Leaf(u64 /*pane id*/), Split { dir: SplitDir, ratio: f32, first: Box<Layout>, second: Box<Layout> } }
pub enum SplitDir { Horizontal /*左右并排*/, Vertical /*上下*/ }

impl Layout {
    pub fn leaf(id: u64) -> Self;
    pub fn split(&mut self, target: u64, dir: SplitDir, new_id: u64); // target 被 second 替换,Split{first=target 原叶子, second=新}
    pub fn remove(&mut self, target: u64) -> bool; // 叶子摘除后父节点塌缩(单子节点提升)
    pub fn panes(&self) -> Vec<u64>; // 先序遍历叶子
    pub fn rects(&self, area: Rect /*x,y,w,h f32*/) -> Vec<(u64, Rect)>; // 递归二分
    pub fn resize(&mut self, target: u64, dir: SplitDir, delta: f32); // 含 target 的边比例调整(简化:沿树找路径上的最近 Split 调 ratio±delta)
}
```

ratio 约束 0.1..0.9;`SplitDir::Horizontal` = 第一/第二左右排(垂直分割线)。

- [ ] **Step 2: 测试**(表驱动):单叶 rects 全区;split 后两区面积和 = 全区、无重叠;嵌套 split;remove 中间叶子后塌缩(rects 恢复二分);resize 边界 clamp;panes 先序稳定
- [ ] **Step 3: Commit** `feat(core): pane layout tree with split/remove/resize`

### Task 2: RGBA 图集迁移(一次到位)

**Files:** `crates/mica-render/src/font/atlas.rs`、`pipeline.rs`、`cells.wgsl`、`font/router.rs`

- [ ] **Step 1: GlyphBitmap 增格式**——`pub enum GlyphFormat { Coverage /*R8 灰度*/, ColorRgba /*预乘 RGBA*/ }`;atlas 数据 R8→RGBA(内存 4x:1024² = 4MB,接受);`blit` 按格式写入(RGBA 直接拷 4 字节;Coverage 写 RGB=255、A=coverage——**预乘注意**:shader 不变的前提下,灰度字形的 RGB 白 × a = ink 乘色 ✓;彩色字形位图按 DirectWrite 输出(非预乘)→ 存储时预乘(RGB *= A)防白边)
- [ ] **Step 2: 纹理格式**——`R8Unorm` → `Rgba8Unorm`;pipeline debug_assert(256 对齐)保留;纹理尺寸上限 2048(emoji 图集增长快)
- [ ] **Step 3: shader**——`fs`:`let t = textureSample(...).rgba; ink = t.a; color = t.rgb * fg;`——灰度字形 t.rgb=白×fg ✓ 彩色字形需要 fg=白:渲染实例对彩色字形 fg 填白(路由层标记 `GlyphInfo { color: bool }`),`color = mix(bg, ink * fg_color, ...)` 语义微调:统一 `t.rgb * fg`(彩色 fg=白 → 原色;灰度 fg=字色 → 染色)。ink_mask 与 uv 尺寸判定不变
- [ ] **Step 4: 迁移回归**——现有全量测试改期望(atlas len ×4、纹理格式断言);黄金对拍不变(实例序列不变);dwrite 光栅化测试(inked glyphs)迁移后仍绿 = 灰度路径无回归
- [ ] **Step 5: Commit** `feat(render): RGBA atlas — one migration, coverage and color in one texture`

### Task 3: COLR 彩色 emoji

**Files:** `crates/mica-render/src/font/dwrite.rs`

- [ ] **Step 1: 彩色探测与光栅化**——rasterize 里 `face` 拿到后:`factory.cast::<IDWriteFactory4>()` → `TranslateColorGlyphRun(DWRITE_GLYPH_IMAGE_FORMATS_COLOREB... /*COLR*/)`;Ok(枚举器)→ 逐层 `GetCurrentRun1` → 每层跑 `IDWriteGlyphRunAnalysis`(同灰度路径,但彩色层产 RGBA 位图——CreateAlphaTexture 只有 alpha;彩色需要 **IDWriteGlyphRunAnalysis 不支持彩色位图**,改用枚举层上 **`IDWriteColorGlyphRunEnumerator1::GetCurrentRun1` 的 run + 调色板色直接填充**?——**执行时核**:彩色路径实际可用 `TranslateColorGlyphRun` 层级的 `paletteIndex` 取色 + 每层再 CreateAlphaTexture(mask)按色染色合成 RGBA。计划锁定:彩色 = 多层 alpha mask × 层色合成,不改 CreateAlphaTexture 依赖
- [ ] **Step 2: GlyphInfo { color: bool }**;彩色路径产 `GlyphBitmap { format: ColorRgba, .. }`;簇大小照 bounds
- [ ] **Step 3: 测试**(windows 真机):route('😀') 产 color 位图、非零像素、format 标记;route('A') 仍 Coverage
- [ ] **Step 4: 真机**:`echo 😀` 彩色;混排(彩色旁灰度)无白边(预乘验证)
- [ ] **Step 5: Commit** `feat(font): COLR color emoji via layered glyph runs`

### Task 4: 连字(DWrite shaping,D16)

**Files:** `crates/mica-render/src/font/dwrite.rs`、`font/router.rs`、`frame.rs`

- [ ] **Step 1: route_run**——`GlyphRouter` trait 增默认方法 `route_run(&mut self, chars: &[char], style) -> Option<Vec<GlyphInfo>>`(None = 不支持 shaping,回退逐字);dwrite 实现:`analyzer.GetGlyphs(chars→u16 序列, face, ...)` + `GetGlyphPlacements` → 连字合成 glyph(cluster map 多对一)+ advances;产**一个** GlyphInfo(合成字形,宽 = 序列总 advance,首个 cluster 的字形 id);Fake router 返回 None
- [ ] **Step 2: build_rows 行内扫描**——同 style 连续非空白段:若 route_run Some 且产出 glyph 数 < 字符数(发生了连字)→ 段首格承载合成字形,余格 spacer 语义(空 bg 已由每格 bg quad 兜底);None → 现路径。**段变化即整行 rebuild(damage 行级天然覆盖)**
- [ ] **Step 3: 字体探测**——DwriteRouter::new 对 "=>","!=",">=" 试 shape,全不连字则 `ligatures: false` 全程走旧路径(行为不劣化)
- [ ] **Step 4: 测试**(windows):Cascadia(支持连字)route_run("=>") 产 1 glyph 宽≈2格;探测开关;"中" 混排不误伤;对拍:无连字字体输出与旧路径逐字节一致
- [ ] **Step 5: 真机**:pwsh 里 `echo 'a => b != c'` 连字成形
- [ ] **Step 6: Commit** `feat(font): ligature shaping via DWrite analyzer with fallback probe`

### Task 5: OSC 52 剪贴板协议

**Files:** `crates/mica-core/src/surface.rs`、`crates/mica-app/src/app.rs`

- [ ] **Step 1: EventProxy 接事件**——`Event::ClipboardStore(ClipboardType::Clipboard, text)` → 暂存 `clipboard_out: Vec<String>`(take 排空,app 写系统剪贴板);`ClipboardLoad(_, fmt)` → `pty_writes.push(fmt(&待回填文本))`——**回填文本从哪来**:Load 语义是"终端请求读剪贴板"(tmux 读远端剪贴板)→ 需要 app 提供当前剪贴板内容:ProxyState 增 `clipboard_in: Option<String>`(app 每次 WM_APP_RENDER 排空前若上轮有 Load 需求则先注入?简化:**app 在写 pty 循环外维护**——EventProxy 的 Load 用回调闭包!`fmt: Arc<dyn Fn(&str)->String>` 接收**当前文本**返回应答串:闭包不能借 app 状态……**落地方案**:surface 新增 `set_clipboard_provider(Arc<dyn Fn()->String>)`(app 注册读 Win32 剪贴板的闭包),ClipboardLoad 时调 provider 取文本再 fmt → 应答入 pty_writes。**执行时若 Arc 回调与 EventProxy Clone 冲突,退化为 out/in 双缓冲轮询**
- [ ] **Step 2: app 接线**——排空 pty_writes 时:ClipboardStore 文本 → `clipboard::set_text`(M2a 现成)
- [ ] **Step 3: 长度护栏**——Store 上限 100KB(spec §9,防炸);超限静默丢弃 + eprintln
- [ ] **Step 4: 测试**——core:驱动 ClipboardStore 事件 → take_pty_writes/clipboard_out 可取;Load 事件 + provider → 应答串正确(apple 测试);app:真机 tmux/printf OSC52 序列往返
- [ ] **Step 5: Commit** `feat(core): OSC 52 clipboard write-through with size guard`

### Task 6: 光标样式(D20)

**Files:** `crates/mica-core/src/surface.rs`、`crates/mica-render/src/frame.rs`、`crates/mica-app/src/app.rs`

- [ ] **Step 1: Surface 透出**——`pub fn cursor_style(&self) -> CursorStyle { self.term.cursor_style() }`(上游现成,DECSCUSR 已落库)
- [ ] **Step 2: 渲染形状**——build_rows 光标判定处按 style:Block=现状(交换);Underline=底部 2px 高亮条 quad(fg 色);Beam=左侧 1.2px 宽条;**Default/Deprecated 变体归 Block**(vte 枚举有混合历史值,映射时 exhaustive match 归三类)。光标格的字形照画(Underline/Beam 不整格交换,字可见)
- [ ] **Step 3: 闪烁**——默认不闪;`cursor-blink = true` 配置键(Bool 键,settings 接):WM_TIMER(500ms)切换 visible → 只重绘光标行(force_full 简化,交互态闪烁成本低);ESC[?12l/h 由上游 mode 处理,visible 翻转仅由 timer 驱动(app 层 blink_phase: Cell<bool>)
- [ ] **Step 4: 测试**——DECSCUSR 序列驱动 cursor_style 变化(core);frame:三种形状的实例几何(高度/宽度断言);真机:`printf '\e[3 q'` beam、`\e[5 q` 块
- [ ] **Step 5: Commit** `feat: cursor styles per DECSCUSR with optional blink`

### Task 7: 分屏接线(功能大头)

**Files:** `crates/mica-app/src/app.rs`(主)、`crates/mica-core/src/layout.rs`(消费)

- [ ] **Step 1: TabState 重构**——`TabState { id, title, dirty, panes: Vec<PaneState>, layout: Layout, focused: usize }`;`PaneState { id: u64, terminal: Terminal }`(M2a 的 terminal 迁进 panes[0]);**迁移策略**:所有 `.get_mut(ACTIVE).map(|tab| &mut tab.terminal)` → `.active_pane_mut()` 辅助(取 focused pane 的 terminal)——集中一处改动
- [ ] **Step 2: 渲染分路**——draw_frame:strip 不变;终端区按 layout.rects(客户区减 strip)逐 pane:build_rows(每 pane 自己的 term/router/damage)→ 实例偏移 pane.rect.xy → 汇入同一 buffer;**焦点 pane 边框**:1px 亮线(4 条细 quad,palette.colors[4])
- [ ] **Step 3: 输入路由**——键盘/选择/滚轮 → focused pane(px_to_buffer_point 减 pane 偏移);WM_LBUTTONDOWN 终端区 → 点中的 pane 聚焦 + 选择;`Alt+方向` 焦点跳转(几何最近邻:候选 pane 中心与当前中心的方向向量点积最大)
- [ ] **Step 4: Action 接线**——SplitRight/SplitDown:layout.split(focused, dir, new_pane)+ 起新 pane(复用 create_tab 的 Terminal 构造,但**不起新 forwarder?**——pane 有自己的 pty → 需要独立 forwarder(WPARAM 复用 tab id?pane 级 id!)——**WM_APP_RENDER WPARAM 编码:高 32 位 tab id 低 32 位 pane id?简化:全局 pane id 单调计数,WPARAM = pane_id,TABS 里按 pane id 查)**;ClosePane:layout.remove + Drop terminal;关最后 pane = 关标签;标签关闭 = 全 pane Drop
- [ ] **Step 5: resize/热重载**——全部 tab 全部 pane 重算(rects 由客户区重推)
- [ ] **Step 6: 测试+真机**——core 布局测试(T1)背书;真机:Ctrl+Shift+D 右分屏、双 pane 各跑 vim、Alt+方向跳焦点、resize pane、ClosePane、关标签清双 pty 无残留
- [ ] **Step 7: Commit** `feat(app): split panes on the layout tree`

### Task 8: 自绘标题栏 + Mica 材质

**Files:** `crates/mica-app/src/app.rs`

- [ ] **Step 1: 去 NC 区**——WM_NCCALCSIZE 返回 0(客户区=整窗含原标题栏区);顶栏 = tab strip 扩展(标题文字区 + 标签 + 关闭/最大化/最小化按钮自绘 + WM_NCHITTEST 命中:顶边拖拽 HTCAPTION、按钮区 HTMINBUTTON/HTMAXBUTTON/HTCLOSE、边缘 HTRESIZE 八向)
- [ ] **Step 2: 按钮**——strip_quads 扩展:右上角三按钮(— □ ×)纯字形路由;点击处理(WM_NCLBUTTONDOWN 或客户区命中模拟:SC_MINIMIZE/SC_MAXIMIZE/SC_CLOSE)
- [ ] **Step 3: Mica 材质**——`DwmSetWindowAttribute(DWMWA_SYSTEMBACKDROP_TYPE, DWMSBT_MAINWINDOW)`(Win11 22H2+;`IsWindowsServer`/版本判断,失败静默降级);暗色随系统(注册表 Themes\Personalize\AppsUseLightTheme 轮询或注册 RegNotify,M2b 用启动时读一次+热重载键 `window-theme = light/dark/auto`)
- [ ] **Step 4: 双击顶栏最大化、系统菜单保留(Alt+Space 已走 DefWindowProc)**
- [ ] **Step 5: 真机**——Win11:Mica 半透质感、按钮可用、拖拽/双击/边缘 resize;Win10 降级纯色不炸(无 Win10 机器则以 CI 编译 + API 失败静默为证,记冒烟边界)
- [ ] **Step 6: Commit** `feat(app): custom titlebar with Mica backdrop, system-button strip`

### Task 9: 整合冒烟(M2b 验收)

- [ ] **Step 1**:全量 fmt/clippy/test + 交叉 lint

清单(全过才算 M2b 完成):

1. 分屏:Ctrl+Shift+D/E 右/下分屏,双 pane 各跑 vim,`Alt+方向` 焦点跳转,`Ctrl+Shift+方向` 调比例,ClosePane 与关标签清 pty 无残留
2. 连字:`echo 'a => b != c -> d'` Cascadia 下连字成形;Sarasa 未装走探测回退不劣化
3. 彩色 emoji:`echo 😀🎉` 彩色渲染,灰度字形邻排无白边
4. OSC 52:`printf '\e]52;c;%s\e\\' <base64>` 写系统剪贴板;tmux 复制(有环境)读回
5. 光标:`printf '\e[3 q'` beam、`\e[4 q'` underline、`\e[2 q'` 块;`cursor-blink = true` 闪烁
6. 标题栏:Mica 材质、三按钮、拖拽/双击最大化、边缘 resize;Win10 降级路径编译绿
7. 灰度回归:中英混排/选择/标签(冒烟 M2a 清单抽样 3 条:标签切换、选择复制、滚轮)
8. 性能抽样:全屏 `dir /s` 流畅(RGBA 迁移无退化)
9. CI 双平台绿;CLAUDE.md 边界清账(M2b 五项下账);README 里程碑 M2 ✅;dev→main 发版 PR

---

## 执行顺序与依赖

T1(布局树)独立先行;T2(RGBA)→ T3(COLR)→ T4(连字)线性(图集能力链);T5(OSC52)独立;T6(光标)独立(建议在 T7 前——光标形状渲染改动 build_rows,T7 的大重构会搬动它,先后少一次合并冲突);T7(分屏)依赖 T1;T8(标题栏)最后(独立但视觉收尾)。**建议串行:T1 → T2 → T3 → T4 → T5 → T6 → T7 → T8 → T9**。

## 已知风险与执行者注意

- **T3 COLR 的位图合成**是唯一没找到直接 API 的点(CreateAlphaTexture 只产 alpha mask):计划锁定"多层 mask × 层调色板色"合成路径,执行时先写探针测试验证 TranslateColorGlyphRun 的层数与调色板语义,不行则以**位图 emoji 彩色格子降级**(记边界)——D19 的 RGBA 图集不白做
- **T4 shaping 的 cluster map**:多字符→单 glyph 映射是连字的判定与几何来源(advance 合成);GetGlyphs 的 u16 输入转换注意 emoji 代理对(单 char 已是码点,u16 转换用 `encode_utf16` 拼接,长度≠chars.len() 时 cluster 对齐要小心——**探针先行**)
- **T7 WPARAM 重编码**是全链路改动(forwarder 投递 → WM_APP_RENDER 查找),先改协议再改池
- **T8 WM_NCCALCSIZE** 后 DWM 合成边缘(阴影/圆角)在 Win11 有隐藏缩进 hack(实测为准);失败回退一个 commit(系统标题栏 + strip 在客户区,即 M2a 形态)
- 预算:RGBA 迁移的测试期望改动量大(全量 ×4 断言),留半天

## Self-Review 记录

- 覆盖 spec M2b 全项:分屏(§6/T1+T7)、连字(§7/T4)、COLR(§8/T2+T3)、OSC52/光标(§9/T5+T6)、标题栏(§10/T8)✓
- 依赖序明确:图集能力链(T2→T3→T4)与结构链(T1→T7)正交;探针先行原则落在两个高风险点 ✓
- 契约延续:两遍发射(T7 逐 pane 仍走 build_rows+repack)、RGBA 一次迁移无双轨(D19)、WT 键位(D15 扩展分屏键)✓
