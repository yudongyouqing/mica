# 修复整合与资源治理实施计划

> 按 subagent-driven-development 执行，主代理统一审查、push、PR 与合并。用户已授权直接执行前一轮四项顺序；不发版、不投递 winget。

**目标：** 整合已完成的修复，消除启动与早关闭缺陷，再使渲染资源有可验证的上限。

**架构：** 保持 app → render → core。每项独立分支、回归和 PR，通过 required checks 再合入 dev。测试使用隔离配置、管道身份及自有 PID。

## 整合

- [x] 重新核对 #110 的精确 head 与 Windows/macOS/merge-gate，通过后合并到 dev（3eeb6c0）。
- [x] 将 dev 合入 #101/#103/#105 的现有分支，保留原历史，不 force push。
- [ ] #101：审查裸协议应答、无重复借用及真实 WM_APP 回归，CI 通过后 merge。
- [ ] #103：补孤立续格的真实列映射与希腊 sigma 的一致折叠测试；CJK/emoji/İ/ASCII 回归保持通过。
- [ ] #105：QT 同样使用固定截止和 16ms 兜底；持续 WM_APP 不派 WM_TIMER 时仍绘制，隐藏时保留 damage、显示后补帧。

## #107：启动路径

文件：core/profile.rs、core/pty.rs、app.rs 的 profile 调用点与 lib.rs CLI。

- [ ] 红灯：真实带空格程序路径、空参数、引号与反斜杠参数；受控挂起/超量输出的枚举。
- [ ] WSL 扫描有真正 3s deadline、输出上限和子进程收尾，不留下无限 reader 线程。
- [ ] GUI 使用立即可用的内置项与缓存快照，后台刷新单飞、有冷却；CLI 可同步有界列举。
- [ ] Windows 引号解析与 PowerShell -Command 余串兼容；WSL 名中的空格完整保留。
- [ ] 精确单测、真实启动测试、workspace fmt/Clippy/test、规格与质量审查、CI 后合并。

## #109：配置 watcher 的启动所有权

文件：app.rs 启动/延迟初始化、tests/window_lifecycle.rs。

- [ ] 先给 debug 被测进程提供受控的 timer 7 延迟，新增不等待延迟段的启动辅助函数；只影响隔离测试进程。
- [ ] 红灯：打开副窗、关闭主窗，确认延迟段尚未运行，再写配置；副窗标题必须改变，IPC 必须仍可用。
- [ ] 首帧后、进入主窗消息循环前建立进程级 watcher，成功/失败仍按原降级规则；从 timer 7 移除该职责。
- [ ] watcher 仍只创建一次、主窗关闭不释放它；原已初始化存活/广播测试与新早关闭测试同时通过。
- [ ] 检查首帧顺序与窗口计数不回退，完整门禁与 CI 后合并。

## #106：渲染预算与复用

文件：render/font/{atlas,dwrite,router}.rs、pipeline.rs，及帧缓存失效与 app 绘制接线。

- [ ] 先采可重放的字形增长、唯一 shaping 行和多 pane 切换负载，报告实际字节、纹理创建/上传计数。
- [ ] 单 atlas、shaping、CPU 全进程与 GPU 缓存均有预算和 Drop 归还；用真实 identity 区分同值 revision。
- [ ] 超限回收只在帧边界换 generation；router/row/overlay 全部失效，构建途中不移动有效 UV。
- [ ] 单帧确实不可容纳时显示替代字形并限频诊断；预算释放后恢复，不静默空白，不每帧回收风暴。
- [ ] 纹理按 identity 保留、尺寸不变时复用并增量上传；缓存本身有上限。
- [ ] 保留并扩展多 pane 像素、identity/revision 撞值、预算满/释放、DPI 重建与 UV 一致性回归。
- [ ] 完整门禁、对照数据、审查与 CI 后合并。

## 后续 P2

先做背景图解码前预算与真实负载验收。app.rs 只随职责改动拆分；版本文档在发版前对齐。8h soak 和完整常驻内存归因不能用短时单测替代；未执行就明确保留未测状态。
