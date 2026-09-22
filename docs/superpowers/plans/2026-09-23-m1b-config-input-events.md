# M1-B 配置·输入·事件化 Implementation Plan

- 日期:2026-09-23
- 状态:待执行
- 上游:spec `2026-09-22-m1-render-config-design.md`(§5 配置、§6 DECCKM、§7 事件化、§4 脏区/Palette);主 spec §10 M1 行
- 前置:M1-A 已完成并合入 main(`306af33`);主题库已 vendored(`themes/`,630 个,`f38af3a`)
- 验收锚点(spec §9 M1c/M1d):**改主题/字体/字号保存即生效**;vim 方向键正确;空闲 CPU 接近 0%

## Global Constraints

- `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings` 零输出;`cargo test --workspace` 全绿(Windows 真机跑全量)
- mac 侧交叉 lint:`cargo clippy -p mica-app --target x86_64-pc-windows-msvc --all-targets`
- 新依赖许可(加前查,记入 workspace):`include_dir` 0.7(MIT)、`notify` 8.2(**CC0-1.0**,非 GPL 合规,已核 docs.rs)。禁 GPL 不变
- 主题库来源变更说明:上游 Ghostty 仓库已不随源码分发 themes/(改 release resources);官方库同步源为 `mbadolato/iterm2-color-schemes` 的 ghostty/ 导出(MIT),已从中提取 630 个到 `themes/`(README 记录 provenance)。**D10 决策内容不变,来源落地为此**
- alacritty API 已核实(0.26.0 本地源码):`Term::damage(&mut self) -> TermDamage<'_>`、`Term::reset_damage(&mut self)`、`Term::mode(&self) -> &TermMode`、`TermMode::APP_CURSOR = 1<<1`、`LineDamageBounds { pub line: usize, .. }`——**迭代器产出的 line 即视口行号(0=顶行)**,不可见行被过滤,消费端仅 `< screen_lines` 裁剪
- include_dir 0.7 已核:`static DIR: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../../themes")`,`Dir::files()/get_file(path)`,`File::contents_utf8()`;不需要 glob/metadata feature
- notify 8.2 已核:`notify::recommended_watcher(tx)(mpsc::Sender<Result<Event>>)` + `watcher.watch(path, RecursiveMode::NonRecursive)`;编辑器原子替换会连发多条事件,**必须去抖**(M1 用 300ms 静默窗,不引 debouncer 子 crate)

---

### Task 1: config 解析器(纯逻辑,TDD)

**Files:**
- New: `crates/mica-core/src/config/mod.rs`、`crates/mica-core/src/config/parse.rs`
- Modify: `crates/mica-core/src/lib.rs`(`pub mod config;`)

**Interfaces:**
- Produces: `parse(source) -> (Vec<(String,String)>, Vec<ParseError>)`——有序键值对(保留重复键与出现顺序,font-family 可重复、palette 值为 `0=#rrggbb` 形态)+ 逐行错误;**绝不 panic**

- [ ] **Step 1: 解析器**

```rust
pub struct ParseError { pub line: usize, pub reason: String }

/// Ghostty 语法:KEY = VALUE;整行注释(# 顶行首,前导空白后);空行/尾随空白容忍。
/// 键字母-连字符;值保留原始字符串(合成层解释)。
/// 行内 # 不剥离——值含 #rrggbb 色值,行内注释语义 Ghostty 未承诺,M1 不支持。
pub fn parse(source: &str) -> (Vec<(String, String)>, Vec<ParseError>)
```

规则:无 `=` 或键为空 → `ParseError`(1-based 行号);键/值两侧空白 trim;未知键**不报错**(Ghostty 同款容忍;键集过滤在合成层)。

- [ ] **Step 2: 测试**(表驱动):单键、重复键保序、`#` 整行注释、行内 `#` 保留在值里、缺 `=`/空键报错带行号、CRLF、空行、`key=value` 无空格也合法
- [ ] **Step 3: Commit** `feat(core): ghostty-syntax config parser with per-line errors`

### Task 2: 主题库嵌入 + 三层合成

