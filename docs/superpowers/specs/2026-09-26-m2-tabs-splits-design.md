# M2 窗口体验设计文档(标签 / 分屏 / 选择 / 连字 / 彩色 emoji)

- 日期:2026-09-26
- 状态:设计已经所有者确认("全按推荐",2026-09-26 头脑风暴五点拍板)
- 上游:主 spec `2026-09-20-win-ghostty-design.md`(§2 D 决策、§5-§7 窗口/渲染/输入、§10 M2 行:多标签、分屏、快捷键体系、右键菜单、OSC 52、连字)
- 验收锚点(M2 完成定义):**可替代 Windows Terminal 日常使用**——多标签多 shell 并行、鼠标选择复制、分屏对照、连字与彩色 emoji、WT 键位肌肉记忆零成本

## 1. 范围(全量 M2)

1. 多标签:自绘 tab strip(客户区顶部)、新标签按钮、后台标签行为
2. 快捷键体系:Windows Terminal 兼容默认键位 + `keybind` 配置化
3. 鼠标:流式选择(拖选/双击词/Shift+Click 扩展)、剪贴板复制粘贴、**滚轮 scrollback**(M1 遗漏的必要能力)
4. M1 边界清账:Alt 窗口路由(WM_SYSKEYDOWN/WM_SYSCHAR)、resize 瞬态白块
5. 分屏:布局树、焦点路由、pane 边框
6. 连字:DWrite 原生 shaping(kitty 式渲染层方案)
7. COLR 彩色 emoji(RGBA 图集)
8. OSC 52(剪贴板读写协议)
9. 光标样式变体(beam/underline/空心块 + 闪烁)
10. 自定义标题栏(WM_NCHITTEST)+ Mica 材质(`DWMWA_SYSTEMBACKDROP_TYPE`)

**非目标(M2 不做)**:设置 GUI(配置文件驱动不变)、单实例/named-pipe IPC(M3)、矩形选择(M3)、IME 候选框跟随打磨(M3)、右键菜单(M2b 顺手做极简版:复制/粘贴/新建标签三项,不做完整菜单体系)、图形协议(M4)。

## 2. 关键决策记录

| # | 决策 | 选择 | 被否选项与理由 |
|---|------|------|----------------|
| D13 | 切片顺序 | M2a(标签+快捷键+选择/剪贴板+滚轮+Alt 路由+白块清账)→ M2b(分屏+连字+COLR+OSC 52+光标样式+自绘标题栏+右键菜单) | 否决连字先行:标签是多 surface 架构改造,分屏与后续一切复用它;选择/复制是"替代 WT"最大缺口 |
| D14 | 标题栏两步走 | M2a 保留系统标题栏,tab strip 画在客户区顶部;M2b 自绘标题栏(WM_NCHITTEST)+Mica 材质 | 否决一步到位做自绘 NC 区:Win32 打磨重灾区,先让标签功能跑通;材质与功能正交 |
| D15 | 默认键位 | **Windows Terminal 兼容**(`Ctrl+Shift+T/W` 开关标签、`Ctrl+Shift+1..9` 跳标签、`Ctrl+Tab`/`Ctrl+Shift+Tab` 轮换、`Alt+方向` 分屏焦点、`Ctrl+Shift+D` 右分屏、`Ctrl+Shift+E` 下分屏、`Ctrl+Shift+方向` 调 pane 尺寸);`keybind = <trigger> = <action>` 配置化,`keybind = clear` 清空重绑 | 否决 Ghostty 键位为默认:目标用户是 WT 迁移者(所有者本人),肌肉记忆零成本优先;Ghostty 语义在配置层做别名,配置语法兼容性不受影响(D6 只承诺 `key = value` 形态) |
| D16 | 连字引擎 | **DirectWrite 原生 shaping**(`IDWriteTextAnalyzer::GetGlyphs/GetGlyphPlacements`,零新依赖)——修订主 spec 原 harfbuzz 决策;等宽编程连字序列(=> != >= ->)简单,analyzer 足够;渲染层 kitty 式:网格模型不变,build_rows 对相邻可连字序列整形,序列首格承载合成字形、余格空白 | 否决 harfbuzz:多一个 C 依赖(license 合规但构建链重)换不来 analyzer 没有的东西;否决改网格模型存连字:伤 M1 全部网格语义与测试。**回退条款:analyzer 实测不能产连字(字体覆盖缺失)时再引 harfbuzz,过设计修订** |
| D17 | 选择模型 | 流式(字符流跨行,行内顺排、行间续行语义),左键拖选、双击选词、三击选行、Shift+Click 扩展;`Ctrl+C` 复制(选中不自动复制,`copy-on-select` 配置键默认 false——WT 默认 true 但误触率高,保守起步)、`Ctrl+V`/`Shift+Insert` 粘贴 | 否决矩形先行:95% 日常是流式;矩形选择是编辑器向需求(M3) |
| D18 | 后台标签行为 | PTY 持续收数据不暂停,feed 持续跑,渲染跳过(仅非活跃标签不 draw);切回时按该标签自己的 damage 全量重绘一次 | 否决后台节流 feed:终端语义要求任务持续跑满(yes > log),丢数据不可接受;否决后台也渲染:GPU 浪费 |
| D19 | COLR 图集 | 单图集升级 RGBA8(R8G8B8A8Unorm),灰度字形 alpha 通道存 coverage、RGB 白(ink 乘色);彩色字形 RGB 存色、alpha 1;shader 不变(`textureSample().rgba` 语义扩展) | 否决双图集双 bind:管线/绑定翻倍复杂度;否决 R8+单独彩色通道:采样路径分叉。内存代价:1024² RGBA=4MB,可接受 |
| D20 | 光标样式 | DECSCUSR 承载(beam/underline/块+空心变体),闪烁用 WM_TIMER 低频(500ms)驱动重绘光标行;样式与色均从 palette.cursor | 否决闪烁常开:费电;默认不闪(块状静态),`cursor-style-blink` 配置 |

