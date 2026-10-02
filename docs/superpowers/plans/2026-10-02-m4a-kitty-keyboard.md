# M4a kitty Keyboard·bracketed Paste Implementation Plan

- 日期:2026-10-02
- 状态:待执行
- 上游:spec `2026-10-02-m4-protocol-design.md`(§3 kitty、§4 paste;D27)
- 验收锚点(spec §10 M4a):nvim 键位全兼容(方向/shift/ctrl/release);粘贴带 200~ 边界

## Global Constraints

- fmt/clippy `-D warnings` 零输出;workspace 测试全绿;交叉 lint
- **API 已核实(0.26 本地源码)**:
  - `Config { kitty_keyboard: bool, .. }`(term/mod.rs:349-350)——`Surface::new` 传 true 即激活上游状态机(mode 栈/查询应答/push-pop)
  - `TermMode` kitty 位:`DISAMBIGUATE_ESC_CODES = 1<<18`、`REPORT_EVENT_TYPES = 1<<19`、`REPORT_ALL_KEYS_AS_ESC = 1<<21`、`REPORT_ASSOCIATED_TEXT = 1<<22`(mod.rs:75-85);`Term::mode() -> &TermMode` 可读
  - `TermMode::BRACKETED_PASTE = 1<<4`(mod.rs:61)
  - 上游**无输入编码器**(keybindings 在 alacritty 主仓,terminal crate 不含)——自建,这正是主 spec §86 的 protocol/ 增量
- 协议事实(kitty 规范,编码器实现依据):
  - 序列形态 `CSI key-code[:modifiers[:event-type]] [; text] u`;键码 = unshifted Unicode 或功能键号(Enter 13/Tab 9/Esc 27/Backspace 127/Up..Right 57352..57357/Home/End/PageUp/Down 57358..57361/Ins 57364/Del 57363/F1-F12 57376..57387)
  - modifiers = 1 + shift(1)+alt(2)+ctrl(4)+super(8)
  - event-type:1 press(缺省)/2 repeat/3 release
  - **event-type/repeat 只在 REPORT_EVENT_TYPES 开启时发送**;all-keys-as-esc 开启时普通可打印字符也编码(否则直通文本)
  - associated text 仅 press 携带(且须与 keycode 一致——unshifted 原则:shift+1 的 text 是 "!" 而 keycode 仍是 '1')
- 行为红线:legacy 路径(keymap 拦截/`input::encode`)必须原样保留——kitty 位全关时行为与 M3 逐位一致;Ctrl+字母在 kitty 模式**不**走 WM_CHAR 控制字节(app 分流)

---

### Task 1: kitty 编码器(纯逻辑 TDD,核心)

**Files:** New `crates/mica-core/src/protocol/mod.rs`、`crates/mica-core/src/protocol/kitty.rs`

- [ ] **Step 1: 类型**

```rust
/// 输入事件类别(kitty event-type 2/1/3)。legacy 路径只认 Press。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind { Press, Repeat, Release }

/// TermMode 的 kitty 位子集(与上游位值锁死,有对齐断言)。
pub struct KittyFlags(u32);

/// key + mods + kind → kitty CSI-u 序列。
/// flags 决定:是否编码普通字符(all-keys)、是否带 event-type、是否带 text。
/// 返回 None = 该事件在当前 flags 下不该走 kitty(如 flags 全关/不可编码键)
///   ——调用方回 legacy。
pub fn kitty_encode(key: Key, mods: Mods, kind: EventKind, flags: KittyFlags) -> Option<Vec<u8>>
```

- [ ] **Step 2: keysym 表**(生成函数):`fn keysym(key: Key) -> Option<u32>`——可打印字符用 `c as u32`(unshifted:取小写/数字形态);功能键按协议常量;不可编码(现在 Key 枚举内的全部都覆盖)→ None
- [ ] **Step 3: 修饰计算**:`1 + shift + alt*2 + ctrl*4 + win*8`;text 关联:Press 且 REPORT_ASSOCIATED_TEXT 且 mods 无 ctrl/alt(协议建议;ctrl/alt 组合的 text 语义各家不一,保守不发)
- [ ] **Step 4: 与上游位值锁死**:`const _: () = assert!(1 << 18 == ...)` 不可直接(位在 TermMode 私有)——改用运行时测试:构造 kitty mode feed `CSI >1u` 后 `term.mode()` 断言 flags 提取函数正确
- [ ] **Step 5: 测试表**(表驱动,目标 ≥25 case):
  - 四 flag 各自开/关 × 方向键/字母/功能键/Esc/Enter/Backspace
  - all-keys off:'a' → None(legacy 直通);on → `CSI 97;;1u` 形态(注意 all-keys 开时 press 无 text 也要 event-type=1 显式?按规范:report-event-types 未开时 event-type 字段整个省略)
  - REPORT_EVENT_TYPES:release 'a' → `CSI 97:5:3u`(ctrl+release);off → None(release 事件丢弃)
  - shift+1:text "!"、keycode 49、mods 含 shift
  - Enter/Tab/Esc/Backspace 功能键号正确
- [ ] **Step 6: Commit** `feat(core): kitty keyboard csi-u encoder with flags matrix`

### Task 2: Surface 协议状态透出 + bracketed paste

**Files:** Modify `crates/mica-core/src/surface.rs`