**Files:**
- New: `crates/mica-core/src/config/theme.rs`、`crates/mica-core/src/config/settings.rs`
- Modify: `crates/mica-core/Cargo.toml`(`include_dir = { workspace = true }`)

**Interfaces:**
- Consumes: T1 `parse`
- Produces: `THEME_LIB: Dir<'static>`、`load_theme(name)`、`Settings`(合成产物)

- [ ] **Step 1: 嵌入与查找**

```rust
pub static THEME_LIB: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../../themes");

/// 名字容错:大小写不敏感、`-`/`_` 互换、剥 .cfg 扩展名。
/// 找不到 → Err(String)(含最接近的 5 个候选名,便于用户排错)
pub fn find_theme(name: &str) -> Result<&'static File, String>
```

- [ ] **Step 2: 合成(默认 < 主题 < 用户)**

```rust
pub struct Settings {
    pub font_families: Vec<String>,   // 空 → DEFAULT_FAMILIES
    pub font_size_pt: f32,            // 默认 12.0
    pub palette: Palette,             // T3;先占位用现 render BASE16 值
    pub theme_name: Option<String>,
}
pub fn resolve(user_source: &str) -> Result<Settings, Vec<ParseError>>
```

合成序:Palette::DEFAULT → 主题片段 pairs → 用户 pairs,后者同名覆盖(重复 `font-family` 追加、`palette = <i>=<hex>` 按 i 写槽)。用户段有 error → Err(保旧由调用方决定);未知键静默跳过。

- [ ] **Step 3: 金样本测试**——`THEME_LIB` **全库遍历**:630 个文件逐个 `parse` 零错误;抽查 Dracula 的 palette 1 = #ff5555;`find_theme("dracula")`/`"tokyo_night"` 容错命中(库内确有该文件才可断言,先 `find` 后写死名)
- [ ] **Step 4: Commit** `feat(core): embedded theme library with layered settings resolution`

### Task 3: Palette 单一来源 + OSC 4/12 应答

**Files:**
- New: `crates/mica-core/src/config/palette.rs`
- Modify: `crates/mica-core/src/surface.rs`、`crates/mica-render/src/color.rs`、`crates/mica-render/src/frame.rs`、`crates/mica-render/src/pipeline.rs`(clear_color)、`crates/mica-app/src/app.rs`(接线)

**Interfaces:**
- Consumes: T2 Settings
- Produces: `Palette` 进 core,render 三处(surface `default_rgb`/`BASE16`/`DEFAULT_BG`)退役

- [ ] **Step 1: Palette 类型**

```rust
pub struct Palette {
    pub colors: [Rgb; 16],
    pub fg: Rgb, pub bg: Rgb, pub cursor: Rgb,
}
impl Palette { pub const DEFAULT: Self /* = 现 BASE16 + 0x1e 底 + 白 fg */ ; fn apply_pair(&mut self, k: &str, v: &str) }
```

- [ ] **Step 2: OSC 应答正确化**(surface.rs)——`ProxyState` 增 `palette: Palette`;`Event::ColorRequest(i, fmt)`:0..=15 → `palette.colors[i]`、256→fg、257→bg、258→cursor(现全答白,tmux/base16 探测误判的欠账);`Surface::set_palette(&Palette)` 热替换;`default_rgb` 删除
- [ ] **Step 3: render 消费**——`color::resolve(color, &Palette)`(BASE16 常量删,`resolve_indexed` 0..=15 走 palette);`frame::build_instances(surface, router, metrics, &Palette)`(DEFAULT_FG/BG 删);`Renderer` 增 `set_clear_color(&Palette)`(app 换肤时调)或 `draw` 参数携带——选前者,一次设置
- [ ] **Step 4: 测试**——Palette apply_pair 覆盖序;OSC 4 查询答 palette 值(驱动 `ColorRequest(1, fmt)` 断言);OSC 12 答 cursor;resolve 走 palette;frame 实例颜色来自 palette(现有测试改构造)
- [ ] **Step 5: Commit** `feat(core): single-source palette; correct OSC 4/12 answers`

### Task 4: 字体配置接线

