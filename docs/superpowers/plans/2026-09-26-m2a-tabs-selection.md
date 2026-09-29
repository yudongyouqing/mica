# M2a 标签·选择·滚轮 Implementation Plan

- 日期:2026-09-26
- 状态:待执行
- 上游:spec `2026-09-26-m2-tabs-splits-design.md`(D13-D20;本计划覆盖 M2a 切片:D15 键位/D17 选择/D18 后台标签 + M1 边界清账)
- 验收锚点(spec §11 M2a):5 标签并行跑、后台保活;选择复制粘贴往返记事本;WT 键位直用;Alt+B 删词;滚轮翻 scrollback 流畅

## Global Constraints

- fmt/clippy `-D warnings` 零输出;workspace 测试全绿;交叉 lint `--target x86_64-pc-windows-msvc`
- **alacritty API 已核实(0.26.0 本地源码,直接用上游,不自造)**:
  - `Term::scroll_display(Scroll)`;`Scroll { Delta(i32), PageUp, PageDown, Top, Bottom }`(term/mod.rs:389、grid/mod.rs:73)
  - `Term.selection: Option<Selection>` 公开字段;`Selection::update(point, side)`、`rotate::<D>(dimensions)`(selection.rs:119/133/137)
  - `Term::selection_to_string() -> Option<String>`(term/mod.rs:529,**拼接含 wrap 语义,上游成品**)
  - `Grid::display_offset() -> usize`(grid/mod.rs:432);`point_to_viewport/display_offset 互相换算`助手(display.rs:124/131)
  - `Event::ClipboardStore(ClipboardType, String)` / `ClipboardLoad(ClipboardType, Arc<dyn Fn(&str)->String>)`(event.rs:25/31)——**M2a 只接 Ctrl+C/V 本机路径,OSC 52 事件接线留 M2b**(键已核,届时直接用)
- Win32 剪贴板(`Win32::System::DataExchange`:OpenClipboard/GetClipboardData/SetClipboardData + GlobalLock)与鼠标消息(WM_LBUTTON*、WM_MOUSEMOVE、WM_MOUSEWHEEL、双击 CS_DBLCLKS 或自计时)——windows-rs 0.62 签名执行时以编译器为准(与 M1-A dwrite 同纪律)
- 行为红线:后台标签 PTY 不丢数据(D18);关标签必杀子进程(Drop 契约);切换标签 force_full 重绘

---

### Task 1: M1 边界清账(Alt 路由 + resize 白块)

**Files:** `crates/mica-app/src/app.rs`

- [ ] **Step 1: Alt 路由**——wndproc 增挂 `WM_SYSKEYDOWN`(vkey_bytes 复用,mods 已含 alt 位)与 `WM_SYSCHAR`(代理对重组逻辑同 WM_CHAR;Alt+字符的 WM_SYSCHAR 发 `ESC + char` = input.rs encode 的 alt 前缀对偶,窗口层只补 `\x1b` 前缀)。DefWindowProc 仍处理系统菜单 Alt+Space
- [ ] **Step 2: resize 白块**——`WM_ERASEBKGND => LRESULT(1)`(阻背景擦除);WM_SIZE 尾部确保 reconfigure 后同函数内 draw(现有顺序已是,补注释锁死)
- [ ] **Step 3: 真机**——pwsh 里 `foo bar` 后 Alt+B 跳词、Alt+Backspace 删词;快速拖动无白闪
- [ ] **Step 4: Commit** `fix(app): route WM_SYS messages and block background erase`

### Task 2: 滚轮 scrollback

**Files:** `crates/mica-core/src/surface.rs`、`crates/mica-render/src/frame.rs`、`crates/mica-app/src/app.rs`

