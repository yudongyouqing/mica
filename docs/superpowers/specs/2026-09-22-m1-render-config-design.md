# M1 渲染补全设计文档(字体管线 / 配置与主题 / 体验补课)

- 日期:2026-09-22
- 状态:设计已经所有者逐节确认(头脑风暴于本日完成)
- 上游:主 spec `2026-09-20-win-ghostty-design.md`(§5 渲染层、§10 里程碑 M1 行);本文档细化并扩充 M1 范围
- 验收锚点(M1 完成定义):**中文日常可用**(输入/显示/复制)、主题与字体配置热生效、vim 方向键正确(DECCKM)、空闲 CPU 占用肉眼可辨地低于 M0 轮询版

## 1. 范围(全量 M1)

M0 复查账本与主 spec 汇总后,所有者选定**全量**:

1. DirectWrite 字形图集(替代 font8x8)+ 字体度量驱动格子尺寸
2. CJK 宽字符渲染 + 字体回退链
3. Ghostty 语法配置 + 主题整库内置 + 热重载
4. 调色板单一来源收敛 + OSC 4/12 应答正确化
5. DECCKM 应用光标模式 + Alt 修饰符前缀(输入补课,清账本两笔)
6. 渲染事件化(去 8ms 轮询)
7. 脏区跟踪(damage API 接入)

**非目标(M1 不做)**:连字 harfbuzz(M2)、光标样式变体(beam/underline,M2+)、WARP PNG golden 渲染测试(M2 增强)、Quick Terminal/标签/分屏(M2/M3)、IME 候选框跟随(壳层已有基础输入,M2 打磨)。

## 2. 关键决策记录

| # | 决策 | 选择 | 被否选项与理由 |
|---|------|------|----------------|
| D8 | 字体后端位置 | `mica-render/font/` 模块:`chain.rs`(纯逻辑)+ `metrics.rs`(纯逻辑)+ `dwrite.rs`(`#[cfg(windows)]` 光栅化) | 否决独立 mica-font crate:单模块体量,crate 仪式税大于收益;否决放 mica-app:壳层变厚违"app 最薄"纪律 |
| D9 | 默认字体 | **Sarasa Mono SC**(2:1 严格等宽中英混排),回退链 `["Sarasa Mono SC", "Cascadia Mono", "Microsoft YaHei", "Segoe UI Emoji"]`;M3 安装器随包分发字体(OFL 许可兼容,分发时保留其许可声明) | 否决"系统自带为默认":更纱黑体的等宽混排是终端理想形,且 OFL 允许随应用分发 |
| D10 | 主题生态 | Ghostty themes 目录(MIT、纯配置片段)**整库拷入仓库 `themes/`**,`include_dir` 编译期嵌入;目录内保留上游 MIT 许可声明 | 否决精选 10 个:D6 承诺的主题生态迁移一步到位,且零代码成本;否决运行时读文件:dev/release 路径不一致 |
| D11 | instance 格式 | `CellInstance` 48B → **64B**:`[pos_px.xy, uv.xy]` `[size_px.xy, uv_size.xy]` `[fg.rgb, _]` `[bg.rgb, _]`,每实例自带像素尺寸与图集 UV 矩形 | 否决 48B 内塞 UV 索引 + shader 查表:过早优化,布局测试与 shader 复杂度反升 |
| D12 | 渲染循环 | 读线程 `PostMessageW(hwnd, WM_APP_RENDER)` 唤醒;主循环 `GetMessageW` 阻塞;消息队列合并投递即天然帧合并 | 否决保留轮询改频率:M0 轮询是声明欠账;否决手写节流/vsync 对齐:Fifo present 自己对齐 |

## 3. 字体管线(mica-render/font/)

### chain.rs(纯逻辑,跨平台 TDD)

- `FontChain { families: Vec<FamilySpec> }`,配置驱动构建(默认见 D9)
- `resolve(ch: char, style: GlyphStyle) -> ChainSlot`:沿链找第一个覆盖该码点的家族;`GlyphStyle { bold, italic }`(来自 cell flags)
- 映射结果带缓存(HashMap,char+style → slot);表格驱动测试:ASCII 落主字体、汉字落回退、emoji 落 emoji 字体、加粗走 bold 变体
- **宽度判定**:`char_width(ch) -> 1|2`(East Asian Width 的 Wide/Fullwidth → 2)——与 alacritty 的 `WIDE_CHAR` 语义对齐,用于渲染与网格一致性

