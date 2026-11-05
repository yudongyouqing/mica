# Mica 全面完善与性能、内存治理计划

> 交接对象：GLM。按任务依赖逐项执行，每个任务单独提交并给出验证证据；不要一次重写整个项目。实施时使用 executing-plans 工作流逐任务推进。

**目标：** 将当前功能较完整的 Windows 原生终端，完善为可长期稳定运行、资源消耗有上限、行为可验证、发布可追溯的日常工具。

**架构：** 保持 `mica-app → mica-render → mica-core` 单向依赖。先修复进程、窗口、IPC、字节流和图集的正确性，再优化调度、CPU、GPU 与内存，最后完善体验和发布；每阶段可单独验收。

**技术栈：** Rust 2024、Win32、ConPTY / portable-pty、alacritty_terminal、DirectWrite、wgpu、Ghostty 风格配置。

**审查基线：** 2026-11-01；本地 `dev`，commit `c6ec62b`。这是本地源码审查与本机测试结果，不代表已检查线上 issue、远端分支保护、实际 Release 附件或其他电脑。

---

## 1. 当前判断

项目已具备多标签、分屏、多窗口、Quick Terminal、配置热更新、主题、中文输入、DPI、搜索、导出和 IPC 等主要能力。下一阶段应以正确性和长期资源治理为主，功能扩展排在后面。

主要问题集中在以下链路：

1. **生命周期：** 常规关标签、关 pane、重启 shell 都存在 `mem::forget`；整份终端状态或 PTY 会话在进程存活期间无法回收。进程退出时的特殊绕行方案，被扩散到了日常操作。
2. **资源上限：** PTY 通道、应用缓冲、图集和 shaping 缓存缺乏统一预算。滚回行数虽然固定为 10,000，但不能代表整个进程内存有界。
3. **渲染正确性：** pane 各有图集，绘制却将所有 pane 实例合并后只绑定焦点 pane 图集。现有单图集上传回归测试没有证明分屏正确。
4. **IPC 和窗口管理：** 通知先于入队、没有真正的请求响应、缺少完整读写超时；最后一个标签与窗口关闭的路径不统一。
5. **协议与 Unicode：** 有已实测的分块丢事件、中文路径乱码、事件顺序和标记位置问题。真实管道可以任意拆包、并包，不能依赖 shell 一次 write 就对应一次 feed。
6. **性能证据不足：** 已有渲染微基准不测真实 GPU 与 UI 交互，且“增量轮次”主动清空行缓存。没有长期内存、句柄、线程、延迟、功耗门禁。

### 1.1 本次已执行验证

| 检查 | 本次结果 | 解释 |
|---|---|---|
| `cargo fmt --all -- --check` | 通过 | 格式基线正常 |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | 通过 | 静态 lint 基线正常 |
| `cargo test --workspace --locked` | 190 通过，4 忽略，0 失败 | 包含实际 Windows PTY、GPU、崩溃档案和独立 UIA provider 测试；不等于完整 GUI 验收 |
| release 渲染微基准 | `cells_per_sec=17243081 frame_us=67` | 仅本机单次结果；不是终端端到端 FPS 或输入延迟 |
| release sidecar 预算测试 | 4 次开销为 7.8%、10.4%、4.9%、15.8% | 3 次超过现有 5% 断言，1 次通过；存在波动，不能报告稳定达标，也不能据此锁定唯一瓶颈 |
| 小型协议探针 | 复现下表 F06～F09 | 使用当前编译的 core，未启动完整终端 |

探针临时源文件位于忽略目录 `target/mica-audit-2026-11-01/probe.rs`，不随 Git 交接。实施者应将以下行为固化为仓库回归测试，不依赖这个临时文件。

**尚未测量：** 真实应用 Private Bytes / Working Set、GPU 显存峰值、反复开关标签曲线、UI 输入到呈现 p95/p99、整机能耗、8 小时运行、MSI 安装升级卸载、生产 UIA 深度查询。下文数值是建议验收预算，不能冒充现有结果。

## 2. 证据与优先级

证据分级：**实测**＝本次探针或命令复现；**源码确定**＝代码路径直接可见，用户界面后果仍须端到端验收；**风险**＝需特定负载或系统行为验证。P0 优先处理正确性、误终止进程、持续泄漏和不可控 IPC；P1 处理交互与资源治理；P2 处理体验和发布完善。

