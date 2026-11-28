# Pane control / snapshots / events 补充审查

范围：#145、#146、#147，以及 PR #148。此次重新读取原始验收项和实现，未以首轮报告或 CI 绿灯代替逐路径验证。

首轮的 432 项测试通过，但遗漏了以下边界，因此当时“没有剩余阻断项”的完成结论过强。本报告替代首轮的完成评估；以下八项 P2 问题均已修复。

## 发现、复现与修复

| 问题 | 实际影响 | 修复与回归证据 |
| --- | --- | --- |
| 恢复已关闭标签绕过 pane 注册，且丢失显式 env | list 能看到新 pane，send/focus/read 按 ID 却失败；启动命令缺少原来的环境变量 | ReopenTab 恢复 launch env 并调用 pane_created；真实 CMD 输入/回读及 watch 的 created/closed 事件回归先失败后通过 |
| 默认 cwd 元数据与 PTY 实际启动目录不一致 | portable-pty 默认使用有效 USERPROFILE，原代码却记录 Mica 进程目录，影响枚举、继承和重启 | 统一解析有效启动目录，同时用于命令和元数据；真实 CMD %CD% 与混合大小写 USERPROFILE 覆盖回归先失败后通过 |
| QT 的 IPC focus 只 ShowWindow，没有补帧 | 隐藏期间已有输出，显示后仍是旧帧；路径也绕过正常 show 的会话恢复 | 抽出热键与 IPC 共用的 show，校验 ID、恢复会话并提交最新帧；实际隐藏输出/DRAW_COUNT 回归先失败后通过，重复 focus 保持可见 |
| 文本快照同时保留 HT 与跨越的填充格 | 消费方再次展开 Tab，列位置被重复计算 | HT 标记按可见空格导出，保留其余真实格；覆盖默认/自定义制表位、宽字符、尾部和被覆盖的格 |
| send 把程序选项误当作 Mica 选项 | mica send codex --resume 被拒绝 | 首个文本词后的普通程序参数按原样转发，保留旧的目标/--literal 语义；解析回归先失败后通过 |
| watch 在半行或半个 UTF-8 字符处断管时报错 | 服务端正常退出/取消慢写时，CLI 错误退出 1 | 按字节读取 NDJSON，仅解析完整记录，丢弃 EOF 残片；真实管道 ASCII/UTF-8 截断回归通过 |
| watch 的 stdout 堵塞时 Ctrl+C 只设置标志 | 主线程阻塞 write_all，无法返回读取循环检查标志 | watch 专用控制处理器直接 ExitProcess(0)，避免 Rust stdout 锁/flush；隐藏私有控制台中堵住 stdout 后实测 Ctrl+C 正常退出 0 |
| 终端侧 shell mark 队列溢出静默丢完成事件 | 单批 C/D 后接 5000 个 B，唯一 CommandEnd 被淘汰，watch 仍保持连接且无法发现缺失 | Surface 暴露生产端溢出信号，发布前取消相关全局流订阅并回收槽位；core 溢出与 bus 断连/回收回归通过 |

## 交叉审查

新增修复又经过两组只读交叉检查，覆盖：cwd 与命令构建顺序、reopen 的 owner/env/lifecycle、QT show 的借用与补帧、EOF 字节边界、控制处理器退出及生产端溢出接线。本轮追加修复未发现新的阻断项。

Ctrl+C 回归使用单独创建的隐藏控制台，发送信号前核对成员恰为测试 helper 与所属 watch 进程，不向用户控制台广播。测试用 helper 在常规 test harness 中标记 ignored，由父测试显式启动。

## 验证

- 本地严格 workspace clippy、格式检查通过。
- 全工作区测试：**444 passed，0 failed，8 ignored**（包括被父测试显式调用的辅助进程入口）。
- 真实集成回归覆盖恢复与重启后的稳定寻址/环境、生命周期事件、隐藏输出补帧、断管 UTF-8 边界与阻塞输出的 Ctrl+C。
- 桌面拖拽测试在自有窗口的固定场景模拟手势，恢复原鼠标位置，不放宽生产命中逻辑。
- 发布版继续验证独立实例的 JSON/NDJSON、cwd/env/pane ID、read 和 badge/focus；watch 传输回归也可通过 MICA_TEST_WATCH_BINARY 指向发布版。

未替换本机安装版，未修改个人 PowerShell/oh-my-posh 配置。