### metrics.rs(纯逻辑)

- `FontMetrics { cell_width, line_height, ascent, descent }` 由主字体度量推导:`cell_width = 最大 advance`(等宽字体即单一 advance);`line_height = ascent + descent + line_gap`
- **硬编码常量退役**:mica-core `surface::CELL_WIDTH/HEIGHT` 与 mica-render `atlas::CELL_WIDTH/HEIGHT` 删除,`Metrics` 从 dwrite 产出、流入 app(网格换算、WindowSize 应答)与 render(shader uniform)——复查账本"度量双份定义"欠账清除

### dwrite.rs(#[cfg(windows)])

- DirectWrite:`IDWriteFactory` → 按家族名建 `IDWriteFont`(`HasCharacter` 判覆盖,bold/italic 走 weight/style);`IDWriteGlyphRunAnalysis` 光栅化 R8 灰度抗锯齿位图
- 产出**真字形图集**:变宽字形、任意位置 UV 矩形、CJK 双宽(2×cell_w)、基线对齐(字形在 cell 内按 ascent 偏移)
- 图集管理:哈希找位分配;放不下 → 整图扩容重排,渲染无缝切换(spec §8 "图集满增量重建")
- COLR 彩色 emoji:M1 先走灰度,彩色路径 M2(记录,不做)

## 4. 渲染层改造

- `CellInstance` 64B(见 D11),布局锁死测试与 WGSL 三 location 对齐更新
- **宽字符**:`WIDE_CHAR` 格以 2×cell_w 绘制字形;`WIDE_CHAR_SPACER` 格跳过绘制(alacritty 网格语义)
- **BOLD/ITALIC**:经 `GlyphStyle` 走字体变体查询(不做人造加粗)
- **脏区**:`term.damage()` 接入——`Full` → 重建全部 instances;`Partial` → 仅重建受影响行(`LineDamageBounds.line - display_offset` 换算视口行,负数或越界跳过);无 damage → 跳过 CPU 重建。GPU 侧仍整缓冲上传(上传成本 ≪ 网格遍历,先取 90% 收益;分块上传 M2 再论)
- **Palette 收敛**:`Palette { 16 色 + fg + bg + cursor }` 单一来源移入 **mica-core**(`config` 模块旁);mica-render 的 `BASE16/DEFAULT_FG/DEFAULT_BG` 与 surface 的 `default_rgb` 全部改为消费 Palette
- **OSC 4**(palette 索引 0-15)与 **OSC 12**(光标色)应答走 Palette,格式 `\x1b]4;<i>;rgb:rrrr/gggg/bbbb\x1b\\`——复查账本两笔欠账清除

## 5. 配置系统(mica-core/config/)

- **解析器**:Ghostty 语法——`key = value`、`#` 注释、容忍空行/尾随空格;逐行收集 `ParseError { line, reason }`,绝不 panic
- **M1 键集**:`font-family`(可重复,追加构成回退链)、`font-size`(pt)、`theme = <name>`、`palette = <0-15> = #rrggbb`、`foreground = #rrggbb`、`background = #rrggbb`
- **加载优先级**:内置默认 < 主题片段 < 用户 `config`(同名键后者覆盖前者)
- **位置**:`%APPDATA%\mica\config`
- **热重载**:app 层 `notify` 监听 → 重解析 → 广播生效(FontChain 重建 / 图集失效 / Palette 应用 / 全量重绘);解析失败弹错误提示但保旧配置继续跑(主 spec §8)
- **主题查找**:`theme = name` 在 `include_dir` 嵌入库按文件名匹配(容错大小写与连字符/下划线);未找到报 ParseError

## 6. DECCKM(输入补课,mica-core)

