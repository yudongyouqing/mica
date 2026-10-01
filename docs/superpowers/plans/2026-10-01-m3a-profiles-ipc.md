# M3a Profile·IPC·Jump List Implementation Plan

- 日期:2026-10-01
- 状态:待执行
- 上游:spec `2026-10-01-m3-integration-design.md`(§3 Profile、§4 IPC、§5 jump list;D21/D22)
- 验收锚点(spec §9 M3a):`mica new-tab Ubuntu` 既有窗口开 WSL 标签;任务栏右键直达 profile

## Global Constraints

- fmt/clippy `-D warnings` 零输出;workspace 测试全绿;交叉 lint
- **API 已核实(windows 0.62.2 本地源码)**:
  - `CreateNamedPipeW(lpname, dwopenmode, dwpipemode, nmaxinstances, noutbuffersize, ninbuffersize, ndefaulttimeout, lpsecurityattributes) -> HANDLE`(System/Pipes/mod.rs:35;**feature `Win32_System_Pipes` + 管道读写用 `Win32_Storage_FileSystem` ReadFile/WriteFile + `Win32_System_Threading` ConnectNamedPipe/DisconnectNamedPipe**——执行时以编译器为准补 feature)
  - `ICustomDestinationList`(UI/Shell/mod.rs:17296;feature `Win32_UI_Shell`)
  - WSL:无绑定 → `wsl.exe --list --all` 子进程(std::process::Command;**输出 UTF-16LE**:stdout 非 UTF-8,需按 u16 解码)
- named pipe 名:`\\.\pipe\mica`(单用户单机,免 SID——桌面场景并发实例非需求)
- 协议帧:`u32 LE 长度 + JSON 单行`(serde_json 已在依赖树?**否——新增 serde/serde_json workspace 依赖,MIT/Apache ✓ 合规**)
- 行为红线:IPC 服务线程绝不持锁跨 PostMessageW;WSL 解析失败 → 空列表静默降级;管道已存在(首实例)时二次启动必须聚焦既有窗口而非报错

---

### Task 1: Profile 扫描(纯逻辑 TDD)

**Files:** New `crates/mica-core/src/profile.rs`;Modify lib.rs

- [ ] **Step 1: 类型与静态项**

```rust
pub struct Profile {
    pub name: String,       // 显示名
    pub command: String,    // 启动命令行(整串)
}
pub const POWERSHELL: fn() -> Profile;  // "PowerShell" / powershell.exe -NoLogo
pub const CMD: fn() -> Profile;         // "Command Prompt" / cmd.exe
pub fn scan_static() -> Vec<Profile>;
```

- [ ] **Step 2: WSL 输出解析器(核心纯函数)**

```rust
/// wsl.exe --list --all 的 stdout(UTF-16LE 原始字节)→ 发行版名列表。
/// 格式:BOM + 表头行("Windows Subsystem for Linux distributions:"或本地化)
/// + 每行一个名字 + 尾随 \r\n\0;默认发行版带 *。
pub fn parse_wsl_output(raw: &[u8]) -> Vec<String>
```

规则:按 u16 逐对解码 → BOM 剔除 → 行切分 → 跳过表头(含 "distribut" 不区分大小写探测)/空行/`*` 前缀剥离 → 名字行;解析层不执行任何 IO。

- [ ] **Step 3: 组合扫描**

```rust
/// 全量:静态项 + WSL(子进程失败/超时 3s → 静默空)。
pub fn scan_all() -> Vec<Profile>  // core 提供 parse;IO 组合在 app 侧(零 GUI 纪律下 std::process 不算 GUI,放 core 亦可——放 core,cmd 唤起可测)
```

- [ ] **Step 4: 测试**:真实样例字节(英文表头/带 * 默认/空输出/纯 BOM/截断对)表驱动;`scan_static` 断言
- [ ] **Step 5: Commit** `feat(core): profile scan with wsl.exe output parser`

### Task 2: IPC 协议与管道服务端

**Files:** New `crates/mica-app/src/ipc.rs`;Modify Cargo.toml(features)

- [ ] **Step 1: 协议帧(纯逻辑,mica-core 或 app 侧均可——放 core)**

```rust
pub struct IpcMessage { pub op: String, pub profile: Option<String> }  // op: "new-tab" | "activate"
pub fn encode(msg: &IpcMessage) -> Vec<u8>;     // u32 LE len + json
pub fn decode(buf: &[u8]) -> Option<IpcMessage>;
```

serde 派生(serde = { workspace = true, features = ["derive"] };serde_json)。

- [ ] **Step 2: 管道服务线程(app)**

```text
CreateNamedPipeW("\\\\.\\pipe\\mica", PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
                 PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT, 1, 4KB, 4KB, 0, None)
失败(ERROR_ACCESS_DENIED = 已有实例)→ 返回 AlreadyRunning 信号
循环:ConnectNamedPipe → ReadFile(8B 长度 + payload)→ decode
     → PostMessageW(hwnd, WM_APP_IPC=0x8003, profile-ptr, 0)(字符串经 Box::into_raw 传主线程,主线程 Box::from_raw 收)
     → WriteFile 响应 → DisconnectNamedPipe
```