| ID | 优先级 / 证据 | 位置（审查时行号） | 发现及影响 |
|---|---|---|---|
| F01 | P0 / 源码确定 | `crates/mica-app/src/app/orphan.rs:47-100` | `execute=false` 只约束宿主清杀，前面的子进程 `TerminateProcess` 没有该条件。“观察模式”仍有破坏性；候选仅按全机进程名和父 PID 判断，也未证明属于 Mica。 |
| F02 | P0 / 源码确定 | `app.rs:1471`、`:1716`、`:2000`、`:3955`；`app/quickterm.rs:227` | 关标签/分屏永久遗忘整个状态，重启遗忘旧 PTY；主窗退出而副窗仍在时也遗忘主窗资源。内存、COM、PTY 句柄可随操作累积；增长幅度待量测。 |
| F03 | P0 / 源码确定 | `app.rs:1792`、`:2056-2180`、`:2539-2562` | 每个 pane 调用 `DwriteRouter::new` 生成独立图集；所有 pane 实例却只使用焦点图集单次绘制。不同字符集、字号或插入顺序会使 UV 所属资源不匹配。 |
| F04 | P0 / 源码确定 | `app/ipc.rs:71-102`、`:141-170` | 服务端先 Post 再入队，UI 可先醒来读空；服务端未写 ACK，客户端却阻塞读并忽略结果。成功返回不证明命令执行，错误目标也无反馈；单客户端可占住服务端。 |
| F05 | P0 / 源码确定 | `app.rs:832`、`:3791`、`:3882`、`:3955` | caption 关闭直接投递 `WM_DESTROY`；最后标签关闭只 `PostQuitMessage`，没有统一实际销毁和计数递减。存在窗口残留、计数不归零、进程持续等待的路径。 |
| F06 | P1 / 实测 | `core/src/protocol/sidecar.rs:144-221` | OSC 7 完整输入能取 cwd；测试序列按偏移 4～25 拆成两段均丢事件。`open_seq` 的延续检测只处理 OSC 133，OSC 7 payload 分块时没有同样续命。 |
| F07 | P1 / 实测 | `core/src/surface.rs:24-53` | URL 解码逐字节转 `char`。百分号 UTF-8 和直接中文路径均得到乱码，例：`/C:/ä¸…`；Windows 盘符前的 `/` 也需规范化。 |
| F08 | P1 / 实测 | `core/src/surface.rs:258-275` | 同块 `2026l` 后 `2026h` 最终应处于开启，实际为关闭；只累计 begin/end 次数，先处理 begin 再处理 end，丢失流顺序。 |
| F09 | P1 / 实测 | `core/src/surface.rs:277-291` | `OSC133 A + 两行文本 + OSC133 B` 一次 feed 得行 0/0，逐字节 feed 得行 0/2；mark 行号记录在块起点。history 满后仅按 history 深度作 epoch 也需验证。 |
| F10 | P1 / 源码确定 | `core/src/pty.rs:120`；`app.rs:1168`、`:3521` | 无界通道叠加无界 `Vec`，每块 Post，UI 一次 feed 全量积压，可能同时造成内存峰值、消息风暴和交互长任务。 |
| F11 | P1 / 源码确定 | `render/src/font/atlas.rs:111-177`；`font/dwrite.rs:575-651` | 图集高度持续加页，未按 GPU dimension / CPU 总预算限制；`run_cache` 按完整文本段存储并持续增长。达到设备极限前后缺少可恢复策略。 |
| F12 | P1 / 源码确定 | `render/src/pipeline.rs:289-350` | 每次 `set_atlas` 都新建整张纹理、全量上传、重建绑定。新字形与切标签造成额外 CPU/GPU 分配和传输；优化时必须保留图集身份判断。 |
| F13 | P1 / 源码确定 | `app.rs:3606-3616`；`app/quickterm.rs:367` | 主窗每次输出重置绘制定时器，持续输出可能不断推迟；QT 逐消息立即绘制，节奏不一致。需要有最大等待上限的统一调度器。 |
| F14 | P1 / 源码确定 | `core/src/profile.rs:75`、`:110-122`；`core/src/pty.rs:74` | WSL 枚举注释写 3s 超时，实际 `Command::output` 没超时；名称含 linux 会被误当表头；命令串按空白拆分，带空格路径/发行版名会坏。 |
| F15 | P1 / 源码确定 | `app.rs:4574-4602`；`render/src/frame.rs:478` | 搜索把 UTF-8 字节偏移当列位置，宽字符/组合字符映射不可靠；每个输入字符同步扫全部回看并保存所有命中。搜索状态为窗口 TLS，需核验切 pane/tab 后的归属。 |
| F16 | P1 / 风险明确 | `render/src/pipeline.rs:412-477`；`app.rs:1240` | 图片先完整解码后缩小；4096 限制不约束解码峰值。图片、字体重建、WSL 扫描、导出及 PTY write 等同步工作可能阻塞 UI。 |
| F17 | P1 / 覆盖缺口 | `app.rs:2569`；`app/tests/uia.rs:18` | 生产走系统 host provider，测试却走已撤线的自定义 provider，测试通过不证明生产文本可访问。已知 native crash 不能靠恢复分派来“补功能”。 |
| F18 | P1 / 源码确定 | `app.rs:475`、`:1427`；`render/src/pipeline.rs:21` | GPU lost 是跨窗口共享布尔值，首个窗口 swap 后别窗可能错过；QT 没有同等恢复链；固定独显优先也缺少笔记本功耗实证。 |
| F19 | P1 / 实测+方法缺陷 | `render/src/frame.rs:1683`；`core/src/surface.rs:1173` | 渲染基准只跑 3 轮，假 router，不含真实 GPU；每轮 `rows.clear()` 破坏增量语义。sidecar A/B 总是固定先后，热身、样本和噪声处理不足。 |
| F20 | P2 / 源码确定 | `Cargo.toml:8`；`README.md:101`；`.github/workflows/ci.yml` | crate 仍为 1.0.9，README 写 1.1，CLAUDE 写 1.3；CI 顶层未配置 tag push，job 的 tag 条件本身不能创建触发；纯文档跳过只见注释，没有对应条件。未发现独立 release workflow。 |
| F21 | P1 / 源码确定 | `app/clipboard.rs:15-77`；`core/src/surface.rs:146-179` | 剪贴板重试在 UI 线程 sleep；HGLOBAL 失败路径未全覆盖释放，读文本未使用分配长度限制；OSC52 只有单条写入长度限制，读权限与累计预算未建模。 |
| F22 | P1 / 风险 | `app.rs:2930`；`app/ipc.rs:24-40` | 输出流提供的 URI 直接交给 ShellExecute；管道固定全局名称、默认安全描述符，没有显式本用户/会话隔离与拒绝远程连接策略。应根据本地威胁模型补边界，不能直接宣称当前可被任意用户利用。 |

## 3. 实施纪律与保护边界

- [ ] 读 `CLAUDE.md` 和设计文档；用户明确偏好优先。以当前源码和新复现为准，旧审查“全部通过”不能替代本次验证。
- [ ] 遵守 core 无 GUI、render 不依赖 app、app 只做系统接线；不复制 Ghostty 实现，不引入 GPL 依赖。
- [ ] 默认 feature 分支从 `dev` 创建并回 `dev`，遵守仓库现有命名要求；未经明确授权不合并、不打 tag、不发布。
- [ ] 每个修复先构建能触发实际故障的回归，记录修前失败和修后通过；避免只测试 getter 或镜像实现。
- [ ] 验证 `Cargo.lock` 对应版本 API；不顺手升级 wgpu/windows/alacritty 大版本。先本地读锁定依赖源码，必要时查该版本官方文档。
- [ ] `CellInstance` 64 字节、像素 UV、背景先于字形、非 sRGB、宽字符、图集身份与 revision、查询必须回包、Ctrl 消费同步吞 WM_CHAR 等契约必须保留。
- [ ] 不因为 ConPTY 主路径过滤某协议就删掉 core 能力；按 Windows build 记录实际可达性。
- [ ] 不恢复未经实现的默认终端注册、不直接恢复已知崩溃的 UIA provider。
- [ ] winget 暂缓是仓库已记录的用户决定：本轮不投递。真实迁移活进程 tear-off、kitty graphics、完整连字引擎、per-pixel 透明先做独立设计和探针。
- [ ] 不用提高内存上限、强制 GC、清空用户滚回、隐藏错误或放宽性能门槛掩盖问题。
- [ ] 在 F01 修复前，不运行真实 orphan sweep 的 false 模式做测试；进程终止测试只能使用隔离创建的测试进程。

## 4. 文件职责与建议拆分

不进行无关全仓重构。以下新文件在相应任务实施时创建；先建立故障回归和接口，再从 `app.rs` 迁移相关逻辑。

