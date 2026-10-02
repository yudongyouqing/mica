# M4 协议补全设计文档(kitty keyboard / 2026 / OSC 8 / 133 / graphics 预研)

- 日期:2026-10-02
- 状态:设计已经所有者确认("全按推荐",2026-10-02 五点拍板)
- 上游:主 spec `2026-09-20-win-ghostty-design.md`(§4 能力清单第 4/8 条、§9 协议增量、§10 M4 行)
- 验收锚点(M4 完成定义):**Claude Code / vim 长输出与键位体验不输 mac Ghostty**——kitty 键位全兼容、粘贴无边界碎片、大刷新无撕裂、链接可点

## 1. 范围(全量 M4)

1. **kitty keyboard protocol**:全部四个渐进 flag(输入侧编码器自建,状态机用上游)
2. **bracketed paste**:mode 感知粘贴包裹(小项,随 A 落)
3. **DECSET 2026 synchronized output**:旁路 Parser 监听 + 帧持有(150ms 安全阀)
4. **OSC 8 hyperlinks**:cell 状态消费(上游已解析)+ 下划线样式 + Ctrl+Click 打开
5. **OSC 133 shell integration**:旁路 Parser 解析 A/B/C/D 标记,存行属性;UI 消费(jump/状态行)留 M5
6. **kitty graphics**:纯预研文档(协议结构/内存模型/wgpu 图集关系),不实现

**非目标(M4 不做)**:OSC 133 的 UI 消费(jump/CWD 提示,M5)、OSC 8 hover tooltip(M5)、kitty mouse protocol(与终端鼠标选择冲突,需求出现再议)、图形协议实现(预研文档定论后再立项)、iTerm2 inline images(同上)

## 2. 关键决策记录

| # | 决策 | 选择 | 被否选项与理由 |
|---|------|------|----------------|
| D27 | kitty 键位范围 | **四 flag 全量**:disambiguate(1)/report-event-types(2, 含 release)/all-keys-as-esc(3)/associated-text(4);编码器 `mica-core::protocol::kitty_encode`(纯函数 TDD);`Config.kitty_keyboard = true` 激活上游状态机,mode 位经 `term.mode()` 读 | 否决只做 1+2:nvim 全键位(sh+fn+release)直接半残,重做成本高于一次到位 |
| D28 | OSC 8 交互 | **下划线样式 + Ctrl+Click `ShellExecuteW` 打开**;hover 提示 M5 | 否决 hover 先行:tooltip 窗口是独立 Win32 工程,Ctrl+Click 是 WT 既有肌肉记忆 |
| D29 | 2026 机制 | **旁路 vte::Parser 监听 + 帧持有**:第二个 Parser(Perform 只记 2026h/l)与主 feed 并行跑;h 起置 sync 标志(app 丢弃重绘唤醒),l 到一次全量画;**150ms 超时强制放帧**(kitty 协议规定的安全阀,防应用忘记 l) | 否决改架构拦截:上游 Term 不认 2026,前置剥离需自定义 Processor——双 Parser 零侵入,吞吐开销可测(吞吐敏感终端,M4 冒烟带基准) |
| D30 | OSC 133 范围 | **解析 + 存储,消费 M5**:旁路 Parser(与 D29 同一个)解析 `OSC 133 ;A/B/C/D`,Surface 暴露行属性查询;本里程碑无 UI | 否决连 UI 一起做:jump/滚动语义是 M5 打磨票,数据先落地让 Claude Code 探测即刻受益 |
| D31 | kitty graphics | **纯预研文档**(`docs/superpowers/research/`):协议结构(命令/传输/放置)、内存模型(id-keyed texture)、与 wgpu RGBA 图集的关系、工作量估计 | 否决最小实现:kitty graphics 是 ~数千行的大件,预研文档先行让 M5 排期有据 |

## 3. kitty keyboard(D27,spec §9)

### 上游激活与状态

`Surface::new` 的 `Config { kitty_keyboard: true, .. }`——上游即刻接管 mode 栈、查询应答(`CSI ? u` 报当前 mode)、push/pop。四个 flag 进 `TermMode`(位 18-22),`term.mode()` 可读。

### 自建编码器(mica-core/src/protocol/kitty.rs,纯逻辑 TDD)

```text
kitty_encode(key: Key, mods: Mods, kind: EventKind, app_kitty_flags: u32) -> Vec<u8>
  EventKind { Press, Repeat, Release }
```

- **何时走 kitty**:app 读 `term.mode()` 含任一 kitty 位 → kitty_encode;否则 legacy `input::encode`。all-keys-as-esc 开启时连普通字母也要编码
- **编码格式**:`CSI unicode-key-code:modifiers:event-type ; text u`——keycode = keysym(Unshifted unicode;特殊键用 functional 编号:Enter 13/Tab 9/Esc 27/Backspace 127/方向 57352-57357…);modifiers = 1+shift(1)+alt(2)+ctrl(4)+super(8);event-type 1/2/3;associated-text 仅 Press 携带且须与 keycode 一致(协议一致性自检)
- **发布与 native 区分**:仅 `REPORT_EVENT_TYPES` 开启时发 Release/Repeat;否则只发 Press(协议规定)
- **Ctrl+字母**:kitty 协议下不降级为控制字节——保持 unicode keycode + ctrl 位(mods=5)交给应用

### 测试表

- 四 flag 各自的开关矩阵 × (key, mods, kind) → 精确字节
- all-keys-as-esc:字母 'a' 裸按也编码(vs legacy 直通)
- associated-text:Press 带 'a' 文本;Release 不带
- mode 关闭 → 调用方回 legacy(app 侧集成测试)

## 4. bracketed paste(随 D27 落)