- [ ] **Step 1: Surface 滚动 API**——`pub fn scroll_display(&mut self, s: Scroll)` 直通 term;`pub fn display_offset(&self) -> usize`;`pub fn grid_view(&self) -> &Grid<Cell>` 语义不变(渲染直接用 grid,display_offset 由消费方读取后行号偏移)
- [ ] **Step 2: 渲染视口换算**——build_rows 的行迭代:`Line(i)` 改 `Line(i as i32 + display_offset as i32)`(buffer 坐标);damage 交互:滚动 → `force_full`(app 层置位,不污染 core)
- [ ] **Step 3: 跟随语义**——app 持 `pinned_offset`(最近用户滚动);新输出到达(display_offset > 0 且用户未主动钉住 → 保持相对位置由上游 rotate 处理;简化:feed 后若 `display_offset == 0` 跟随,否则钉住)。WM_MOUSEWHEEL:`scroll_display(Delta(-120/40 * 3 行))`;`Shift+滚轮` 横滚 M3 不做
- [ ] **Step 4: 测试**——core:scroll 后 display_offset 递变、Bottom 归零;render:offset≠0 时 build_rows 取到历史行(feed 多行 → 滚一屏 → 首行内容来自 scrollback)
- [ ] **Step 5: 真机**——`dir /s` 后滚轮上翻流畅、底部跟随/上翻钉住正确;vim 中滚轮无效(alt screen,上游语义)
- [ ] **Step 6: Commit** `feat(core): wheel scrollback with follow/pin semantics`

### Task 3: 选择模型与渲染高亮

**Files:** `crates/mica-core/src/surface.rs`、`crates/mica-core/src/config/palette.rs`、`crates/mica-render/src/frame.rs`

- [ ] **Step 1: Palette 补槽**——`selection_bg/selection_fg` 两字段(默认互换 fg/bg 灰白);`apply_pair` 接 `selection-background/selection-foreground`(M1 忽略的主题键,金样本库全有)
- [ ] **Step 2: Surface 选择 API**——`selection_anchor(point, side)`(新建 Selection + update)、`selection_update(point)`(拖动)、`selection_clear()`、`selection_text() -> Option<String>`(上游 `selection_to_string`)、`selection_range() -> Option<Range>`(渲染判定;`SelectionRange` 的 contains/is_within 语义执行时核,不可用则按 start/end 手写比较 ~10 行)。point 为 **buffer 坐标**(viewport 行 + display_offset,换算用 display.rs 助手)
- [ ] **Step 3: 高亮渲染**——build_rows 内:格在选区内 → fg/bg 用 palette.selection 对(在 INVERSE/光标交换**之前**,选中优先于两者;选中+光标 → 光标色仍可辨)。resize → `Selection::rotate`(上游 API)
- [ ] **Step 4: 测试**——palette 槽默认值与主题键;core:拖选跨行 wrap 拼接(selection_text 对上游行为的包装正确);render:选中格色对、与 INVERSE/光标叠加序
- [ ] **Step 5: Commit** `feat(core): selection model on upstream API with palette slots`

### Task 4: 鼠标事件路由(app)

**Files:** `crates/mica-app/src/app.rs`

- [ ] **Step 1: 像素→格子**——`px_to_cell(x, y, metrics) -> (Point<usize>, Side)`(Side 按半格界);y 含 display_offset 换算成 buffer 行
- [ ] **Step 2: 拖选状态机**——WM_LBUTTONDOWN(捕获鼠标 SetCapture)→ anchor;WM_MOUSEMOVE(按住)→ update + draw;WM_LBUTTONUP(ReleaseCapture);双击 = SelectionType::Semantic(词,边界按上游)、三击 = Lines;Shift+Click 移动 head;点击清选区;WM_MOUSEMOVE 未按住不处理
- [ ] **Step 3: 滚动中拖选**(滚轮接续选区扩展)M3 不做,记边界
- [ ] **Step 4: 真机**——拖选/双击词/三击行/Shift 扩展,高亮实时
- [ ] **Step 5: Commit** `feat(app): mouse selection state machine`

### Task 5: 剪贴板(app)

**Files:** `crates/mica-app/src/app.rs`(新 `clipboard.rs` 模块)