| 文件 | 职责 / 修改范围 |
|---|---|
| `crates/mica-app/src/app/orphan.rs` | 候选计算与终止执行解耦，明确 Mica 所有权，dry-run 零副作用 |
| `crates/mica-app/src/app/lifecycle.rs`（新） | pane、会话、窗口关闭状态机和资源回收，避免生命周期跨散多个事件分支 |
| `crates/mica-app/src/app/scheduler.rs`（新） | dirty、pending wake、帧截止时间、最小化/失焦调度；时间由可控时钟输入 |
| `crates/mica-app/src/app/pty_bridge.rs`（新） | 有界输出桥、异步写队列、退出通知、取消和 generation |
| `crates/mica-app/src/app/window_registry.rs`（新） | 主副窗口登记、活跃窗口寻址、IPC 重定向、计数与退出 |
| `crates/mica-app/src/app/search.rs`（新） | 搜索状态归属、节流、取消、命中映射和高亮 |
| `crates/mica-app/src/app/diagnostics.rs`（新） | 可关闭的计时、队列、资源计数；不记录输入和剪贴板正文 |
| `crates/mica-app/src/app.rs` / `app/quickterm.rs` | 统一接入上述机制；两类窗口行为矩阵与明确差异 |
| `crates/mica-core/src/pty.rs` | 有界读取、明确 EOF/超时/错误和可取消生命周期；不引入 Win32 GUI API |
| `crates/mica-core/src/ipc.rs` / `app/ipc.rs` | 有界增量编解码、结构化响应、身份与超时、RAII 句柄 |
| `crates/mica-core/src/profile.rs` | 结构化 exe/args/cwd，后台扫描与真实超时，正确识别 WSL 名称 |
| `crates/mica-core/src/protocol/sidecar.rs` / `surface.rs` | 跨块不变、顺序事件、精确位置、正确 URL 字节解码、滚回预算 |
| `crates/mica-render/src/font/atlas.rs` / `font/dwrite.rs` | 图集身份、上限、page/generation、缓存预算、失效契约 |
| `crates/mica-render/src/pipeline.rs` / `frame.rs` | 多图集绘制批次、纹理复用与脏矩形上传、实例复用、文本格映射 |
| `crates/mica-core/tests/stream_invariance.rs`（新） | 协议任意切块、并块与文本 Unicode 回归 |
| `crates/mica-app/tests/ipc.rs` / `lifecycle.rs`（新） | 真 Windows IPC、窗口和受控子进程故障注入 |
| `scripts/measure-resources.ps1`（新） | 按明确 PID 采集 CPU、Private Bytes、Working Set、线程与句柄；默认只读 |
| `docs/benchmarks/scenarios.md` / `results/`（新） | 工作负载、硬件/OS/配置、原始结果、统计方法与预算 |
| `.github/workflows/ci.yml` / `release.yml`（后者新） | 正确性门禁、定期压力测试、显式 tag 发布触发和可追溯附件 |

## 5. 按依赖执行的任务

### 阶段 A：建立证据并修复 P0

### T01：资源与行为观测基线

**文件：** 新建 `diagnostics.rs`、`scripts/measure-resources.ps1`、`docs/benchmarks/scenarios.md`。

- [ ] 固定 release、字体、主题、背景、窗口网格、history 和 OS build；记录 Rust、GPU、驱动、显示器 DPI/刷新率。
- [ ] 用进程 PID 绑定采样，记录每秒 Private Bytes、Working Set、CPU 时间增量、线程数、句柄数；子 shell 单独统计，不能计入后又重复相加。
- [ ] 记录应用自有缓冲容量与长度、队列高水位、读/写吞吐、未处理唤醒、feed 耗时、build/repack/upload/submit 分段、图集和缓存字节数。
- [ ] 采样关闭时不走每格日志、不分配大字符串；开启后吞吐开销建议不超过 2%，需实测。
- [ ] 采 1/5/10 标签、4 分屏、持续输出、隐藏 QT、开关标签 200 轮的修前数据；实际 GUI 启动需在 T02 移除全局误杀行为之后进行。

**验收：** 先可用纯 core/独立测试进程完成采样器验证；T02 后补完整 GUI 基线。结果包含原始 CSV、命令和配置，缺项明确标未测。

### T02：让进程清理可证明安全（F01，P0）

**文件：** `app/orphan.rs`；纯逻辑测试置于该模块。

- [ ] 把进程快照→候选列表做成纯计算；执行器只接受已验证所有权的候选。
- [ ] 所有终止操作都经过同一 `execute` 决策；测试通过 mock 执行器证明 dry-run 调用终止次数为 0。
- [ ] 不能证明归属 Mica 时跳过并记录原因；不要清理全系统“父 PID 不存在”的 OpenConsole 及任意子进程。
- [ ] 优先跟踪自己创建的会话 PID、创建时间和取消/退出状态；按 PID+创建时间验证，不能只按名字认领。Job Object 是否适用须先验证 ConPTY 和嵌套 Job 行为，不能默认所有宿主可纳入。
- [ ] 测试父进程仍活、PID 复用、同名无关进程、枚举失败、句柄打开失败、dry-run、真实隔离测试进程。

**验收：** `orphan_dry_run_never_terminates`、`orphan_unowned_process_is_ignored` 等回归通过；不触碰用户其他终端和真实任务。

### T03：关闭/重启后回收资源（F02，P0）

**文件：** `app/lifecycle.rs`、`app/pty_bridge.rs`、`core/pty.rs`、`app.rs`、`app/quickterm.rs`。

- [ ] 画清对象所有权：shell、PTY master/writer、reader、forwarder、buffer、Surface、字体 COM、CPU atlas、GPU 资源各由谁关闭。
- [ ] 状态采用 Running→Closing→Closed；关闭先拒绝新输入并撤掉唤醒目标，发取消，再终止所属子进程，最后回收句柄和缓存。
- [ ] UI 线程不执行无限等待和阻塞 Drop；将可能阻塞的 PTY 关闭放入数量受控的清理机制，记录超时和 backlog。
- [ ] 不把“每次建一个线程然后永久卡住”当替代方案；清理 backlog 必须有上限，超出时明确故障处理，不继续吞资源。
- [ ] 对历史坏驱动重现做最小探针：确认究竟是哪类对象 Drop 卡住。日常关闭必须释放 Surface、row cache、字体和 CPU atlas，不能一并 forget。
- [ ] 重启使用 session generation；旧 reader/forwarder 的尾字节不能进入新 shell，旧句柄可收回。
- [ ] 最后进程退出若仍需驱动兼容绕行，限定为该阶段的最小 GPU 对象，写明退出和回收证据；它不能覆盖标签/分屏的日常关闭。
- [ ] 加关闭 200 轮、重启 100 轮、QT 退出/重开、输出中关闭、子进程拒退、读线程不醒和并发多窗测试。

**验收：** 暖机后每轮存活会话/线程/句柄回到基线附近，无按轮数线性增加；Private Bytes 后 100 轮拟合增长建议 ≤32 KiB/轮且总增量 ≤16 MiB，超过即归因。不能用手动清空用户 history 达标。

### T04：统一窗口关闭与进程存活（F05，P0）

**文件：** `app/window_registry.rs`、`app/lifecycle.rs`、`app.rs`、`app/quickterm.rs`；`tests/lifecycle.rs`。

- [ ] caption、Alt+F4、关闭最后 pane/tab、IPC 退出、系统关闭都进入统一关闭入口，实际销毁 HWND，由真实生命周期消息更新登记。
- [ ] 窗口计数使用登记 token / guard 保证创建失败回滚、销毁只减一次；禁止以伪造 `WM_DESTROY` 代替关闭。
- [ ] 主窗先关而副窗还在时，IPC/配置监听等进程级服务应继续工作，能定位到仍存活窗口。
- [ ] 关闭最后普通窗口时明确 QT 策略：默认跟随进程结束；如果以后增加托盘驻留，作为显式配置单独设计。
- [ ] 用集成测试子进程加 watchdog，覆盖新窗失败、主先关、副先关、最后标签先关、快速连关、QT 可见/隐藏。

**验收：** 窗口和注册计数一致；最后关闭后正常机器建议 2 秒内进程退出，故障注入有明确超时出口；无残留 shell、IPC 占用和幽灵窗口。

### T05：修正分屏图集归属（F03，P0）

**文件：** `app.rs:draw_frame`、`render/pipeline.rs`、`render/font/atlas.rs`；渲染测试。