- [ ] **Step 1: kitty 配置开**:`Surface::new` 的 Config 增 `kitty_keyboard: true`(常量置位;上游自动答查询/维护栈)
- [ ] **Step 2: 透出**:`pub fn kitty_flags(&self) -> KittyFlags`(从 term.mode() 提取四 位);`pub fn bracketed_paste_active(&self) -> bool`(BRACKETED_PASTE 位)
- [ ] **Step 3: 测试**:feed `CSI >1u`(push disambiguate)→ flags 变化;feed `CSI <u`(pop)恢复;`CSI ?2004h` → paste active 切换
- [ ] **Step 4: Commit** `feat(core): surface exposes kitty flags and paste mode`

### Task 3: app 键位路由(kitty 分流)

**Files:** Modify `crates/mica-app/src/app.rs`(WM_KEYDOWN/WM_CHAR)

- [ ] **Step 1: WM_KEYDOWN**:路由前置——读焦点 pane 的 `kitty_flags()`;非零 → 调 `kitty_encode`(kind=Press;Repeat 用 WM_KEYDOWN lparam bit 30 判重复按下);None → 现有 keymap/encode 路径。**Ctrl+字母的 WM_CHAR 分流**:kitty flags 非零时 WM_CHAR 的控制字节(C0)不直写 pty,改为并入 kitty 编码路径(WM_KEYDOWN 侧已发,WM_CHAR 丢弃;键序实测对齐——冒烟第 3 条)
- [ ] **Step 2: WM_KEYUP 事件跟踪**:目前窗口不收 KEYUP(RegisterWindowMessage 无需)——`wndproc` 增 `WM_KEYUP` 分支:kitty REPORT_EVENT_TYPES 开 → kitty_encode(kind=Release) 写 pty;否则丢弃
- [ ] **Step 3: WM_CHAR 修正**:kitty active 且字节 < 0x20(控制字符)→ return(避免双发);可打印(含 IME/代理对)照旧
- [ ] **Step 4: QuickTerm 同款路由**(quickterm.rs WM_KEYDOWN/CHAR 同样分流——flags 读 QT 自己的 Surface)
- [ ] **Step 5: 真机**:nvim 开 kitty(`:let &t_ku=..` 或现代 nvim 自动):方向/shift+/ctrl+v 组合/释放全对;`cat -v` 观察 `CSI 97:5u` 形态
- [ ] **Step 6: Commit** `feat(app): route input through kitty encoder when flags active`

### Task 4: bracketed paste 接线

**Files:** Modify `crates/mica-app/src/app.rs`(Ctrl+V 路径)

- [ ] **Step 1**:粘贴前查 `bracketed_paste_active()`:开 → 包 `ESC[200~` + 归一文本 + `ESC[201~`(包裹在 CRLF 归一**之后**——边界序列不能被归一改写);关 → 原样
- [ ] **Step 2: 测试**:core 侧 mode 位切换已有(T2);app 侧真机 bash/pwsh `bracketed-paste on` 粘贴多行不执行只有一块(shell 侧行为)
- [ ] **Step 3: Commit** `feat(app): bracketed paste wrapping`

### Task 5: 冒烟清单(M4a 验收)

- [ ] 全量 fmt/clippy/test;`cargo run --bin mica`

1. nvim(或 `nvim --clean` + `:set termguicolors`)自动进 kitty 模式;插入模式方向键移动光标(无 B/D 字母插入)✓
2. nvim Normal 模式 `Ctrl+V` 块选(不是 paste)✓;`Shift+A` 插入尾缀 ✓
3. PowerShell 下按字母/ctrl+c(中断)/ctrl+v(粘贴)不双发、不漏发(`cat -v` 抽查)
4. bash(WSL)`bind 'set bracketed-paste on'` 粘贴多行:一块不执行,回车才跑 ✓
5. kitty 关闭的应用(cmd.exe legacy):一切照 M3 行为(Ctrl+字母控制字节)✓
6. 键位冲突回归:Ctrl+Shift+T/W(标签)、Alt+方向(焦点)在 kitty active 时仍走 keymap(kitty 编码只拦终端内键,app 键位优先级在前)✓
7. CI 双平台绿

---

## 执行顺序

T1(编码器)→ T2(Surface)→ T3(app 路由,大头)→ T4(paste)→ T5(冒烟)。

## 风险与执行者注意

- **WM_CHAR/WM_KEYUP 双发**:Windows 对 Ctrl+字母既发 KEYDOWN+CHAR;kitty 模式下 KEYDOWN 已编码完整序列,CHAR 的控制字节必须丢;可打印字符(含 shift 组合)相反只信 CHAR(text 路径完整)。规则:**C0 控制字节 → KEYUP/KEYDOWN 侧管;≥0x20 → CHAR 侧管**;冒烟第 3 条双验
- **Repeat 判定**:WM_KEYDOWN lparam 位 30(上一 down 未弹);首次按不置位
- **keymap 优先级**:keymap 查表在 kitty 分流**之前**(Ctrl+Shift+T 是 app 键,kitty active 也不该变成终端内编码)——现路径已是 keymap 先,保持
- **QuickTerm 的 flags 独立**:QT 有自己的 Surface/mode 状态;不共享主窗 flags
- **Sidecar(D29)不在本切片**:M4a 只做键盘;2026/OSC8/133 是 M4b

## Self-Review 记录

- 覆盖 spec §3(kitty 全链)/§4(paste)/§10-M4a 锚点 ✓
- 上游零改动依赖:状态机用现成(Config flag),编码器独立纯函数 ✓
- legacy 保真红线:M4a 全关路径 = M3 行为(测试 + 冒烟第 5/6 条)✓