主线程 `WM_APP_IPC`:按 profile 起新 tab(start_tab 扩展 profile 参数);无 profile → 仅 SetForegroundWindow。

- [ ] **Step 3: 测试**:encode/decode 往返、坏帧、长度截断;真机双进程往返
- [ ] **Step 4: Commit** `feat(app): named-pipe IPC server with json protocol`

### Task 3: CLI 与单实例

**Files:** Modify `crates/mica-app/src/main.rs`(现为直调 run())

- [ ] **Step 1: 参数分发**:`mica`(启动,管道占用失败→发 activate→退出)、`mica new-tab [profile]`、`mica wsl [distro]`(`wsl` 无 distro 参数→列发行版退出)、`mica list-profiles`
- [ ] **Step 2: 客户端连接(3s 超时)**:WaitNamedPipeW 探测 + CreateFileW 连接;连不上(new-tab/wsl)→ 自己启动并带 profile
- [ ] **Step 3: AUMID**:`SetCurrentProcessExplicitAppUserModelID(L"Mica.Terminal")`(jump list/任务栏分组地基)
- [ ] **Step 4: 真机**:双开聚焦;`mica wsl` 列表;`mica new-tab "Command Prompt"` 开 cmd 标签
- [ ] **Step 5: Commit** `feat(app): single-instance cli with new-tab/wsl subcommands`

### Task 4: Jump List

**Files:** New `crates/mica-app/src/jumplist.rs`

- [ ] **Step 1: ICustomDestinationList COM 序列**:`CoCreateInstance` → `BeginList` → `AddUserTasks`(每 profile 一个 IShellLinkW:路径 = 当前 exe、参数 = `new-tab <profile>`、图标 = exe)→ `CommitList`(SAFELY 注释齐全;失败静默 eprintln)
- [ ] **Step 2: 注册时机**:首个窗口创建后调用一次;profile 扫描为空时仅静态项
- [ ] **Step 3: 真机**:任务栏右键 Mica 见 PowerShell/cmd/WSL 项,点击开对应标签
- [ ] **Step 4: Commit** `feat(app): jumplist with profile tasks`

### Task 5: TabState 支持 profile 启动

**Files:** Modify `crates/mica-app/src/app.rs`(create_pane/start_tab)

- [ ] **Step 1**:`create_pane(hwnd, profile: Option<&Profile>)`——pty 命令 = profile.command 或 default_shell_command();标题占位 = profile.name 或 "PowerShell"
- [ ] **Step 2**:`TAB_SEED` 不动(字体);启动 profile 由调用点注入(CLI 参数 → TLS `START_PROFILE`;WM_APP_IPC 直接传)
- [ ] **Step 3: 真机**:三个 profile 各开一 tab 并存
- [ ] **Step 4: Commit** `feat(app): profile-driven pane startup`

### Task 6: 整合冒烟(M3a 验收)

- [ ] 全量 fmt/clippy/test;清单:

1. `mica` 启动 → 再 `mica` → 聚焦既有窗口(不双开)
2. `mica new-tab "Command Prompt"` → 既有窗口新 cmd 标签;`mica wsl <发行版>` 同理(有 WSL 时)
3. `mica list-profiles` 打印列表
4. 任务栏右键:profile 项直达
5. 无 WSL 机器:列表只静态项,无报错(静默降级)
6. 管道残留(强杀进程后)重启不阻塞(FILE_FLAG_FIRST_PIPE_INSTANCE 竞争窗口冒烟)
7. CI 双平台绿;CLAUDE.md 边界更新(WSL IPC 记账)

---

## 执行顺序

T1(profile)→ T2(IPC 服务端)→ T5(接线 profile 启动,早于 CLI)→ T3(CLI 客户端)→ T4(jumplist)→ T6(冒烟)。T2 依赖 T1 的消息类型;T5 插在 T3 前避免 CLI 空转。

## 风险与执行者注意

- **wsl.exe stdout 是 UTF-16LE**:std Command 直接 String::from_utf8 必炸;拿 Vec<u8> 原始字节交 parse_wsl_output。真机样例字节先抓一份存进测试
- **WM_APP_IPC 跨线程字符串所有权**:Box::into_raw/from_raw 成对;主线程处理失败路径也必须收
- **管道 FIRST_PIPE_INSTANCE 的竞争窗口**(两实例同时启动):CreateNamedPipeW 都成功 → 后者 CreateFileW 连前者成功即让位;冒烟第 6 条覆盖
- **ICustomDestinationList 参数多**:0.62 签名以编译器为准(SAFETY 注释;COM 结构非 Copy 按引用借——M2b 老坑)
- **CoInitialize**:jumplist 前需 CoInitializeEx(MTA/STA 任一;进程级一次)

## Self-Review 记录

- 覆盖 spec §3/§4/§5/§9-M3a 全项 ✓;D22(子进程解析)/D21(IPC 先行)落地 ✓
- 依赖新增仅 serde/serde_json(MIT/Apache ✓);WSL 降级语义三层防御(解析容错/超时/空列表)✓
- 单实例语义与 QuickTerm(M3b 复用 activate 路径)对齐 ✓