## 3. 标签架构(M2a 核心)

### 状态模型(app 层)

- `Vec<Tab>` + `active: usize`;每 `Tab { label, Terminal }`(Terminal 即 M1 的 struct:term/session/pty_buf/router/metrics/palette/row_insts/force_full…不变)
- **窗口级共享一份**:GpuContext、wgpu surface、Renderer、字体链(同字号;标签级字体差异 M3 再论——`font-size-per-tab` 记开放问题)
- 切换标签 = 换 active 索引 + 对新标签 `force_full = true` + draw_frame;旧标签资源驻留(关标签才 Drop,PTY Drop 杀子进程的既有契约不变)
- tab strip 高度一行入 `metrics` 外的常量(如 32px),客户区 = 全区减 strip;布局换算集中一处(`client_rect_for_panes`)

### tab strip 渲染(与终端同管线)

- strip 是 UI quad 序列:背景条 + 每 tab 背景矩形(活跃 tab 高亮色 = palette.colors[4] 蓝或专用)+ 标题文字
- **文字走 DwriteRouter 同一图集路由**(tab 标题 = OSC 0 title,M1 已采集)——UI 文本与终端文本共享字形管线,零新增系统
- 新标签 `+` 按钮:敏感矩形命中(mouse click 区域表);标签关闭:中键点击 + `Ctrl+Shift+W`(WT 语义)
- 标签溢出:超出宽度显示滚动箭头(简单水平滚动),M2a 只做能看不阻塞

### 后台标签(D18)

- 转发线程按标签投递:pty_buf 按 tab 分流(或 per-tab forwarder 线程——取 per-tab 线程,`spawn_render_forwarder` 已参数化,WPARAM 带 tab 索引的 WM_APP_RENDER)
- WM_APP_RENDER(hwnd, tab_idx):feed 对应 tab;非活跃 → 置脏不 draw;活跃 → draw

## 4. 快捷键体系(M2a)

