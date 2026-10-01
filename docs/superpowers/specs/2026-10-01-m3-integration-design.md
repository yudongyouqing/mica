# M3 系统集成设计文档(Profile/IPC/Quick Terminal/安装/defterm)

- 日期:2026-10-01
- 状态:设计已经所有者确认("全按推荐",2026-10-01 六点拍板)
- 上游:主 spec `2026-09-20-win-ghostty-design.md`(§7 系统集成、§10 M3 行、§11 开放问题 ARM64)
- 验收锚点(M3 完成定义):**别人能装能用**——安装包双击安装、winget 可装、开始菜单/桌面有入口;设置"默认终端"能选 Mica;`mica wsl` 直接进发行版;`mica new-tab` 在既有窗口开标签;全局热键下拉 Quick Terminal

## 1. 范围(全量 M3)

1. Profile 体系:PowerShell/cmd/WSL 发行版枚举(启动目录/profile 选择)
2. jump list:任务栏右键直达(PowerShell/cmd/WSL 各一项 + 最近 profile)
3. 单实例 + named pipe IPC:`mica new-tab`(既有窗口开标签)、`mica wsl [distro]`、重复启动聚焦既有窗口
4. Quick Terminal:全局热键 `Win+``,顶部下拉、失焦自动收起、`quick-terminal-key` 可配置
5. 安装包:WiX v4 MSI(x64 + ARM64 矩阵)+ winget 清单;开始菜单快捷方式 + 桌面可选 + 卸载干净
6. defterm 第一级:RRVA 注册表——出现在 Windows"默认终端"选择列表
7. 图标:程序化生成占位 .ico(多分辨率 16/24/32/48/64/128/256),正式品牌图标后换

**非目标(M3 不做)**:defterm 完整 ITerminalHandoff COM 接管(M3c 末尾探针,成本超预期留 M4)、资源管理器右键"在此处打开"(winget/安装包外挂注册表,记 M4 打磨)、品牌域名(所有者线下事务)、自动更新(winget 管)、每标签独立字体(M3a profile 携带 shell/目录,字体键 M4 随 profile 完全体)。

## 2. 关键决策记录

| # | 决策 | 选择 | 被否选项与理由 |
|---|------|------|----------------|
| D21 | 切片 | M3a Profile/IPC/jumplist → M3b Quick Terminal → M3c 安装/defterm | 依赖正交:IPC 是 new-tab/QuickTerm/CLI 三者地基,先行;安装最后(前面的入口才齐) |
| D22 | WSL 枚举 | **`wsl.exe --list --all` 子进程 + UTF-16LE 解析**(启动用 `wsl.exe -d <name>`) | 否决 wslapi.dll FFI:windows-rs 0.62 无绑定,手写成本与维护面大;子进程法 WT 同款,wsl 不装时自然降级(枚举空) |
| D23 | 安装包 | **WiX v4 MSI(x64+ARM64)+ winget 清单** | 否决 MSIX(COM/defterm 注册特殊、重打包管线复杂);否决 NSIS(无升级/卸载语义、企业部署弱) |
| D24 | defterm 分级 | 一级:RRVA 注册表(`HKCU\...\Console\DelegationConsole/DelegationTerminal` 指向 Mica,设置页可选);二级:`ITerminalHandoff` COM 完整接管——**M3c 探针,超成本留 M4** | 否决直接手绑 COM:0.62 无绑定需逐字对 SDK 头,风险大;一级覆盖 95% 场景(用户在设置里选) |
| D25 | Quick Terminal 键 | 默认 `Win+```(WT 同款),`quick-terminal-key = <trigger>` 可配置;失焦(WM_ACTIVATE deactivate)自动收起;下拉动画 M3b 打磨票(先瞬切) | 否决默认禁用:Quick Terminal 的核心价值就是热键;否决自绘动画先行:窗口几何语义(顶栏保留/去)先跑通 |
| D26 | 图标 | 程序化生成(构建脚本画 7 分辨率 M 方块主题色 .ico)嵌入 exe + 安装包 | 否决等待正式图标:M3 验收"别人能装能用"需要图标占位;品牌资产到后热替换 .ico 文件即可 |

## 3. Profile 体系(M3a)

```rust
// mica-core/src/profile.rs(纯逻辑,TDD)
pub struct Profile {
    pub name: String,          // 显示名("PowerShell"、"Ubuntu-22.04")
    pub command: String,       // 启动命令行(wsl.exe -d ... / powershell.exe / cmd.exe)
    pub icon_hint: String,     // strip 显示用(纯文本,字形路由)
}
pub fn scan_profiles() -> Vec<Profile>          // 静态(PowerShell/cmd)+ wsl 枚举
pub fn scan_wsl() -> Vec<Profile>               // wsl.exe --list --all 解析(UTF-16LE → '\0' 切分 → 跳过表头)
```

- WSL 无(未装/无发行版)→ 列表只含静态项,静默降级
- pty 侧:`PtySession::spawn` 已接受任意命令 ✓(M0 起就参数化);启动目录(M3a 只支持 home,`starting-directory` 配置键 M4)

## 4. 单实例 + named pipe IPC(M3a)

- **管道名** `\\.\pipe\mica-<用户 SID>`(CreateNamedPipeW,单实例 DUPLEX;windows-rs System::Pipes ✓ 已核实)
- 首实例:窗口创建后起 pipe 监听线程(accept 循环);客户端连接超时 → 自己启动
- **协议**:一行 JSON(长度前缀 u32 LE + payload):`{"op":"new-tab","profile":"Ubuntu-22.04"}` / `{"op":"activate"}`;响应 `{"ok":true}` 后客户端退出
- 服务端收到 → `PostMessageW(WM_APP_IPC)`(主线程处理,复用 start_tab + profile 启动)
- **CLI**(`main.rs` 解析参数后分发):
  - 无参 → 单实例检查(管道连得上 → 发 activate 退出;否则正常启动)
  - `mica new-tab [profile]` / `mica wsl [distro]` → 发 IPC 退出
- 多标签第一个 pane 的 shell 由 `--profile` 或 config `default-profile` 决定(默认 PowerShell)

## 5. jump list(M3a)

- `ICustomDestinationList::SetAppID` 后 AddUserTasks(PowerShell/cmd/各 WSL 一项,参数 `mica new-tab <profile>`)
- AUMID(AppUserModelId)= `Mica.Terminal`(安装包与进程 SetCurrentProcessExplicitAppUserModelID 同源——任务栏分组与通知的地基)

## 6. Quick Terminal(M3b)

- `RegisterHotKey(hwnd, 1, MOD_WIN, VK_OEM_3)`(``` 键;失败弹窗提示改键)→ WM_HOTKEY
- 专用窗口:无标题栏高度 40% 屏宽 100%,顶部 y=0,WS_POPUP + 顶边常驻(不抢任务栏图标:扩展样式 TOOLWINDOW)
- 切换逻辑:可见 → 隐藏;不可见 → 显示 + 激活(居顶);WM_ACTIVATE( deactivated)→ 隐藏(失焦收起)
- 热键可配:`quick-terminal-key = win+oem_3`(复用 keymap::parse_trigger,mods 支持 win 位需扩展——M3b 增 `win` 修饰符)
- QuickTerm 窗口复用全部 Terminal/GPU 管线(独立 TabState,不走主窗口 strip;单 pane)