- [ ] 先写回归：A/B 图集故意以不同顺序插入不同色块或字形、revision 相同；左右 pane 使用各自 UV，验证像素不是只验证调用次数。
- [ ] 第一阶段采用每 pane 对应纹理/bind group 的绘制批次；一次获取 surface、一次 clear、所有 pane 与 UI 在同一帧依次绘制、最后一次 present。
- [ ] 每个 batch 显式携带 atlas identity、generation、revision 和 scissor；UI/strip 使用声明的同源 atlas。
- [ ] 不可在循环中多次清屏或 present；不可仅传 revision 数值；不可在不同 atlas 间复用没有身份的 UV。
- [ ] 若选择共享图集方案，先定义包含 font face、字号、DPI、style、glyph id 的 key，并验证不同字体和 UI 字号不会碰撞；不要顺手做全局共享大改。
- [ ] 覆盖 2/4 pane、CJK/emoji/ASCII、不同插入顺序、切焦点、热重载、DPI、设备丢失与切标签。

**验收：** 离屏像素测试 + 真机分屏截图均正确，保留宽字、背景先行和非 sRGB 契约；纹理归属不会随焦点错误变化。

### T06：重建 IPC 正确性与有界协议（F04/F22，P0）

**文件：** `core/ipc.rs`、`app/ipc.rs`、`app/window_registry.rs`、`mica-app/src/lib.rs`；`tests/ipc.rs`。

- [ ] 顺序改为先入队再通知；唤醒失败可重试或由统一调度补发，不能永久滞留。
- [ ] 定义有版本和 request_id 的响应：queued/applied/error 的含义清楚，非法 op/profile/tab/window/pane 有错误码。`send-text` 成功只承诺字节写入 PTY，不承诺 shell 命令执行完毕。
- [ ] 服务端实际写响应，客户端校验响应与请求对应；全生命周期连接/读/写有截止时间，半帧慢客户端不能独占服务。
- [ ] 初始帧上限建议 1 MiB；长度越界、非法 JSON、未知操作必须立即拒绝。编解码区分 NeedMore / Complete / Invalid，不能把坏帧当未到齐一直累积。
- [ ] 处理客户端抢先连接时的 `ERROR_PIPE_CONNECTED`；所有返回路径 RAII 关闭句柄，写失败不能泄漏。
- [ ] 管道名称按用户与会话隔离；显式本用户 ACL 和本机连接策略，验证普通/提升进程的期望互通边界。
- [ ] 主窗关闭后重定向到有效窗口；逐步引入稳定 window/tab/pane id，保留旧 `--tab=N` 的兼容语义并说明适用窗口。
- [ ] 用唯一测试 pipe 做部分帧、超长帧、坏 JSON、100 并发连接、客户端断连、超时、UI 先醒、服务端退出、错目标测试。

**验收：** 任一无效请求不阻塞随后正常客户端，CLI 非零退出表达真实错误；句柄数不随失败请求增长；正常请求没有丢失或重复应用。

### 阶段 B：协议、输入与主线程响应

### T07：字节流分块不变与正确路径（F06～F09）

**文件：** `core/protocol/sidecar.rs`、`core/surface.rs`；新 `core/tests/stream_invariance.rs`。

- [ ] OSC 7/133、CSI 2026、BEL/ST/ESC 边界采用同一可解释状态机，保证任意切块和并块后的最终结果一致。
- [ ] 输出有顺序和字节位置的事件；先解析至事件位置，再更新对应终端状态。不要按块起始 cursor 给所有 marks 定位。
- [ ] 保留事件时间顺序，测试 l→h、h→l、h→l→h 和重复设置，最终状态由最后事件决定。
- [ ] URL 百分号解码先产字节，再按 UTF-8 解码，Windows 盘符/UNC/authority 规则明确；不自动联网探测远程 authority，不因缺失路径卡住 UI。
- [ ] 超长未终止控制序列设置长度/工作量边界和恢复策略；精确事件处理不能退回逐字节调用整个主解析器而没有吞吐评估。
- [ ] marks 用滚动行序列或等价稳定锚点；测试 history 填满并淘汰、resize reflow、alternate screen、清屏、长行自动折返。
- [ ] 属性测试覆盖所有单切点、单字节 feed、固定种子随机 chunk；比较 grid、title、cwd、sync、marks、回复等可观察状态。

**必须覆盖的最小例子：**

```rust
use mica_core::surface::{ScreenSize, Surface};

#[test]
fn sync_last_event_wins() {
    let mut s = Surface::new(ScreenSize::new(80, 24));
    s.feed(b"\x1b[?2026l\x1b[?2026h");
    assert!(s.sync_output_active());
}

#[test]
fn osc7_chinese_url_decodes_as_utf8() {
    let mut s = Surface::new(ScreenSize::new(80, 24));
    s.feed(b"\x1b]7;file:///C:/%E4%B8%AD%E6%96%87\x07");
    // Windows 路径统一约定为 C:/...；若改为反斜杠，应同步接口规范。
    assert_eq!(s.cwd(), Some("C:/中文"));
}

#[test]
fn marks_do_not_depend_on_feed_chunks() {
    let input = b"\x1b]133;A\x07line1\r\nline2\r\n\x1b]133;B\x07";
    let mut whole = Surface::new(ScreenSize::new(80, 24));
    whole.feed(input);
    let mut bytewise = Surface::new(ScreenSize::new(80, 24));
    for byte in input { bytewise.feed(&[*byte]); }
    let positions = |s: &Surface| s.shell_marks().iter().map(|m| m.line).collect::<Vec<_>>();
    assert_eq!(positions(&whole), vec![0, 2]);
    assert_eq!(positions(&whole), positions(&bytewise));
}
```

**验收：** 上述测试修前可重现失败、修后通过；跨块矩阵通过；release 解析吞吐有前后对照，不因精确性修复造成未解释的大幅退化。

### T08：有界 PTY 桥和写入队列（F10/F16）

**文件：** `core/pty.rs`、`app/pty_bridge.rs`、`app.rs`、`app/quickterm.rs`。

- [ ] 合并两级输出积压的预算，按字节计量；初始建议每 pane 待解析 2 MiB、进程总待解析 32 MiB，达到上限采用可取消背压，不静默丢正常终端字节。
- [ ] 队列满时仍能取消和关闭；不要持共享锁执行阻塞 send、PTY write、字体解析或 GUI API。
- [ ] 每 pane 最多一个 pending wake；消费后采用清标记→复查→补通知的防丢唤醒契约，Post 失败必须能恢复。
- [ ] 每 UI 调度批次限制字节数或耗时，初始建议 256 KiB 或 2ms，随后让出消息循环再续；保障 DSR 等回复及时。
- [ ] 单 writer 保序写队列处理用户输入、粘贴、CLI 注入与终端回复；容量有界，错误、关闭、部分写可观察。不能为大粘贴在 UI 上循环 write_all。
- [ ] EOF、超时、取消、读取错误分开表示；读/转发线程可追踪并回收，generation 避免重启串流。
- [ ] 压测前台/后台各 pane 持续输出、8～16 pane、10 MiB 粘贴、shell 不读、关闭中的满队列、超密查询应答。