- `Keymap { binds: Vec<(Trigger, Action)> }`,Trigger = (Key, Mods);Action 枚举:`OpenTab/CloseTab/NextTab/PrevTab/GotoTab(n)/SplitRight/SplitDown/ClosePane/FocusNeighbor(dir)/ResizePane(dir)/Copy/Paste/ScrollLine(d)/ScrollPage(d)/ScrollTop/ScrollBottom`
- 默认表(D15)+ `keybind = ctrl+shift+t = new_tab` 配置解析(parse 已容错未知键,keybind 值语法 `mods+key = action`,ghostty 形态);`keybind = clear`
- 路由:WM_KEYDOWN 先查 Keymap,命中 → Action 执行(终端外语义);未命中 → `input::encode`(终端内语义)。**Alt 路由清账**:WM_SYSKEYDOWN/WM_SYSCHAR 挂进同一分发(M1 已知缺口的修复形状)

## 5. 选择与剪贴板(M2a)

- `Surface` 增 selection:`Option<(Anchor, Head)>`(网格坐标,Line 含 scrollback 偏移);set/clear/`selected_text() -> String`(流式拼接,行尾按 wrap flag 决定 \n)
- 触发:WM_LBUTTONDOWN 设锚、MOUSEMOVE 拖选(像素→格子换算)、双击/三击(词/行,词边界按 `char::is_alphanumeric` 类别)、Shift+Click 移动 head、ESC/点击清空
- 渲染:选中格 fg/bg 用 `palette.selection_fg/selection_bg`——**M1 忽略的 selection-background/selection-foreground 主题键接上**(apply_pair 补两槽)
- 复制:`Ctrl+C`(有选区时复制并清选区;无选区发 ETX——WT 同款语义)、`Ctrl+Insert`;粘贴 `Ctrl+V`(剪贴板文本→pty 写,LF 归一为 CRLF:ConPTY 语义)、`Shift+Insert`
- 剪贴板 Win32:`OpenClipboard/GetClipboardData(CF_UNICODETEXT)`,app 层薄封装

### 滚轮 scrollback(M2a 补漏)

- WM_MOUSEWHEEL → `Surface::scroll_display(delta)`(alacritty `Term::scroll_display` 现成 API,视口偏移 grid 显示);滚出内容时视口钉在底部;新输出且视口在底部 → 跟随;不在底部 → 不跟随(钉住阅读位置)
- 渲染:display offset ≠ 0 时行号换算(grid 可见行 = buffer 行 - offset),damage 交互:滚动置 force_full

## 6. 分屏(M2b)

- 布局树纯逻辑(mica-core 新 `layout.rs`,可 TDD):`Layout::split(pane, dir, ratio)/resize(pane, delta)/remove(pane) -> 矩形表`;树形 { Leaf(tab_idx) | Split(dir, ratio, a, b) }
- pane = Tab 的推广:同一标签页内多 Terminal;**标签与分屏正交**:Tab 持 Layout 而非单 Terminal(M2a 的 Tab.terminal 先是单 Leaf 布局的特例——迁移路径平滑)
- 焦点:每标签一个 focused_pane;`Alt+方向` 按几何最近邻跳;边框 1px 亮线(焦点 pane 高亮色)
- 渲染:每 pane 视口(scissor/viewport 偏移)+ 全 pass 平铺(repack 输出按 pane 矩形偏移)

## 7. 连字(M2b,D16)

- `dwrite.rs` 增 shaping 路径:`DwriteRouter::route_run(&[char], style) -> Vec<GlyphInfo>`(TextAnalyzer 产连字合成 glyph + advances);单字符路径(route)退化为 run=1
- build_rows 行内扫描:对连续同 style 文本段(≥2 字符)走 route_run;连字序列首格承载合成字形(宽 = 序列总 advance 取整到格),余格空白(spacer 语义)
- damage 交互:序列任一格变化 → 该序列整体重排(行级重建天然覆盖——整行重build,无需序列级追踪)
- 字体覆盖探测:启动时对 "=>","!=" 试 shape,产物 glyph 数 < 字符数 = 该字体支持连字;不支持则自动跳过 shaping(行为不劣化)

## 8. COLR 彩色 emoji(M2b,D19)

- 图集 RGBA8 化:GlyphBitmap 增 `format: GlyphFormat`(R8/ColorRgba);彩色字形 `IDWriteFactory4::TranslateColorGlyphRun` 光栅化 RGBA
- 灰度字形迁 RGBA:coverage 存 a、RGB=白;shader `ink = sample.a`、彩色 `color = sample.rgb * a`——fs 改一处(按 GlyphInfo 增 color 标志位或统一预乘)
- palette 不参与彩色字形(emoji 自带色)