- `Surface::bracketed_paste_active() -> bool`(读 `TermMode::BRACKETED_PASTE`)
- app 的 Ctrl+V/Shift+Insert 粘贴:位开 → `ESC[200~` + 文本 + `ESC[201~`;关 → 原样(CRLF 归一不变)
- 已有 `clipboard::normalize_paste` 不动

## 5. DECSET 2026(D29)

### 旁路 Parser(mica-core/src/protocol/sidecar.rs)

```text
/// 第二 vte Parser 的 Perform:只认识 CSI ?2026 h/l 与 OSC 133,其余全丢。
/// 与主 feed 并行跑同一段字节流——alacritty 对未知序列本就忽略,零冲突。
struct SidecarParser { /* vte::Parser + 状态 */ }
impl Perform for SidecarRecord { ... }
Surface::feed 前调 sidecar.scan(bytes) -> SidecarEvents { sync_begin, sync_end, osc133: Vec<(usize 行号, Mark)> }
```

零侵入双解析的代价是吞吐(每字节两遍)——**基准预算:M4 冒烟带 `hyperfine cat 大文件` 对比,回归 >5% 则把 sidecar 换成字节前缀快筛(`ESC[?2026` 与 `ESC]133` 才深跑)。

### app 帧持有

- sync_begin → TLS `SYNC_HELD = true`:WM_APP_RENDER 照常 drain+feed 但**跳过 draw**(损坏区累积照旧,数据不丢)
- sync_end 或 150ms 超时 → `SYNC_HELD = false` + force_full + 一次 draw
- 超时定时器:复用 WM_TIMER(id=2,150ms;sync_end 即 KillTimer)

## 6. OSC 8 hyperlinks(D28)

### 渲染(mica-render)

- `build_row`/`build_instances`:cell.hyperlink() 存在的字形格 → 下划线 quad(复用 cursor_bar 的形状逻辑:底部 1px,色 = fg 略淡或 palette.cursor;**不占用新图集空间**)
- 下划线不与光标条冲突:光标优先(已有 swap 路径);选区反色时下划线随 fg

### 交互(mica-app)

- WM_LBUTTONDOWN 且 GetKeyState(VK_CONTROL) < 0 → 命中格的 hyperlink → `ShellExecuteW(HWND_DESKTOP, "open", uri, ...)`;URI 打开失败静默(URL scheme 莫名其妙的场景)
- 命中换算复用 pane 路由(rect 偏移 + px_to_buffer_point)

## 7. OSC 133(D30)

- sidecar 解析 `OSC 133 ;A`(prompt start)/`;B`(cmd start)/`;C`(output start)/`;D`(cmd end;可带 exit=0);非法参数丢弃
- `Surface` 存 `Vec<RowMark>`(行号 + Mark;scrollback 区随 resize 丢弃——与 damage 语义对齐,只保视口+历史常驻部分)
- 本里程碑无消费方;M5 jump/CMD 检测直接读

## 8. kitty graphics 预研(D31)

`docs/superpowers/research/2026-10-kitty-graphics.md`:命令族(q/L/t/f/p/d)、base64 payload 传输、keyed image 内存模型、 placements(虚拟网格 vs 像素)、与现有 wgpu RGBA 图集的融合点(cell 图集 vs 独立 image atlas)、工作量粗估。**不写代码。**

## 9. 测试策略

- **纯逻辑(mac 全 TDD)**:kitty_encode 全矩阵(表驱动,数十 case)、sidecar 解析(2026 嵌套/133 五种标记/半序列容忍/与正常文本混流)、bracketed 包裹判定、OSC 8 下划线实例断言
- **windows-only 真机**:Ctrl+Click 开浏览器、2026 帧持有(vim `:redraw!` 批量)不撕裂、kitty 模式 vim 键位
- **基准**:sidecar 双解析吞吐(cat 10MB < 5% 回归)

## 10. 切片与验收

| 切片 | 内容 | 验收锚点 |
|---|---|---|
| M4a | kitty encoder + bracketed paste + app 键位路由 | nvim 键位全兼容(方向/sh+/ctrl+/release);粘贴带 200~ 边界 |
| M4b | 2026 sidecar + 帧持有 + OSC 8(样式+Ctrl+Click)+ OSC 133 解析 | 大输出无撕裂;链接可点;Claude Code prompt 检测受益(手验) |
| M4c | graphics 预研文档 + 收尾(边界清账/发版) | 预研文档评审过 |

## 11. 风险与缓解

| 风险 | 缓解 |
|---|---|
| sidecar 双解析吞吐回归 | 冒烟基准;前缀快筛备胎(D29) |
| kitty keysym 表错(特殊键编码) | 表驱动测试锁 + nvim 全键位冒烟清单 |
| 2026 超时与 sync_end 竞态(end 在超时后到) | sync_end 幂等(HELD 已 false 则只画不重置);150ms 窗口按协议是建议非硬限 |
| Ctrl+Click 与选择的冲突 | Ctrl 位分流:按住 Ctrl 不启动拖选(直接 return,打开或无操作) |
| associated-text 与 keycode 不一致(应用侧解析炸) | 编码器自检断言 + 表测试 |

## 12. 开放问题

1. kitty mouse protocol 报告模式(与终端自身鼠标选择抢事件)——需求出现再议,本里程碑不做
2. OSC 133 D 标记的 exit code 展示——M5 状态行
3. undercurl 等波浪下划线(kitty escape 4:3)——OSC 8 先用直线下划线,曲线 M5 随图集扩展
4. 多显示器 DPI 下 2026 帧持有的 150ms 是否够(大屏慢 GPU)——冒烟观察,可配置不进 M4