**验收：** 缓冲不超预算，终端字节序与参考回放一致，Ctrl+C/切标签/关窗不会被满队列饿死，内存峰值与吞吐曲线可解释。

### T09：统一有最大延迟的绘制调度（F13）

**文件：** 新 `app/scheduler.rs`，修改 `app.rs`、`app/quickterm.rs`。

- [ ] 用 dirty + absolute next_deadline 调度；首次 dirty 安排截止时间，后续数据不无限重置截止时间。所有 pane 在窗口级合并。
- [ ] 初始按显示刷新率给出 8～16.7ms 帧预算；低延迟输入唤醒不等同于无限重绘，具体阈值由 T01 数据决定。
- [ ] Win32 timer 只作唤醒，不把请求 5ms 当实得 5ms；用单调时间判断是否到期。
- [ ] 最小化/occluded/隐藏 QT 继续解析并服务查询，跳过光栅化与 GPU 提交；恢复立即画最新完整状态。
- [ ] 空闲无动画、无闪烁需要时停止绘制；光标只使所属行/区域变脏。动画关闭和减少动态效果都有效。
- [ ] 2026 最大持帧约束归属正确 pane/window，后台 pane 的状态不能误清前台 held 标记。
- [ ] 用虚拟时钟测持续输出不饥饿、无输出不忙转、输入优先、定时器丢拍补偿；QT 接同一调度契约。

**验收：** 连续输出 60s 仍定期呈现；静止窗口（关动画/光标闪烁）10s 无重复绘制；调度层等待不超过两帧预算，端到端另实测。

### T10：Unicode 搜索、选择和命中映射（F15）

**文件：** 新 `app/search.rs`，修改 `render/frame.rs:row_string`、`core/surface.rs`、`app.rs`。

- [ ] 明确 UTF-8 byte、Unicode scalar、grapheme 与 terminal cell 的转换；宽字符 spacer 不重复输出，组合附加字符不丢失。
- [ ] row 文本提供字节范围→cell 范围映射；大小写折叠后的长度变化也保留原文映射，不能拿 lowercased 字节偏移画格子。
- [ ] 搜索 state 按 pane id/generation 归属；切换、关闭、重启、history 淘汰、resize 后重新验证命中。
- [ ] 70～100ms 输入去抖或按 UI 时间片增量扫描，旧 query 可取消；先返回可见结果，命中总量超限不无限堆 Vec。
- [ ] 禁止把持有 GUI grid 借用跨线程发送；选择增量 UI 扫描，或在预算内生成只读快照交给工作线程。
- [ ] 加 `中文ABC` 搜 ABC/中文、emoji、ZWJ、组合音标、宽字、德语/土耳其大小写、软折行、空查询、10k 行高命中测试。
- [ ] 导出和复制使用同一正确文本提取语义，明确换行和末尾空格规则。

**验收：** 命中高亮与复制内容一致，切 pane 不串结果；新查询反馈目标 ≤100ms，扫描期间输入无连续 >16ms 的同步长任务，异常长行受预算约束。

### T11：Profile、WSL 和 CLI 启动参数（F14）

**文件：** `core/profile.rs`、`core/pty.rs`、`mica-app/src/lib.rs`、`app.rs`。

- [ ] Profile 表示 exe、args、cwd、env，不再以空白拆串作为底层接口；兼容旧串配置时明确 Windows 引号规则并回归。
- [ ] WSL 枚举移出 UI，真正 3s deadline，到时 kill + wait 并释放管道；不能只给等待线程超时然后遗留子进程。
- [ ] 优先评估 `wsl --list --quiet` 的锁定系统支持范围，解析不能按包含 linux/distribut 就删名称；失败静默降级但可诊断。
- [ ] 覆盖 `kali-linux`、`AlmaLinux`、自定义空格名、中文、BOM/无 BOM、UTF-16LE/UTF-8、空结果、进程卡死。
- [ ] `new-tab --font-size` 错值、无值、NaN、超范围明确返回错误；首次启动 fallback 保留字号/profile/cwd，不能只在 IPC 路径生效。
- [ ] `new-window [profile]` 真正应用参数；补 CLI help/version、`--` 文本分界和稳定目标标识说明。

**验收：** 带空格 exe/args 可正确启动；WSL 故障不冻结主窗；首次启动与已有实例分发行为一致。

### T12：输入、剪贴板和输出打开边界（F21/F22）

**文件：** `app/clipboard.rs`、`core/surface.rs`、`core/input.rs`、`core/keymap.rs`、`app.rs`、`app/quickterm.rs`。

- [ ] HGLOBAL 使用明确所有权 guard；GlobalLock、EmptyClipboard、SetClipboardData 失败时释放，成功移交后不重复释放。读文本按实际分配长度查终止符。
- [ ] 剪贴板被占用用异步重试/消息重调度，不在 UI sleep；大文本按输入队列预算提交。
- [ ] 为 OSC52 建独立读/写配置，默认禁读本地剪贴板，写操作受焦点或用户配置策略约束；每条和累计字节都限流，不假设 ConPTY 永远屏蔽。
- [ ] URI scheme 分类：默认允许 http/https（mailto 可配置）；文件和其他自定义协议需要明确用户动作或策略，不由任意输出直接调系统处理器。
- [ ] 粘贴矩阵覆盖 CR/LF/CRLF、bracketed paste、正文自带结束序列、NUL/ESC、大文本和多行确认策略；策略可配置且不破坏 coding agent 常用输入。
- [ ] 主窗与 QT 覆盖 surrogate pair、IME、AltGr、Ctrl+Alt、按键重复/释放、kitty flags、修饰键映射及已消费 Ctrl 的 WM_CHAR 抑制。

**验收：** 权限受限的远程终端内容不能静默读取本地剪贴板；粘贴不重复、不丢字符；失败路径无内存/句柄泄漏，输入延迟有测量。

### 阶段 C：CPU、内存、GPU 与异常恢复

### T13：有预算的字形图集与 shaping 缓存（F11）

**文件：** `render/font/atlas.rs`、`render/font/dwrite.rs`、`app/diagnostics.rs`、配置。

- [ ] 查询实际 device limits；图集长边不能超 `max_texture_dimension_2d`，所有乘法、页数和 Vec 大小 checked 计算。
- [ ] 采用固定大小页和明确 CPU/GPU 字节预算；初始候选：窗口 atlas CPU 总计 32 MiB、GPU 32 MiB，进程各 128 MiB。先在 10 标签/4 分屏实测后定档，不能逐 pane 各给同额导致总预算失控。
- [ ] 保持既有条目不移动的优点；淘汰按 page/generation 失效，仍被实例引用的页必须 pin 或在复用前强制正确重建。
- [ ] `run_cache` 按条目和估算字节双限额 LRU；字符 cache、失败 miss cache、字体集合按作用域统计。只在支持 shaping 的字体路径评估 run_cache 成本。
- [ ] 淘汰不能只删 HashMap key 而保留 atlas 位图；两者内存治理要闭环。
- [ ] 超宽字形、超高字形、OOM/额度不足改为受控 fallback + 诊断，不让未可信输出触发断言终止。
- [ ] 用大量唯一 Unicode、动态变化长文本、emoji、字体热切换、字号缩放、窗口多开验证预算与重建。