## 9. OSC 52 / 光标样式(M2b)

- OSC 52:alacritty `Event::Clipboard(ClipboardType::Copy/Paste, String)`——base64 在上游解码好;app 层 Copy→剪贴板、Paste→读剪贴板写应答;长度上限 100KB(防炸)
- DECSCUSR:`\x1b[N q` → cursor style;Surface 暴露 `cursor_style()`;渲染:beam=1px 宽度 quad、underline=底部 2px、空心块=描边四条 quad;闪烁 WM_TIMER(500ms)只重绘光标行

## 10. 自绘标题栏 + Mica 材质(M2b 收官)

- WM_NCCALCSIZE 去系统标题栏 → 客户区上移;WM_NCHITTEST 顶边拖拽/双击最大化/系统菜单;tab strip 融入顶栏(与终端同管线渲染)
- `DWMWA_SYSTEMBACKDROP_TYPE = MICA`(Win11 22H2+;检测失败降级纯色,Win10 同);暗色模式跟随系统(注册表 Themes\Personalize 监听)
- resize 白块清账:WM_SIZE 同帧 reconfigure+draw(事件化后丢帧窗口已极小,补 WM_ERASEBKGND 返回 1 阻擦除)

## 11. 测试策略

- **纯逻辑(mac 全 TDD)**:布局树(split/resize/remove/矩形表)、选择流式拼接(wrap/跨行/空格裁剪)、keybind 解析(触发器语法/冲突后值覆盖/clear)、滚轮视口语义(跟随/钉住)、光标样式编码
- **windows-only**:shaping 连字表驱动(支持字体产合成 glyph;不支持退单格)、COLR RGBA 光栅化、tab strip 命中、剪贴板往返——交叉 lint + 真机冒烟
- **冒烟锚点(M2a)**:5 标签并行跑(powershell/cmd/pwsh),后台 `dir /s` 切回无丢失;选择复制粘贴到记事本往返;`Ctrl+Shift+T/W` 肌肉记忆直用;Alt+B 删词工作;滚轮翻 10000 行 scrollback 流畅
- **冒烟锚点(M2b)**:vim 里 `=>` `!=` `->` 连字;😀 彩色;tmux OSC 52 远程复制到本机;左右分屏双 vim 对照移动;Win11 Mica 材质观感

## 12. 风险与缓解

| 风险 | 缓解 |
|---|---|
| tab strip/UI quad 自绘打磨无底洞 | UI 面积锁死最小集(strip+按钮+边框);Win32 原生控件方案否决在先(spec §5 自绘既定),不自拓 |
| 连字与 damage/两遍发射契约冲突 | 序列级重排靠整行 rebuild 天然覆盖;黄金对拍测试扩到连字路径 |
| RGBA 图集迁移破坏 M1 全量渲染 | 一次迁移一次到位(不做兼容双轨),全量截图对拍 + ink 回归测试;内存 4MB 封顶 |
| 自绘标题栏 DPI/多显示器 edge case | M2b 最后一步做,独立冒烟;出问题回退系统标题栏一个 commit |
| TextAnalyzer 连字覆盖不足 | D16 回退条款:实测不足再引 harfbuzz(过设计修订,不硬扛) |
| per-tab 转发线程爆炸(标签多) | 线程仅转发 PostMessage,恒轻量;标签数上限 32(超过 UI 也不可用) |

## 13. 开放问题

1. 每标签独立字体/字号(远程机器小屏场景)——M3 随 profile 体系一并论
2. 标签拖出成新窗口( tear-off)——M3/M4 观望,依赖多窗口架构
3. `copy-on-select` 默认值是否随反馈改 true——发布后所有者实测定
4. 分屏内滚轮路由到焦点 pane 还是悬停 pane——倾向悬停(WT 行为),冒烟定
5. 标签页崩溃隔离(单 pty 死拖垮全窗?)——PTY Drop 契约已隔离子进程,窗口层崩溃面待 M2a 冒烟观察