- [ ] **Step 1: Win32 封装**——`clipboard::get_text() -> Option<String>`、`set_text(&str)`(CF_UNICODETEXT;OpenClipboard 失败重试 3 次;关闭前取完)
- [ ] **Step 2: 键语义**——`Ctrl+C`:有选区 → 复制+清选区;无选区 → 发 `\x03`(ETX,WT 同款)。`Ctrl+V`/`Shift+Insert`:粘贴,LF→CRLF 归一(`\r\n` 已有则不动)。`Ctrl+Insert` 同 Ctrl+C
- [ ] **Step 3: 真机**——选中复制到记事本粘贴往返;无选区 Ctrl+C 中断 ping;多行粘贴正确
- [ ] **Step 4: Commit** `feat(app): clipboard copy/paste with WT key semantics`

### Task 6: Keymap 体系

**Files:** 新 `crates/mica-core/src/keymap.rs`、`crates/mica-core/src/config/settings.rs`、`crates/mica-app/src/app.rs`

- [ ] **Step 1: Action 与默认表**(core,纯逻辑)——`enum Action { NewTab, CloseTab, NextTab, PrevTab, GotoTab(u8), Copy, Paste, ScrollLine(i32), ScrollPage(i32), ScrollTop, ScrollBottom, … }`(分屏 Action 留变体占位,M2b 启用);`Keymap::default()` = D15 WT 表;`Keymap::bind(trigger, action)`(后值覆盖)、`clear`
- [ ] **Step 2: keybind 配置键**——settings 增 `keybinds: Vec<(Trigger, Action)>`;`keybind = <mods+key> = <action>` 解析(值内首个 `=` 分割;`keybind = clear` 特例);resolve 合成默认表 + 用户表
- [ ] **Step 3: 路由(app)**——WM_KEYDOWN 先查 Keymap(命中 → Action 执行,return);未命中 → encode 现路径。**Ctrl+C/V 的冲突**:Keymap 的 Copy/Paste 优先于 encode(encode 本也不处理 C/V,它们走 WM_CHAR 控制字符——WM_CHAR 里 `\x03`/`\x16` 到来前 Keymap 已拦截 WM_KEYDOWN,天然分层)
- [ ] **Step 4: 测试**——默认表断言;后值覆盖;clear;trigger 语法解析(ctrl+shift+t、f5、alt+left)
- [ ] **Step 5: Commit** `feat(core): keymap with WT-compatible defaults and keybind config`

### Task 7: 标签架构

**Files:** `crates/mica-app/src/app.rs`(主战场)、`crates/mica-render/src/frame.rs`(strip quad)

- [ ] **Step 1: Tab 池**——`Vec<Tab>` + `active`;`Tab { title_hint, Terminal 既有字段... }`;窗口级共享 GpuContext/surface/Renderer;**迁移而非重写**:现有 Terminal struct 原样搬进 Tab,draw_frame 带 tab 索引参数
- [ ] **Step 2: per-tab 会话与转发**——init_terminal 参数化(新标签 = 复用启动路径);forwarder 线程 per-tab,`PostMessageW(hwnd, WM_APP_RENDER, tab_idx, 0)`;WM_APP_RENDER 按 WPARAM 分流(非活跃 → 置脏;活跃 → draw)。**后台标签 feed 照跑**(PTY 数据进 grid,只跳渲染)
- [ ] **Step 3: tab strip 渲染**——`frame::strip_quads(tabs, active, palette, width) -> Vec<CellInstance>`(复用 CellInstance 格式:背景条/标签块/选中高亮;标题文字经 DwriteRouter 路由,同图集);draw_frame 两段 draw(strip + 终端区,viewport/scissor 或 Y 偏移——取 Y 偏移:终端 quad 的 y 加 strip 高,清屏色盖全区)
- [ ] **Step 4: 标签操作**——新建:`Ctrl+Shift+T` + strip `+` 按钮命中矩形;关闭:`Ctrl+Shift+W` + 中键(关前杀 pty 既有 Drop;关活跃 → 邻标签顶上;全关 → 退出);切换:点击命中/`Ctrl+Tab`/`Ctrl+Shift+Tab`/`Ctrl+Shift+1..9`;标题 = OSC title(既有采集,非活跃标签更新标题可见——strip 每帧用最新)
- [ ] **Step 5: 布局换算收口**——客户区 = strip(32px)+ 终端区;resize/网格换算全部走 `terminal_client_rect()`;溢出滚动箭头:标签总宽超限时 strip 整体水平偏移(极端简版)
- [ ] **Step 6: 真机**——5 标签(powershell/cmd/pwsh 并存),后台 `dir /s` 切走再切回内容完整;标签关闭无残留 powershell 进程;标题随 shell 更新
- [ ] **Step 7: Commit** `feat(app): multi-tab architecture sharing one GPU pipeline`