**验收：** 达上限后稳定运行，画面正确且资源计数平台化；无旧 UV 采到新字形、无限增高、溢出或非预期 panic。

### T14：纹理复用和增量上传（F12）

**依赖：** T05、T13 的 identity/generation 契约。

**文件：** `render/pipeline.rs`、`font/atlas.rs`、`app.rs`。

- [ ] 同 atlas/same size 的插入复用 texture、sampler 和 bind group，只上传脏矩形；新建/换页/设备重建才分配。
- [ ] GPU 端按 atlas id + generation 管理资源；切标签可命中缓存，但缓存总字节受预算管理。
- [ ] 图集 CPU/GPU dirty 同步在成功上传后提交；失败或设备重建不得误标已同步。
- [ ] 统计每帧纹理新建次数、上传字节、绑定重建次数和缓存命中率，验证不同 atlas revision 相同的旧回归不复发。
- [ ] 固定序列重放对比 CPU、Private Bytes、GPU memory 和 p95 upload 成本；只有收益明确才合入。

**验收：** 已缓存字体的光标闪烁/空闲绘制不新建 atlas texture；单字形新增不触发整张全量上传；切标签与多 pane 像素回归仍通过。

### T15：实例与临时分配优化

**文件：** `render/frame.rs`、`render/pipeline.rs`、`app.rs:draw_frame`。

- [ ] 用 allocation profiler 或内部计数确定最热分配，再复用 repack/all_instances/strip/search_overlay 缓冲。
- [ ] dirty 行复用 CPU 行缓存；分别测全屏、单行、仅光标、仅标题栏，明确哪些路径仍全量 flatten 和上传。
- [ ] GPU buffer 采用复用/几何增长；是否实现分段更新或环形 staging 由 upload 占比决定，不预先引入复杂同步。
- [ ] resize 峰值后的大 buffer 可按冷却策略缩容，但避免来回调整反复分配；不每帧 shrink_to_fit。
- [ ] 每个 pane 使用 scissor，防止宽字、tooltip 或光标越界污染旁边 pane。

**验收：** 暖缓存单行更新不重光栅全屏，分配次数和上传字节下降有量化结果，CJK/选择/下划线/光标/overlay 像素不变。

### T16：滚回、图片、导出和磁盘预算（F16）

**文件：** `core/config/settings.rs`、`core/surface.rs`、`render/pipeline.rs`、`app.rs`、后台工作队列。

- [ ] 让 scrollback limit 可配置，明确范围与默认 10,000 行；测窄屏、4K 宽屏和多 pane 实际 bytes/line，建立进程总预算估算。主动减小 history 必须是用户配置行为。
- [ ] 图片在完整分配前检查尺寸、像素数和 decoder allocation limit；限制文件大小不能替代限制解码后大小。
- [ ] 解码/缩放在工作队列进行，返回有界 RGBA 后由渲染线程上传；快速切换用 generation 丢弃过时任务，保留旧背景直到新图成功。
- [ ] 固定 RGBA CPU 副本、缩放中间图、对齐上传暂存、GPU 纹理的重叠生命周期，量测峰值。
- [ ] 导出按行/流式写入，文件名用毫秒加随机后缀避免同秒覆盖；保存失败明确反馈，不持 UI 状态锁写盘。
- [ ] 对崩溃档案/临时导出提出本地保留数量和容量策略；清理只限 Mica 自有目录，先预览候选，不按全局临时目录泛扫。

**验收：** 超大/损坏图片可恢复报错；切配置不长时间卡住输入；导出内容保持 Unicode 和折行语义，峰值与磁盘增长受控。

### T17：多窗口 GPU 故障、DPI、功耗和睡眠恢复（F18）

**文件：** `app.rs:init_window_gpu/rebuild_gpu_after_loss/reload_dpi`、`app/quickterm.rs`、`render/pipeline.rs`。

- [ ] device lost 按 device/window id 或故障 generation 广播，不能由全局 bool 被一个窗口消费掉；QT 接入同等恢复。
- [ ] 重建显式恢复背景图、透明度相关配置、字体图集、surface、viewport 和 dirty 状态；失败有限重试+退避，可见错误，避免重建风暴。
- [ ] 区分 Surface Lost/Outdated/Timeout/Occluded/设备错误；minimize 时不 configure 0×0，遮挡不忙转，validation 不静默吞掉。
- [ ] 新窗口、100/125/150/200% 混合 DPI、跨屏、RDP 连接/断开、锁屏、睡眠唤醒和显示器拔插进入验证矩阵。
- [ ] 适配器优先策略保留历史虚拟驱动故障回归，增加用户可选 auto/low-power/high-performance；先测独显唤醒、空闲功耗、吞吐再设默认。
- [ ] 是否跨窗口共享 device 仅作为测量后的专项优化，先评估 COM !Send、surface 与 device lost 扇出，不强行改变全部线程模型。

**验收：** 可控 fault injection 后所有存活窗口恢复一致，无重复建资源、无 shell 重启或输出丢失；有真机屏幕与功耗证据。

### T18：可追溯诊断与崩溃恢复

**文件：** `app/crash.rs`、`app/diagnostics.rs`、发布配置和用户文档。

- [ ] 日志有级别、限速和本地滚动；禁止记录键入命令、IPC 正文、剪贴板与终端全文作为默认诊断。
- [ ] 错误至少区分 PTY 创建/读/写失败、渲染不可用、配置错误、IPC 拒绝和后台任务失败；保留可用旧状态。
- [ ] dump 与 exe/pdb/build id 可对应，故障目录容量有上限；native 异常处理内减少分配和潜在递归，复杂故障收集评估独立 helper。
- [ ] 不让一个后台线程 panic 静默导致终端永久失效；观察通道异常并向所属 pane 显示可恢复错误。
- [ ] 自动化崩溃测试用独立目录，避免并行测试互读档案；测试和采样脚本都带进程级 watchdog。

**验收：** 无法恢复的错误有明确原因和定位信息；恢复不会重放输入或泄漏其他 pane 内容；dump 可用匹配符号实际解析一次。

### 阶段 D：验证体系、体验与交付

### T19：让性能基准测到真实工作（F19）

**文件：** `render/frame.rs:bench`、`core/tests/corpus_bench.rs`、`core/surface.rs:sidecar_throughput_budget`、`docs/benchmarks/`。

- [ ] 拆开 parser MB/s、sidecar 开销、冷/暖字体、全屏 build、增量行 build、repack、真实 GPU upload/present 与端到端输入延迟。
- [ ] 增量基准保留上一帧 rows，不 clear；验证实际重建行数和计量分母，“instances/s”与“cells/s”不能混称。
- [ ] 热身后用足够样本（建议 ≥30），报告 median/p95/p99、最差值、原始分布；吞吐可另保留 best 值诊断，但不能只用 best 代表交互稳定性。
- [ ] sidecar A/B 交替或随机顺序、相同初始化与输入；测试普通输出、候选密集、恶意半序列和不同 chunk size，解释本次 4.9%～15.8% 波动。
- [ ] corpus 使用可提交、脱敏、固定 hash 的语料，明确是标准 asciinema 还是自定义 base64 格式；当前录制并不是通用 asciinema output 格式，不能误标可互操作。
- [ ] record_corpus 的 pump 处理启动 DSR 和真正命令完成标记，不用“静默 200ms”代替完成；不得提交环境变量、用户名和私密命令输出。
- [ ] 性能 CI 保留源码正确性检查，可靠自托管/固定硬件承担严阈值；共享 runner 负责趋势与明显退化，上传失败日志和原始结果。