## 7. 安装包与 defterm(M3c)

- **WiX v4**(cargo wix 或独立 wix.proj;CI windows job 追加 `cargo wix` 打包 artifact)
- 组件:exe(含 themes 嵌入,无外部文件)、开始菜单快捷方式、桌面(可选)、卸载注册、**RRVA defterm 键写 HKCU**(per-user MSI;卸载清除)
- winget 清单(独立 yaml 提交 microsoft/winget-pkgs,M3c 手动首提)
- ARM64:CI 矩阵追加 `aarch64-pc-windows-msvc` 编译检查(target-add;打包失败不阻塞——D26 探针精神)
- **defterm RRVA**:写 `HKCU\Software\Microsoft\Windows\CurrentVersion\Console\DelegationConsole={MicaConsoleCLSID}`、`DelegationTerminal={MicaTermCLSID}`(GUID v5 定值,代码常量;外加 HKCU Classes CLSID 注册 ExeId/Name 显示名)。卸载时仅在值仍指向 Mica 时清除
- **图标**:build.rs 画 7 分辨率 BMP(主题色底 + 白 M 字母,纯字节操作)合成 .ico;winres 嵌 exe 资源

## 8. 测试策略

- **纯逻辑(mac TDD)**:WSL 输出解析器(真实样例字节:UTF-16LE/表头/尾随 \0)、IPC 协议帧编解码、profile 扫描(注入假 wsl 输出)、jump list 参数构造、热键触发器解析(win 修饰符)
- **windows-only 真机**:管道双进程往返(new-tab 真开标签)、RegisterHotKey、QuickTerm 失焦收起、安装/卸载(VM 或冒烟手动)、defterm 设置页出现
- CI:现有矩阵不变;windows job 追加 wix 打包(artifact);ARM64 追加 check

## 9. 切片与验收

| 切片 | 内容 | 验收锚点 |
|---|---|---|
| M3a | profile 扫描 + WSL + named pipe IPC + CLI + jump list | `mica new-tab Ubuntu` 既有窗口开 WSL 标签;任务栏右键直达 |
| M3b | Quick Terminal(热键/失焦收起/独立窗口) | Win+` 任意程序下拉/收起,重启后热键仍在(本会话注册即可,开机自启 M4) |
| M3c | WiX 安装包 + 图标 + RRVA defterm + winget | 双击 MSI 装完开始菜单有 Mica;设置"默认终端"列表出现 Mica;卸载干净 |

## 10. 风险与缓解

| 风险 | 缓解 |
|---|---|
| RRVA 键在不同 Win11 版本行为差异 | 键值照抄 WT 注册表形态(只读参照);冒烟只验收"设置列表出现",接管行为不承诺 |
| ITerminalHandoff 手绑失败(M3c 探针) | D24 分级:一级已覆盖用户主动选择;二级失败记 M4 边界,不阻塞 M3 |
| wsl.exe 输出格式随版本漂移 | 解析器容错(解析失败 → 空列表静默降级,不 panic);真实样例字节测试锁格式 |
| QuickTerm 与全屏应用抢焦点 | RegisterHotKey 系统 Ctrl 替代逃生键(M3b 冒烟定);失焦收起自愈 |
| cargo wix 工具链在 CI 不稳 | 打包独立 job 失败不红主 CI(artifact optional) |

## 11. 开放问题

1. Quick Terminal 是否随开机自启(托盘常驻)——M4 随设置 GUI 一并做;M3b 仅运行期
2. 资源管理器右键"在此处打开"——M4(注册表 Directory\shell 挂 mica)
3. 正式品牌图标与域名——所有者资产,到位后热替换
4. `starting-directory`/`--starting-directory`——M4 profile 完全体