- `Surface::app_cursor_mode() -> bool`(读 `term.mode()` 的 `TermMode::APP_CURSOR`)
- `input::encode` 增加 mode 维度:app 模式下方向/Home/End 发 SS3(`\x1bOA/\x1bOB/\x1bOC/\x1bOD/\x1bOH/\x1bOF`),CSI/tilde 键不变
- 顺带补齐账本:Alt 修饰前缀(`\x1b` + 序列,Alt+Backspace → `\x1b\x7f`)与 alt 位的专测

## 7. 渲染事件化(mica-app)

- 读线程收数据 → `PostMessageW(hwnd, WM_APP_RENDER, ...)`;HWND 生命周期由 app 持有保证
- 主循环 `GetMessageW` 阻塞(M0 的 `PeekMessageW` + 8ms sleep 删除)
- `WM_APP_RENDER` → drain → feed → damage → draw;输入消息处理后按需 draw;`WM_SIZE` 逻辑不变
- 不手写帧节流:消息合并 + Fifo present 各司其职
- app 层改动;交叉 lint(`--target x86_64-pc-windows-msvc`)+ 真机冒烟验证

## 8. 测试策略

- **纯逻辑(mac,全 TDD)**:chain 映射表测、metrics 数学、config 解析器(单键/重复键/坏行/优先级)、**Ghostty 主题库全库遍历解析金样本测试**(嵌入的 300+ 真实文件逐个解析零错误)、palette/OSC 应答、DECCKM 编码表、宽字符与 damage 的 instances 逻辑(用 fake 字形元数据,dwrite 不在测试路径上)
- **windows-only**:dwrite 光栅化、事件循环——交叉 lint + 真机冒烟;WARP PNG golden 留 M2
- **CI**:双平台现有矩阵不变;主题库金样本测试在 mac 跑

## 9. 切片与验收

| 切片 | 内容 | 验收锚点 |
|---|---|---|
| M1a | dwrite 光栅化 + FontChain + Metrics 全流通,font8x8/atlas.rs 退役 | 英文字形正确、变宽字形(如 iiW 对比) |
| M1b | 宽字符渲染 + 回退链 | **中文显示,告别 `?`**;中英混排对齐 |
| M1c | 配置解析器 + 主题库嵌入 + 热重载 + Palette 收敛 + OSC 4/12 | 改主题/字体/字号保存即生效 |
| M1d | DECCKM + 渲染事件化 + 脏区 | vim 方向键正确;空闲时任务管理器 CPU 接近 0%(M0 轮询版为常驻数个百分点) |

依赖:M1a→M1b 线性;M1c 依赖 M1a(FontChain);M1d 与 M1b/c 可并行。M1 完成后按主 spec 进入 M2(窗口体验)。

## 10. 风险与缓解

| 风险 | 缓解 |
|---|---|
| DirectWrite API 面大(COM、分析器布局) | 先核实签名再写(主 spec 纪律);dwrite.rs 隔离在单文件 cfg 门控内;M1a 冒烟最早真机验证 |
| 用户未装 Sarasa | 回退链自动落 Cascadia;README 写安装指引;M3 随安装器分发 |
| damage 行号换算错(scrollback 绝对行 vs 视口行) | 换算逻辑纯函数化 + 专项测试(含 display_offset>0 的滚动场景) |
| 主题库个别文件含 M1 未支持的键 | 解析器对未知键**容忍跳过**(Ghostty 同款行为),金样本测试锁住"零错误" |
| PostMessage 跨线程 HWND 失效 | app 保证读线程先于窗口销毁停止(退出流程显式 join) |
| include_dir 增大二进制 | 300+ 纯文本片段约数百 KB,可接受;实测超预期再考虑 zstd |

## 11. 开放问题

1. **光标样式**(M0 是块状反色):M1 维持块状,beam/underline/闪烁 M2——与主题的 cursor 配色已在 Palette 预留 `cursor` 槽位。
2. **font-size 单位语义**(pt vs px、DPI 缩放):M1a 实现 dwrite 时按 Ghostty 行为定(pt,per-monitor DPI aware),实施计划中核实。
3. **主题暗/亮自适应**(Ghostty 的 `theme = light:xxx,dark:yyy`):M2 随系统主题通知一并做,M1 只支持单主题名。