**验收：** benchmark 名称、单位和实际测量一致；新调度/图集优化在真实负载有收益；已有 5% sidecar 目标由受控实验复核，不能直接改为 20% 让测试变绿。

### T20：生产路径 GUI 和长期压力回归（含 F17）

**文件：** `mica-app/tests/`、测试 fixture、资源采样脚本、CI。

- [ ] 为真实主窗和 QT 提供隔离测试启动方式：测试配置目录、唯一 IPC、测试输出目录、禁全局清理、指定 shell、确定性工作负载。
- [ ] smoke：输入/复制粘贴、中文 IME、emoji、选择、滚轮、鼠标报告、搜索、缩放、标签/分屏、标题、配置保存、CLI 注入、shell 退出与重启。
- [ ] UIA 测试经过生产 WM_GETOBJECT 路由；host provider 能力如实断言，自定义 provider 留隔离实验。未解决 crash 前不启用生产 provider。
- [ ] 压力：200 轮标签开关、100 轮 shell 重启、50 轮主副窗、100 轮主题/字号/背景切换、持续混合输出 30 分钟，再执行 8 小时 soak。
- [ ] 可控异常：PTY EOF/断管、shell 不读、半帧 IPC、线程 panic、剪贴板占用、文件权限不足、损坏配置/图片、GPU lost、睡眠/恢复。
- [ ] 将能自动化的像素/状态/进程/句柄断言自动化；输入法、屏幕阅读器和不同显卡仍保留真机人工记录。
- [ ] 测试清理只终止自己创建的 PID 树，不按 `mica`/`powershell` 名字全局 taskkill。

**验收：** 回归报告写明自动/人工/未测，任何 hang 都被 watchdog 捕获，不能把超时当跳过后继续宣布通过。

### T21：日常体验补齐（P2，前面稳定后）

**文件：** 配置、`core/keymap.rs`、`app.rs` 相关拆分模块、README。

- [ ] 标签右键菜单：新建/关闭/关闭其他/改名/颜色/复制标题；键盘同样可访问，失效动作禁用。
- [ ] 分屏拖动调整、最小 pane 尺寸与键盘等价操作；若已有某项先核验，避免重复实现。
- [ ] 搜索界面补“无结果/搜索中/结果截断”、Esc、上下命中和当前范围；性能限制对用户可理解。
- [ ] 设置入口至少支持打开配置、校验诊断、显示当前文件路径和恢复默认；profile 选择明确显示真实 shell。
- [ ] shell integration 给 PowerShell/cmd/WSL 的安装说明与可选脚本；不擅自修改用户 `$PROFILE`，明确 OSC7/133 对应 OS build 的可用性。
- [ ] 重开标签说明仅恢复 shell/cwd/标题，不承诺活进程和完整 pane 布局；会话布局保存只作为显式功能，不能持久化密码/终端全文。
- [ ] Quick Terminal 是否支持搜索、选择、鼠标、主题热更新和 IME逐项列能力表，明确已实现与计划补齐。
- [ ] 高对比度、减少动画、键盘焦点、提示文字可读；完整 UIA 文本提供器单独设计，先证明稳定再扩展。

**验收：** 每项产品能力都有 UI/键盘路径与状态反馈，文档与实际一致，不把用户可见界面塞满内部实现细节。

### T22：CI、依赖与工程维护（F20）

**文件：** `.github/workflows/ci.yml`、依赖策略配置、贡献说明。

- [ ] 固定 Rust toolchain / MSRV 口径，所有确定性构建使用 `--locked`；主要 Actions 固定受控版本，关键供应链步骤评估 commit pin。
- [ ] CI 先分检测路径：纯文档也产出成功的 required check，不能直接跳过 required workflow 导致 pending；不只写注释。
- [ ] Windows 全 workspace、macOS core/render；完整异常/soak 夜间或手动运行，保留定期真实 Windows 图形/输入回归。
- [ ] 性能任务命令非零必须正确传播；不能仅有旧 BENCH 文本就算成功。每个 job 有 timeout、最小权限、取消策略和产物保留期。
- [ ] 用 `cargo audit` / `cargo deny` 或等效方式做依赖漏洞、许可证、重复依赖核验，明确豁免理由，不盲目升级全家桶。
- [ ] 将 app.rs 按 T03/T06/T09/T10 的职责自然拆分；修掉旧 UV/version/退出注释与 NUL 字符导致 rg 把源码当 binary 的问题。

**验收：** PR 正确性门禁真实运行；纯文档 required check 可结束；依赖许可证满足 MIT/Apache 项目约束；CI 文档与 YAML 一致。

### T23：版本、安装与发布闭环（P2）

**文件：** `Cargo.toml`、`Cargo.lock`、`build.rs`、`wix/main.wxs`、README、CLAUDE、`.github/workflows/release.yml`。

- [ ] 统一 crate、exe 版本资源、MSI、tag、README 和 changelog；本地 CLAUDE 的 1.3 声明不能替代对真实发布产物的核验。
- [ ] 显式 tag 触发，先校验测试/版本一致，再构建 exe/MSI、SHA256、许可清单、版本匹配的符号档案；release profile 是否带 debuginfo 单独配置并实际确认 PDB 可用。
- [ ] 检查当前 perUser MSI 却选 ProgramFiles 的路径/权限一致性；测试普通用户全新安装、升级、修复、卸载，PATH 重复和失效项清理。
- [ ] 验证旧配置兼容、首次启动、未装推荐字体、无 WSL、无管理员权限、非 ASCII 用户目录和安装路径含空格。
- [ ] `winres` 失败不能把缺 DPI manifest 的正式构建当成功；正式包必须检查 manifest/version/icon，而不是仅存在 MSI。
- [ ] 维持未签名政策的真实说明；如用户另行决定签名，再设计证书与密钥管理。不自动购买证书、不投递 winget、不擅自发布。

**验收：** 版本一致、安装升级卸载实测通过、可从干净 checkout 重建并对应发布附件与符号；最终发布需用户已有或新增授权。

## 6. 性能、内存和稳定性验收预算

以下为**初始建议**，T01 收集基线后在同一受控机器上定档。若与现有架构冲突，先提交测量和取舍，不能单方面放宽后宣布达标。所有 p95/p99 都要给样本量与测量区间。

