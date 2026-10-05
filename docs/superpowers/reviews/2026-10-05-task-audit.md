# 任务级对账(M0-M5c 全 plan vs 代码)

- 日期:2026-10-05;方法:每个 plan 的任务提取可核验技术声明(API/
  常量/行为),grep+读码逐条核实。第四轮审查(前三轮见 1.0.8-audit.md)。

## 对账矩阵(64 项声明)

| Plan | 任务数 | 核实声明数 | 通过 | 备注 |
|---|---|---|---|---|
| M0 骨架 | — | 2 | 2 | 三 crate ✓、CI 双平台(5 job 含 perf/msi/merge-gate)✓ |
| M1-A 字体 | 7 | 7 | 7 | 双计数+测试锁、64B+size_of、CLEARTYPE_3x1 回归锁、代理对、回退链日志、round 取整 ×10 处 |
| M1-B 配置 | 9 | 8 | 8 | 三层合成、630 金样本、保旧弹窗 ×13、OSC4、事件化、脏区、BOM(app 层 ×2 处,与文档"app 层剥"一致)、DECCKM |
| M2a 标签 | 8 | 8 | 8 | Alt 路由、follow/pin、上游 Selection ×7、剪贴板智能语义、wt_default、TABS 池、WT 键位 |
| M2b 分屏 | 9 | 9 | 9 | 布局树、RGBA mix、COLR、连字管线、OSC52 护栏、DECSCUSR ×10、NCCALCSIZE ×11、hit-test ×8、ClosePane 升级 |
| M3a IPC | 6 | 6 | 6 | WSL 双解码、PIPE_NAME、JSON 帧、CLI 四入口、jumplist COM、**CREATE_NO_WINDOW ×2** |
| M4a kitty | 5 | 5 | 5 | encode 纯函数、状态机激活、KEYUP、C0 丢弃、bracketed paste |
| M5a 打磨 | 9 | 9 | 9 | marks/epoch ×13、jump ×19、ExitDot、hover、下划线族、extend_edge、bold_italic ×8、立方公式、IPC font_size |
| M5b 健壮 | 5 | 5 | 5 | minidump ×4、GPU_LOST、poll_exits/restart、WM_DPICHANGED/DPI_SCALE、position_ime |
| M5c 发布 | 5 | 5 | 5 | 孤儿清杀、uia.rs+测试双存留、perf 3 轮、README 4 处、winget manifest |

## 结论:**64/64 通过,零漂移**

所有 plan 的"已完成"声明与代码一致。M1-B 的 BOM 剥离位置(app 层非
core)与 CLAUDE.md 记载一致;M2b OSC52"真机疑拦截"是已声明边界非
漂移;M5c 的 UIA 撤线后模块+测试双存留(恢复路径完好)。

任务级对账 + 前三轮(架构 13 项/模式扫描/五维度)构成完整审查闭环。
