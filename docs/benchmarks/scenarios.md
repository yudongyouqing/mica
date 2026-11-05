# 性能与资源基准:场景与方法(T01,2026-11-01 起)

> 本目录是 Mica 性能/内存/稳定性工作的**测量契约**:场景先定义、数据带
> 配置、结论必须可回放。计划文档(`docs/superpowers/plans/2026-11-01-
> comprehensive-hardening.md`)给出的预算是"初始建议",在受控机器上
> 以本目录方法定档后才作为门槛。

## 采样工具

`scripts/measure-resources.ps1`(只读,按 PID 绑定):

- 目标进程与子进程树(shell/conhost)**分开统计**,不重复相加。
- 指标:CPU 累计/区间 %(1 核满载=100%)、Working Set、Private Bytes、
  句柄、线程;GPU Process Memory(计数器不可用则记空并在结果中标注)。
- 输出 CSV,每秒一行(可调)。

## 标准场景

| ID | 场景 | 操作定义 | 采样 |
|---|---|---|---|
| S1 | 空闲单标签 | 启动→首 shell ready→静置 | 60s |
| S2 | 5 标签 | S1 + 4×`mica new-tab` | 60s |
| S3 | 10 标签 | S1 + 9×`mica new-tab` | 60s |
| S4 | 持续输出 | `mica send` 向活跃标签注入 `while($true){Get-Date -Format o}` 循环 | 60s,结束后 Ctrl+C |
| S5 | 开关标签循环 | 交替 `new-tab` / 关闭(Ctrl+W;当前仅 SendKeys 可自动化,best-effort) | N 轮 |
| S6 | 4 分屏 | 分屏键位 ×3(需输入自动化,随 T03 落地) | 60s |
| S7 | 隐藏 QT | QT 开→隐藏→持续输出(随 T03/T09 落地) | 60s |
| S8 | 长时 soak | 30min 混合负载→8h(T20 执行) | 全程 |

**自动化约束(如实记录,不得装作测过):** 关标签/分屏/QT 无 IPC 或 CLI
入口,目前只能 SendKeys;S5 数据带"best-effort"标记,S6/S7 未测。

## 环境记录(每次结果目录必须附)

- exe:git commit + profile(release)+ 版本
- OS build(`winver` / `cmd /c ver`)、Rust 版本
- CPU 型号/核数、内存、GPU 型号与驱动、显示器 DPI/刷新率
- 配置:字体族/字号、主题、背景图开关、history 默认(10,000)、
  透明度/背景(默认关)

## 统计口径

- 帧类指标给 median/p95/p99 + 样本量 + 测量区间;吞吐另可给 best 值
  作诊断,但**不得**只以 best 代表交互稳定性。
- 内存曲线看:稳态(后 50% 样本 median)、峰值、增长斜率(线性拟合,
  用于泄漏判定:关闭/重启循环后段斜率)。
- 单次运行噪声大(共享 runner 实测 2x):同配置至少 3 次取分布对比,
  修前/修后同机同时段进行。

## 结果目录

`results/<日期>-<主题>/`:原始 CSV + `summary.md`(环境块 + 结论 +
未测项)。当前基线见 `results/2026-11-02-baseline/`。

## T05 分批绘制的性能对照义务(2026-11-02)

T05 的逐批 submit(每 pane 独立 encoder+submit)与每批可能的图集重绑
是**正确性方案**:多 pane 下每帧纹理新建次数、上传字节、submit 数、
绑定重建数必须纳入 T14/T19 的对照测量,与本批修复前的单次提交路径
比较后才可下性能结论——当前**不宣称**性能达标或退化。