| 指标 | 初始目标 | 测量方式 |
|---|---|---|
| 冷/暖启动 | 首窗首绘与 shell ready 分开统计；相比当前基线不退化 >10% | 冷/暖各至少 20 次，区分缓存状态；首绘不能代替 shell 可用 |
| 输入延迟 | 60Hz 普通负载 p95 ≤33ms、p99 ≤50ms；无 >100ms 反复卡顿 | 事件→提交为软件指标，键入→屏幕像素用呈现追踪/外部拍摄，不能混称 |
| UI 调度 | 连续输出按帧预算刷新，单次 feed 目标 ≤2ms，后台任务不占连续长时段 | monotonic 时间分段、真实 workload |
| 单帧 CPU | 120×40 暖缓存 p95 ≤4ms；4K 网格另给比例和结果 | build/repack/upload 分列，排除人为等待 |
| 空闲 CPU | 关动画/光标闪烁、无输出时平均 ≤0.5% 单逻辑核 | `CPU时间增量 / 墙钟时间`，1 核满载＝100%；同时列全机归一化指标 |
| 隐藏/最小化 | 无输出时不提交 GPU 帧；有输出时只解析和必要应答 | 统计 draw、queue submit、CPU 和 GPU busy |
| PTY 待处理 | 每 pane 2 MiB、进程 32 MiB 初值，取消可打断背压 | 应用字节计数与高水位，不仅看队列元素数 |
| 关闭/重启泄漏 | 200 轮关闭、100 轮重启后资源不线性增长 | 后 100 轮 Private Bytes 斜率 ≤32 KiB/轮，总增量 ≤16 MiB；句柄/线程稳定，解释 allocator 高水位 |
| 字体缓存 | CPU/GPU atlas 各窗口 32 MiB、进程 128 MiB 初值；shaping 另设上限 | CPU bitmap/caches、GPU texture 分开；共享资源只计一次 |
| 常驻内存 | 先测 1/5/10 标签和 4 分屏，再定绝对预算 | Private Bytes、Working Set、GPU dedicated/shared、shell 子进程分别报，不预设未经测量的“几十 MB”承诺 |
| 解析吞吐 | 固定语料相对基线下降不超过 5%；sidecar 额外成本受控复核 | 交替 A/B、至少 30 样本、同 hardware/config；协议正确性不让步 |
| IPC | 正常请求本机 p95 ≤100ms，不含 shell 执行；全流程超时初值 3s | 唯一测试 pipe、UI busy/连接竞争/坏客户端测试 |
| 图片峰值 | decoder 先检查；后台任务/结果/上传缓存有总预算 | 记录解码前、解码中、上传后峰值，分别算 CPU/GPU |
| 长期稳定 | 30min 压力 + 8h soak 无崩溃、死锁、资源线性增长 | 每秒采样，固定操作周期，记录后台标签输出和系统休眠 |
| 发布 | 干净用户安装/升级/卸载全部通过，配置保留策略明确 | Windows x64 主目标；ARM64 为独立兼容任务，不无证宣称支持 |

### 工作负载矩阵

1. 空闲 shell、普通打字、快速粘贴、shell 不读取输入。
2. ASCII 滚屏、SGR 密集日志、中文/emoji、宽字与组合字符、大量唯一 Unicode。
3. coding agent 风格局部重绘、全屏 TUI、鼠标报告、alternate screen、SSH/tmux/WSL。
4. 1/5/10 标签、2/4 分屏、多窗、后台持续输出、隐藏 QT 仍有输出。
5. 搜索 10k 行、极高命中、导出、主题/字体/背景热切换、4K/高 DPI。
6. 快速 resize、跨屏、RDP、显卡失效、休眠、损坏输入和低资源失败。

## 7. 依赖、批次与工作量

任务大小估计为含回归和验证的相对量级：S≈0.5～1工程日、M≈1～3日、L≈3～7日；是规划估计，不是 GLM 自动运行时间承诺。真实驱动、UIA 和多屏问题可能超出估计。

| 批次 | 任务 | 依赖 / 交付出口 | 量级 |
|---|---|---|---|
| A0 | T01 最小观测 + T02 | 先禁全局误杀，再采真实 GUI 基线 | M |
| A1 | T03 + T04 | 生命周期和关闭回归；后续压力测试的前提 | L |
| A2 | T05 | 分屏资源身份正确；后续 atlas 优化前提 | M～L |
| A3 | T06 | IPC 无丢请求/挂死，目标/错误可见 | M～L |
| B1 | T07 | 字节流与 Unicode 路径正确 | M～L |
| B2 | T08 + T09 | 有界桥和调度共同设计，逐任务提交 | L |
| B3 | T10 + T11 + T12 | 搜索、启动、输入各自验收 | 各 M |
| C1 | T13 + T14 + T15 | T05/T01 后实施；先上限，再减少传输与分配 | 各 M～L |
| C2 | T16 + T17 + T18 | 后台任务、GPU 恢复和诊断 | 各 M～L |
| D0 | T19 + T20 | 从 A0 起逐步积累，最后执行完整压力/soak | L |
| D1 | T21 + T22 + T23 | 核心治理通过后打磨与交付 | 各 M |

不要把这些批次当作必须一次合入的巨型 PR。最先交付 T02、T04、T05、T06 的独立修复，资源治理和测量持续跟进。若 T03 风险较大，先用测试锁住阻塞对象，再拆分可独立回收的 CPU 状态，不能凭历史注释直接删掉所有退出绕行。

## 8. 每个任务的完成模板

GLM 每轮交付使用以下信息，避免只报“已优化”：

```text
任务：Txx / 对应 Fxx
修前行为：具体触发步骤与失败证据
修改：文件、关键接口、为什么解决根因
正确性：测试命令、通过数、截图或状态断言
性能：固定负载下修前/修后 median、p95、p99、原始数据位置
资源：Private Bytes / CPU / GPU / handles / threads / queue 高水位
兼容性：ConPTY build、IME、DPI、旧配置、旧 CLI 的结果
限制：未测环境、已知边界、恢复/回退路径
Git：分支、commit、PR（若已授权创建），未授权不合并/发版
下一任务：依赖与验收出口
```

常规检查：

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo test --release -p mica-render bench_build_rows_throughput -- --ignored --nocapture
cargo test --release -p mica-core --lib sidecar_throughput_budget -- --ignored --nocapture
```

最后两条是现有基准，当前不足以证明完整性能，且 sidecar 本次存在超预算失败。T19 后更新脚本和解释，不能仅凭前三条通过宣布性能与内存达标。

## 9. 最终完成定义

- [ ] 所有 P0 有可复现的修前证据与修后回归，误终止其他进程风险被消除。
- [ ] 标签、pane、shell、窗口、QT 日常关闭能回收资源；进程级服务在多窗口关闭顺序下正常。
- [ ] 终端字节流任意拆/并块不改变可观察状态；Unicode 路径、搜索、复制和导出正确。
- [ ] PTY、IPC、atlas、shaping、图片和后台任务存在可观测的字节/任务预算和取消策略。
- [ ] 分屏每个绘制实例绑定正确图集；资源失效、切换、DPI、设备重建不会复现错字。
- [ ] 普通、压力、异常、空闲和隐藏负载均有性能/内存数据，8h soak 有记录。
- [ ] 所有功能声明对应生产路径验证；未完成的 UIA、ConPTY 边界和进程迁移如实保留。
- [ ] CI、版本、安装包、README、CLAUDE 与实际实现一致，发布操作遵守授权。

本计划完成后的判断依据应当是可重放的行为、受控资源曲线和可追溯产物，而不是功能清单长度或旧审查的全绿结论。
