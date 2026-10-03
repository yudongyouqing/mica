# M5b 实施计划:健壮性五件(§4 of M5 spec,D35)

- 分支:`feat/m5b-robustness`;上游 spec:`2026-10-03-m5-1.0-design.md`
- API 核实(2026-10-03,源码实证):
  - wgpu 30:`Device::on_uncaptured_error(Arc<dyn UncapturedErrorHandler>)`
    + `set_device_lost_callback(impl Fn(DeviceLostReason, String) + Send)`
  - windows-rs 0.62 `Win32_System_Diagnostics_Debug`(需加 feature):
    `MiniDumpWriteDump(hprocess, pid, hfile, MINIDUMP_TYPE, ...)`;
    Ime 模块(需加 feature):ImmGetContext/ImmSetCompositionWindow/
    ImmSetCandidateWindow/ImmReleaseContext;WM_IME_* 在 WindowsAndMessaging
  - portable-pty:`PtySession::try_wait() -> Option<ExitStatus>`(已有)+
    `ExitStatus::exit_code()`;Reader EOF 时通道关(退出≠静默已可判)
  - winres(M3c 已用):`set_manifest` 嵌 PMv2 DPI manifest

## 任务

### T1 panic hook + 本地 minidump(先做:后续排障地基)

- `main` 最早处装 `std::panic::set_hook`:dmp 写
  `%LOCALAPPDATA%\mica\crashes\mica-<yyyymmdd-hhmmss>.dmp`
  (MiniDumpWriteDump,MiniDumpNormal|WithDataSegs;GetModuleHandleW
  dbghelp 先 Ensure)?直接 link dbghelp 即用;panic 消息/位置写同基名 .txt;
  MessageBoxW 提示路径(线程有消息权时)。**永不上传**
- 隐藏自检 CLI `mica --panic-test`:集成测试 spawn 断言 dmp 落盘
- feature:+Win32_System_Diagnostics_Debug

### T2 device lost 自动重建

- `create_context` 成功即挂:on_uncaptured_error(eprintln 记档)+
  set_device_lost_callback → TLS `GPU_LOST` 置位
- 消息泵轮次(WM_APP_RENDER / WM_TIMER 空闲)检 GPU_LOST → 重建
  WindowGpu(instance→adapter→surface→config→renderer),全部 tab 的
  `renderer_atlas_revision = u64::MAX`(强制图集重传)+ force_full
- 真机触发难自动化(禁显卡):代码审查 + 单测锁标志逻辑;
  冒烟清单留手验项(spec §6 第 3 条)

### T3 PTY 退出态 + 重开

- Terminal 增 `exited: Option<u32>`;WM_APP_RENDER 尾(或 1s WM_TIMER)
  各 pane `session.try_wait()` → 记 code;exited pane 冻结尾帧
  (EOF 后无新数据,天然冻结;draw 跳过其行缓存重建)
- strip 活跃 pane 退出时标题尾缀 ` [exit N]`
- Action::RestartPane(默认 Ctrl+Shift+R):同 shell 重 spawn,复用 pane 槽
  (id 不变,forwarder 重挂);标签尾 pane 退出仍需显式关(语义不变)
- core 侧无逻辑(exit 是壳层簿记),测试走集成:ConPTY 跑 `exit 7` 断言
  exited=7 + 重开后新会话活

### T4 per-monitor DPI(退役写死 96)

- build.rs:`res.set_manifest` 嵌 PMv2 DPI-aware manifest
  (PerMonitorV2 + fallback PMv1;GdiScaling 否)
- WM_DPICHANGED(wparam 高 16 位 = 新 DPI;lparam = 建议矩形):
  → `reload_dpi(dpi)`:seed 字号 × dpi/96 重建全部 router/metrics/网格
  + ConPTY resize;窗口按建议矩形 SetWindowPos(系统惯例)
- 现有 96 写死处(度量换算)不动——入口乘 scale,数学层零变化
- 测试:纯函数 dpi→字号换算;真机多屏手验(冒烟清单第 8 条)

### T5 IME 组合窗口跟随 + 冒烟

- WM_IME_SETCONTEXT/WM_IME_COMPOSITION:ImmGetContext →
  ImmSetCompositionWindow(CFS_POINT,光标格屏幕坐标)+
  ImmSetCandidateWindow(CFS_EXCLUDE,光标格下缘)
- 组合中文本(WM_IME_COMPOSITION GCS_COMPSTR):以虚线下划线预显进
  grid(写 temp overlay:v1 简化为组合串直接作为 strip 提示?否——
  按格写入 grid 需旁路 surface;v1:候选/组合窗交给系统原生(跟随
  已修),组合内不上屏文本,GCS_RESULTSTR 结果经 WM_CHAR 路径上屏
  ——PS 下实测若结果串走 WM_IME_CHAR 而非 WM_CHAR,补转发)
- 冒烟矩阵:微软拼音 + 搜狗(候选跟随/上屏/中英切换)

## 纪律

- 每任务:fmt / clippy -D warnings / 全量绿后提交
- 冒烟清单复用 M5a 工装(drive.ps1 前台验证版)
