# Contributing to Mica

感谢你对 Mica 的兴趣!这是一个 Windows 原生终端模拟器(Rust),对标 Ghostty 的性能、功能与原生 UI。项目由独立开发者主导,欢迎高质量的 issue 和 PR。

## 快速上手

```bash
git clone https://github.com/yudongyouqing/mica.git
cd mica
cargo test --workspace          # 全量测试(core/render 跨平台)
cargo run --bin mica            # 运行(需要 Windows;其他平台是 stub)
```

- Windows 10 1809+ / Windows 11,Rust stable。
- pty 相关测试会启动真实子进程(unix 上 sh,Windows 上 ConPTY),首个 ConPTY 测试冷启动可能要几十秒,属正常。

## 架构速览(改代码前必读)

三个 crate,依赖方向严格单向:**mica-app → mica-render → mica-core**

| crate | 职责 | 纪律 |
|---|---|---|
| `mica-core` | 终端状态机、PTY、键位编码 | **零 GUI 依赖** |
| `mica-render` | 字形图集、颜色、grid → GPU(wgpu) | **只认 grid,不认窗口** |
| `mica-app` | Win32 窗口壳、输入路由、系统集成 | **最薄**,不含终端逻辑 |

设计全文见 `docs/superpowers/specs/`(决策依据)。

## 硬规则

1. **测试先行**:新功能/修 bug 先写会失败的测试。PR 必须带着覆盖变更的测试。
2. **零警告**:`cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings` 必须干净(CI 强制)。
3. **不引入 GPL 依赖**:项目是 MIT OR Apache-2.0,加任何依赖前先查它的 license。
4. **不改 crate 边界**:core 里出现 GUI 依赖、render 里出现窗口概念——这类 PR 直接拒绝。
5. **参考 Ghostty 不抄 Ghostty**:行为与协议语义可以对标上游,代码不复制。

## 提交与 PR

- Conventional commits:`feat:` / `fix:` / `test:` / `docs:` / `chore:`,正文说明动机。
- PR 到 `main`,标题同风格;描述里说清"改了什么、为什么、怎么验证的"。
- 提交署名用你自己的身份(git 配置好 user.name / user.email)。

## 声明的边界(不是 bug)

以下是有意的阶段性欠账,提交"修复"它们之前请先开 issue 讨论:M0 阶段 CJK 字形显示 `?`、每帧全量重绘、多数 cell flags 忽略、渲染为轮询循环。路线图见 README。

## License

提交即表示你同意以 MIT OR Apache-2.0 双许可发布你的贡献。