**Files:**
- Modify: `crates/mica-app/src/app.rs`(FONT_SIZE_PT 常量与 DEFAULT_FAMILIES 直用 → Settings)

**Interfaces:**
- Consumes: T2 `Settings`、T1-A `DwriteRouter::new(font_size_pt, &[&str])`
- Produces: 字体链/字号来自配置

- [ ] **Step 1**:init 读 `%APPDATA%\mica\config`(缺失 = 全默认,不报错)→ `resolve` → `DwriteRouter::new(s.font_size_pt, &families)`;`families: Vec<String>` → `Vec<&str>` 临时借用。resolve 失败:MessageBox 报错误行,回落默认继续跑
- [ ] **Step 2**:交叉 lint + 真机:config 写 `font-size = 16` → 窗口按新度量启动
- [ ] **Step 3: Commit** `feat(app): font chain and size from config`

### Task 5: 热重载

**Files:**
- Modify: `crates/mica-app/Cargo.toml`(+`notify`)、`crates/mica-app/src/app.rs`、workspace `Cargo.toml`

**Interfaces:**
- Consumes: T4 启动路径;notify 8.2(CC0)
- Produces: 保存 config 即生效(重建 router/图集/palette/网格)

- [ ] **Step 1: watcher 线程**(app 层):`recommended_watcher(tx)` + `watch(config_path, NonRecursive)`;线程收事件去抖(300ms 静默窗,手工实现)→ `PostMessageW(hwnd, WM_APP_CONFIG, 0, 0)`。HWND 生命周期:退出流程先置 None 哨兵再停 watcher(参照 T7 退出序)
- [ ] **Step 2: WM_APP_CONFIG 处理**(主线程):重读+resolve;失败 → MessageBox + 保旧;成功 → 重建 `DwriteRouter`(新度量 `set_cell_metrics(.round())`、窗口像素尺寸不变重算网格 resize)→ `set_palette` → `Renderer::set_clear_color` → 全量重绘。旧 router Drop 释放图集自然重光栅化,无需显式清缓存
- [ ] **Step 3: 真机**:改 theme 保存 → 立即换肤;`font-size = 9` → 字变小、列数增多;config 写坏行 → 提示且现配置不动
- [ ] **Step 4: Commit** `feat(app): hot-reload config via notify with 300ms debounce`

### Task 6: DECCKM + Alt 前缀(清账本 #6/#7)

**Files:**
- Modify: `crates/mica-core/src/input.rs`、`crates/mica-core/src/surface.rs`、`crates/mica-app/src/app.rs`(WM_KEYDOWN 传 mode)

**Interfaces:**
- Produces: `Surface::app_cursor_mode() -> bool`;`input::encode(key, mods, app_cursor: bool) -> Vec<u8>`(**顺带清死代码:返回值改 Vec,永远 Some 的 Option 退役**)

- [ ] **Step 1: encode 扩维**——`app_cursor && mods == NONE` 时 Up/Down/Right/Left/Home/End 发 SS3(`\x1bOA..D/H/F`);带修饰仍 CSI(xterm 规范 SS3 无参数,修饰走 CSI)。Alt 前缀:非 NONE 的 alt 且序列非空 → `ESC + bytes`(Alt+Backspace → `\x1b\x7f`,Alt+Enter → `\x1b\r`,Alt+方向 → `\x1b` + CSI,即 xterm meta 编码)
- [ ] **Step 2: surface**——`pub fn app_cursor_mode(&self) -> bool { self.term.mode().contains(TermMode::APP_CURSOR) }`(mode() 返回 &TermMode,已核)
- [ ] **Step 3: app**——WM_KEYDOWN 调 `encode(key, mods, t.term.app_cursor_mode())`
- [ ] **Step 4: 测试**——表驱动补全:app 模式 SS3 六键、app+修饰仍 CSI、Alt 三例、现有用例适配 Vec 返回
- [ ] **Step 5: Commit** `feat(core): DECCKM application cursor keys + alt modifier prefix`

### Task 7: 渲染事件化(去 8ms 轮询)

**Files:**
- Modify: `crates/mica-app/src/app.rs`、`crates/mica-core/src/pty.rs`(读线程发完 EOF/退出事件,可选)