### Task 8: M2a 整合冒烟

- [ ] **Step 1**:全量 fmt/clippy/test + 交叉 lint;`cargo run --bin mica`

清单(全过才算 M2a 完成):

1. 5 标签并行(powershell/cmd/pwsh),`Ctrl+Shift+T/W/1-9/Tab` 肌肉记忆直用
2. 后台标签跑 `dir /s`,切走切回内容零丢失(D18)
3. 拖选/双击词/三击行/Shift 扩展,`Ctrl+C` 复制到记事本粘贴正确(多行/wrap)
4. `Ctrl+V` 粘贴多行命令;无选区 `Ctrl+C` 发 ETX
5. Alt+B 跳词、Alt+Backspace 删词(T1 清账)
6. 滚轮翻 scrollback 流畅;底部自动跟随、上翻钉住(T2)
7. 换主题(Dracula→TokyoNight Storm)→ strip/选中色随主题(T3/T7)
8. 快速拖窗口无白闪(T1)
9. 关全部标签 → 窗口退出、任务管理器无残留 shell 进程
10. 推送 CI 双平台绿;README/CLAUDE.md 边界清账(Alt 路由、白块两行下账)

- [ ] **Step 2: Commit + push,PR 回 dev**(流程同 M1-B)

---

## 执行顺序与依赖

T1(清账,小)→ T2(滚轮)→ T3(选择 core)→ T4(鼠标路由,依赖 T3)→ T5(剪贴板)→ T6(Keymap,T7 的键位地基)→ T7(标签,最大,依赖 T6)→ T8(冒烟)。T2 与 T3 可换序;T5 可与 T4 并行。

## 已知风险与执行者注意

- **SelectionRange 判定语义**(T3 Step2):上游 `SelectionRange` 字段/方法以本地源码为准(执行时核 `selection.rs:33` 起);is_within 不可用即手写比较
- **strip 文字路由**:tab 标题混排(图标/截断)不做,超宽省略号 `…`(单字符路由即可)
- **Ctrl+C 语义分叉点**:Keymap 拦截在 WM_CHAR 之前——`\x03` 的 WM_CHAR 仍会到(按住期间),去重:WM_CHAR 忽略 `\x03/\x16`(Keymap 已处理)或 Keymap 只挂 KeyDown 天然只触发一次
- **display_offset 进 build_rows**:光标高亮/INVERSE 都在 buffer 坐标,滚动时光标通常不在视口——自然隐藏,正确行为
- **per-tab forwarder 数量**:≤32 上限(spec 风险表);线程仅 PostMessage 转发,轻量

## Self-Review 记录

- 覆盖 spec M2a 全项:标签(D18/T7)、键位(D15/T6)、选择+剪贴板(D17/T3-T5)、滚轮(§5/T2)、Alt/白块清账(T1)✓
- API 复用最大化:scroll_display/selection 全套上游成品,自造面缩到状态机与路由 ✓
- 契约延续:两遍发射(选中色只改颜色不改顺序)、force_full 语义(T2/T7 复用)、PTY Drop(T7 关标签)✓