**Interfaces:**
- Produces: 读线程数据 → `WM_APP_RENDER` 唤醒;主循环 `GetMessageW` 阻塞;**PostMessageW 只在 app 层**(pty.rs 零 GUI 纪律:core 不碰 win32)

- [ ] **Step 1: 转发线程**——app 起 thread:loop `rx.recv()`(PtyReader 现有 mpsc)→ `PostMessageW(hwnd, WM_APP_RENDER, 0, 0)`;**消息队列天然帧合并**,多字节到达只触发一次 drain
- [ ] **Step 2: 主循环改写**——`PeekMessageW+sleep(8ms)` → `GetMessageW` 阻塞循环(返回 0 = WM_QUIT 退出);`WM_APP_RENDER` handler:drain 全部 → feed → take_pty_writes 回写 → take_title → draw_frame;`WM_CHAR/WM_KEYDOWN` 后按需 draw(维持现状:输入路径 pty 回显走 WM_APP_RENDER)
- [ ] **Step 3: 退出序**(spec 风险表):WM_DESTROY → 置退出标志 → join 转发线程(它在 PostMessageW 上可能阻塞在满队列;PostMessageW 异步不阻塞,join 安全)→ 现有 STATE.take 显式 Drop 杀 pty → PostQuitMessage
- [ ] **Step 4: 常量**——`const WM_APP_RENDER: u32 = 0x8000 + 1;`(WM_APP 起)
- [ ] **Step 5: 真机**——任务管理器空闲 CPU ≈0%(M0 轮询为常驻数个百分点);快速输出无卡顿
- [ ] **Step 6: Commit** `feat(app): event-driven render loop, retire 8ms polling`

### Task 8: 脏区跟踪

**Files:**
- Modify: `crates/mica-core/src/surface.rs`、`crates/mica-render/src/frame.rs`、`crates/mica-app/src/app.rs`(draw_frame 分路)

**Interfaces:**
- Consumes: `Term::damage()/reset_damage()`(已核签名与视口行号语义)
- Produces: `Surface::take_damage() -> Damage`;`frame::rebuild_rows/repack` 行级重建

- [ ] **Step 1: Surface::take_damage**

```rust
pub enum Damage { Full, Lines(Vec<usize>) } // 行号 = 视口行(0=顶)
pub fn take_damage(&mut self) -> Damage {
    // 迭代 LineDamageBounds,收集 line < screen_lines;空 → Lines(vec![]);
    // term.damage() 首次调用(无历史)自然 Full——用哨兵:has_drawn 标志首帧强制 Full
    // 结束 reset_damage()
}
```

- [ ] **Step 2: frame 结构化**——instances 改两层:`rows: Vec<RowInst { bg: Vec<CellInstance>, glyphs: Vec<CellInstance> }>`(行主序;两遍发射契约在行内保持:bg 先字形后);`build_rows(surface, router, metrics, palette, damage)`:Full 全量、Lines(l) 只重建 l 行(路由缓存兜住重复字形)、无 damage 直接返回旧结构;`repack(&rows) -> Vec<CellInstance>` 平铺(bg 段全行连续在前、字形段随后,与现 shader/管线契约不变)。**平摊成本 ≪ 重建;GPU 仍整缓冲上传(spec 取 90% 收益)**
- [ ] **Step 3: draw_frame**——持 rows 于 `Terminal`;`take_damage()` 分路;repack 后走现有 set_atlas/draw
- [ ] **Step 4: 测试**——feed 文本到两行 → damage 恰含这两行;cls → Full;resize → Full;repack 输出与 T1-A 全量 `build_instances` 逐字节等价(黄金对拍,防两遍发射契约漂移);无输出无输入 → rows 不变(用路由计数 router 断言未调用)
- [ ] **Step 5: 真机**——`dir /s` 长滚动流畅;`cls` 后重打正常;vim 全屏移动无残影
- [ ] **Step 6: Commit** `feat(render): line-level damage tracking with repacked instance buffer`

### Task 9: 整合冒烟(M1c+M1d 验收)

- [ ] **Step 1: 全量验证**——fmt/clippy/test workspace + 交叉 lint;`cargo run --bin mica`

真机清单(全部通过才算 M1-B 完成):

1. `%APPDATA%\mica\config` 写 `theme = Dracula` 保存 → 即时换肤,16 色+底色+光标全换
2. `font-size = 16` 保存 → 字号即时变,窗口尺寸不变、网格重排
3. config 坏行(如 `foo`)保存 → 错误提示,现配置不动
4. `printf '\e]4;1;?\e\\'` 类探测(或 base16-shell/tmux 探测脚本)不误判——OSC 4 答主题色而非全白
5. vim 打开 → 方向键移动正确(DECCKM SS3);`vim` 内 Home/End 正常
6. Alt+Backspace/Alt+B 词删除(pwsh PSReadLine)正常
7. 空闲时任务管理器 CPU ≈0%(事件化生效)
8. `dir /s` 长输出流畅、无残影;`cls` 后重打正常(脏区正确性)
9. 未装 Sarasa 回退 Cascadia 不变;`themes/README.md` provenance 可查
10. 推送后 CI 双平台绿;README 状态推进 M1c/M1d

- [ ] **Step 2: 文档收尾**——CLAUDE.md"当前边界"清账(OSC 4/12、DECCKM、Alt、轮询、脏区五笔下账;8/16 硬编码残留核查);README 里程碑表 M1 全 ✅
- [ ] **Step 3: Commit + push** `docs: M1-B complete, ledger updated`(分支 `m1b-config`,PR 流程同 M1-A)

---

## 执行顺序与依赖

T1 → T2 → T3(线性,palette 是 T2 合成的槽位但类型定义在 T3,实现时 T3 的 `Palette` 先行或 T2 占位默认——**取前者:T3 的 Step1 提前进 T2 的 Step2 之前**,避免占位返工)。T4/T5 依赖 T2;T6 独立可先行;T7 独立;T8 依赖 T3(签名)与 T7(事件化后才有"无消息=无重建"语义)。T9 最后。

## 已知风险与执行者注意

- **PostMessageW 跨线程 HWND 失效**(spec 风险表):退出序按 T7 Step3;转发线程在窗口销毁后 Post 到死句柄只是失败返回,无害
- **damage 与光标**:光标行移动/闪烁是否入 damage 由 alacritty term 内部决定;冒烟若见光标残影,在 take_damage 里把"上一帧光标行 + 当前光标行"并入 Lines(便宜且保守),不算设计变更
- **include_dir 二进制体积**:630 文本约数百 KB,可接受;超预期再议压缩(spec §10)
- **notify 事件风暴**:编辑器原子替换 = Delete+Create+Write 连发,去抖窗收尾再读文件;读不到(瞬时缺失)跳过本轮
- **`$CARGO_MANIFEST_DIR/../../themes` 路径**:mica-core 的 manifest 是 `crates/mica-core`,回两级到仓库根的 `themes/`;若 Cargo 报路径不存在,先确认 themes/ 在位(已 vendored,f38af3a)
- **palette 值格式**:主题库统一 `palette = <0-15>=#rrggbb`;用户 config 手写 `background = #rrggbb` 等键与之并存,apply_pair 按键名分派

## Self-Review 记录

- 覆盖:spec §5(T1/T2/T4/T5)、§4 Palette/OSC(T3)、§6(T6)、§7(T7)、§4 脏区(T8)、§9 验收锚点(T9)✓
- API 全部本地源码核实:alacritty damage/mode/APP_CURSOR/LineDamageBounds、include_dir 0.7、notify 8.2、iterm2-color-schemes ghostty/ 630 文件已入库 ✓
- 契约延续:两遍发射(T8 repack 对拍锁死)、set_atlas revision 门控(T5 重建 router 自然重传)、非 sRGB surface、`set_cell_metrics` 取整(I3)✓
- 账本清偿:OSC 4/12(§T3)、DECCKM+Alt(§T6)、8ms 轮询(§T7)、每帧全量(§T8)、调色板三处(§T3)——M0 复查账本与 CLAUDE.md 边界全部下账 ✓
