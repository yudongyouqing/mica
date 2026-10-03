# kitty graphics protocol 预研(M4c/D31)

- 日期:2026-10-03
- 性质:纯预研,不写代码。M5 排期依据(M4 spec §8)
- 来源:kitty 官方协议文档(sw.kovidgoyal.net/kitty/graphics-protocol/,2026-10 抓取)、
  Ghostty 上游实现规模(main 分支文件树)、mica 自身管线核实、**两项本机实证**(见 §6/§7)

## 结论速览

| 问题 | 答案 |
|---|---|
| 协议大不大 | 大:命令键 ~25 个、传输 4 通道、放置含虚拟占位与相对放置,状态语义密 |
| 能不能进字形图集 | **不能**。独立 id→texture 存储(§5) |
| vte 能不能解析 | **不能**。vte 0.15 无 `apc_dispatch`,APC 载荷被静默丢弃(§6) |
| Windows 主路径可达吗 | **当前不可达**。ConPTY 剥离 APC(字节级实证,§7) |
| 工作量 | Ghostty 全量 ~15k 行(含测试);mica 最小子集估 6-7k 行(§8) |
| M5 建议 | 依赖 ConPTY 透传演进;协议层可 headless 先行,整体立项与否留给 M5 拍板(§9) |

## 1. 协议结构

一切走 APC:`ESC _ G <control> ; <payload> ST`,control 是逗号分隔的 `k=v`,
payload 为 base64(RFC 4648)。APC 的好处:旧终端整体忽略,天然向后兼容。

### 动作(`a=`)

- `t/T`:transmit-and-display(大写=不移动光标语义变体,配合 `C`)
- `p`:放置已传输的图(配 `i=`)
- `q`:查询(加载并应答,不存储不替换——探测用)
- `d`:删除(族见 §4)
- `f`:动画帧数据(后续块必带)

### 数据格式(`f=`)

- `32`:RGBA8(默认)——与 mica 图集纹素格式一致
- `24`:RGB8(sRGB,必须给 `s`,`v` 尺寸)
- `100`:PNG(尺寸从数据读;与压缩联用时必须给 `S`)

### 几何/放置键

- 源矩形:`x,y,w,h`(像素);`s`=宽、`v`=高
- 目标:`c`=列、`r`=行(只给一个则按纵横比推另一个;都给则信箱化)
- 像素偏移:`X,Y`(格内,须小于格子尺寸)
- `z`:层序(§5)

### 传输/状态键

- `t=`:`d` 直接(默认)/`f` 文件/`t` 临时文件/`s` 共享内存
- `S`=字节尺寸、`O`=偏移(文件/SHM 部分读)
- `o=z`:zlib deflate(RFC 1950),在 base64 之前
- `m=1/0`:分块标志;`q=1/2`:抑制 OK/错误应答
- `i`=图 id(1..=u32::MAX)、`I`=图编号(非唯一标签)、`p`=放置 id
- `C=1`:光标不动策略;`U=1`:虚拟放置;`N`:使用提示(transient)
- `P,Q,H,V`:相对放置(父图/父放置 + 格偏移)

## 2. 传输机制

- **直接(t=d)**:base64 进转义序列,**≤4096B 分块**,非末块须 4 的倍数;
  首块带全量 control,后续块只带 `m`(动画再加 `a=f`)。块间不得插其他
  graphics 序列;末块到达才算放置,校验失败整体不渲染
- **文件/临时文件(t=f/t)**:常规文件;临时文件须在已知 temp 目录且路径含
  `tty-graphics-protocol`,读后删;须跟随符号链接、拒绝设备/套接字、
  可拒绝敏感路径(`/proc` 等)。**防信息泄露契约:所有读失败统一回
  `EBADF:Failed to read image file`**(真实原因只进日志)
- **共享内存(t=s)**:POSIX shm_open / Windows 命名对象,读完 unlink
- **压缩(o=z)**:任意格式可叠 deflate,先解压再解释

## 3. 存储模型(id-keyed)

- `(image id, placement id)` 二级键:一张图数据,多处放置
- 重传同 id 数据 = **删图及全部放置**,新数据不自动显示(防无关程序复用 id
  产生分歧行为);`i` 与 `I` 同给 = EINVAL
- 删除族(`d=` 键 × 大小写,大写=连数据一起释放):`a/A` 全屏、`i/I/n/N`
  按 id/编号(+`p`)、`c/C` 光标处、`f/F` 动画帧、`p/P` 格、`q/Q` 格+z、
  `r/R` id 区间、`x/X` 列、`y/Y` 行、`z/Z` 层
- 存储压力下**先淘汰无放置的图**(终端自由裁量)

## 4. 放置

- 默认原点 = **当前光标格左上角** + `X,Y` 像素偏移;随后光标右移 c 列下移
  r 行(出屏/滚动区为 UB),`C=1` 抑制
- 同 `(i,p)` 重发 = 原位替换不闪烁(resize/move 的正规路径)
- **滚动跟随**:设计要求图随文本滚动——锚必须是 buffer 行坐标而非视口行
- **Unicode 占位符(U+10EEEE,0.28+)**:宿主应用在文本里摆 PUA 占位格,
  图 id 编码进前景色、放置 id 进下划线色、行列进组合变音符;虚拟放置
  只能被 `d=i,I,r,R,n,N` 删除。宿主(如 kitty 集成的 tui)自己动文本
- **相对放置(0.31+)**:`P/Q` 父 + `H/V` 格偏移,链深 ≥8,环检测

## 5. 与 mica 渲染管线的融合点

### 5.1 不进字形图集(结论)

mica 字形图集(atlas.rs,294 行)的四个契约都与大图冲突:

| 字形图集契约 | 大图冲突 |
|---|---|
| shelf 行分配,放不下倍增高 + 全量重排 | 4K 截图 33MB 触发 O(N) 重排链 |
| 每次写入 revision 门控**全量重传** | 插一张图 = 整个字形图集重上传 |
| 条目表全量保真(重排搬动) | 字形量级百级 vs 图 MB 级 |
| 单纹理尺寸 | GPU max texture(典型 8192/16384)小于大图 |

**正解:独立 `ImageStorage`**(id → 独立 `wgpu::Texture`,`Rgba8Unorm`,
`COPY_DST | TEXTURE_BINDING`,每图独立 bind group 或 binding array),
容量上限 + 无放置先淘汰(§3 契约)。Ghostty 同构:renderer/image.zig 与
字形图集分离。

### 5.2 层次与两遍发射契约

现契约:先全部整格背景 quad、后全部字形 quad(blend:None 下防宽字形被
背景擦除)。扩展为按 z 分段:

```
bg quads → z<INT32_MIN/2 的图 → (非默认背景格已是 bg quads 的一部分)
z<0 的图 → 字形 quads → z≥0 的图(按 z 升序,同 z 低 id 在下)
```

z 语义细节(text 的隐式 z、同 z 平局)实现前**再核官方文档**(本次抓取
未覆盖 =0 平局的完整措辞)。同 z 多图 alpha 混合需要 blend 状态:
现管线 blend:None,图段需独立 blend(premultiplied 或 straight-alpha,
与图集"存储非预乘"的既有决策对齐)。

### 5.3 滚动跟随的坐标系

placement 锚 = buffer 坐标 Point(与 M2a 选择、M4b RowMark 同一坐标系
结论:上游光标/网格锚恒在活动屏帧,渲染换算 `grid[Line(l - offset)]`,
frame.rs 既有公式直接复用)。滚动时锚随内容漂移的方向已在 M4b T5 的
坐标系核实中定过性。

### 5.4 采样与缩放

图放置格数与像素尺寸不一致时需缩放:Nearest(块状忠实)或 Linear
(平滑)。c/r 派生纵横比时用 Linear 贴合 kitty 观感——M5 实现时以
kitty/ghostty 实测观感定。

## 6. vte 侧:APC 不可经 Perform 捕获(本机实证 ①)

**vte 0.15 无 `apc_dispatch`**(源码核实):`SosPmApcString` 态的载荷字节
只走 `anywhere`(C0/C1/ESC),无任何 Perform 回调——APC 内容被静默丢弃。
主 Term 的 Processor 同样丢弃(无冲突,但也拿不到)。

**接入路径二选一**:

1. **扩展 D29 前缀门控为 `ESC_G` 捕获器**(推荐):sidecar 门控已在做
   字节级前缀扫描,加一个 ~50 行的 APC-G 状态机(捕获 `ESC _ G` 到 ST,
   4K 分块跨 feed 缓冲——与 carry/open_seq 同款流式模式,M4b 已验证)。
   零新依赖、零上游 patch
2. `[patch.crates-io]` fork vte 加回调:维护负担,否决

## 7. Windows 可达性:ConPTY 剥离 APC(本机实证 ②,核心新事实)

**字节级探针**(2026-10-03,Win11 26200,tests/powershell.rs):PowerShell
经 ConPTY 输出完整 `ESC _ G q=1,i=31,s=1,v=1,t=d,f=24;AAAA ESC \` 序列,
客户端读回的输出流中:

- `ESC _ G` 全序列**零出现**(1323 字节输出全文无匹配)
- 命令回显文本含 "_G" 字面量——**假阳性源**,断言必须匹配完整三字节
- 观察到孤立 `\`(ST 终结符的泄漏)以可见字符到达

即:**ConPTY 消费 APC 不透传**——与 DECSET 2026(M4b 冒烟实证)、
OSC 52(疑)同类边界。含义:

- kitty graphics 在 Windows 主路径(PowerShell/cmd/WSL,凡经 ConPTY)
  **当前不可达**,协议层做对了也收不到序列
- 依赖项:ConPTY 的 VT 透传演进(Windows Terminal 1.22+ 的 passthrough
  能力何时进入 CreatePseudoConsole 默认路径 / portable-pty 何时采用)
- 已落**金丝雀测试** `conpty_strips_apc_today_canary`:锁当前行为,未来
  ConPTY 加透传时它变红 = 边界移动,重估可达性

## 8. 工作量粗估

### 上游参照(Ghostty main,gh api 文件树,字节数)

| 文件 | 大小 | 内容 |
|---|---|---|
| terminal/kitty/graphics_storage.zig | 183KB | 存储/放置(最大件) |
| terminal/kitty/graphics_exec.zig | 122KB | 执行语义 |
| terminal/kitty/graphics_image.zig | 78KB | 图数据/解码 |
| terminal/kitty/graphics_command.zig | 60KB | 命令解析 |
| terminal/kitty/graphics_unicode.zig | 44KB | Unicode 占位符 |
| renderer/image.zig | 55KB | GPU 侧 |
| 其余(animation/pixel/render) | ~15KB | |

合计 ~555KB Zig ≈ **14-16k 行**(含测试与表),另有 C legacy 76KB。
spec §2 D31 的"~数千行大件"估计成立且偏保守。

### mica 最小子集(估)

范围:t=d/f 传输(f=32/24/100,PNG 解码)、基础放置(光标原点+c/r/X/Y)、
z 层序、delete 基础族、应答与 DA1 探测;**不做** animation、Unicode 占位符、
相对放置、共享内存。

| 块 | 估行数 |
|---|---|
| ESC_G 捕获器 + 命令解析(含分块/压缩) | ~1.5k |
| 存储/放置模型 + 删除族 + 滚动锚 | ~2.5k |
| wgpu ImageStorage + 分段渲染 | ~1k |
| 测试(表驱动,headless TDD) | ~2k |
| **合计** | **~7k 行,一个完整里程碑切片** |

依赖候选(加前查 license,硬约束):`png`(MIT,解码 f=100)、
`miniz_oxide`(MIT/Zlib,deflate;或 `flate2` MIT/双许可)。

## 9. M5 排期建议

1. **不建议 M5 立项实现**:核心价值被 ConPTY 边界堵死(§7),做完协议层
   真机也收不到序列,投入产出最差
2. 若 M5 决定做:**协议层 headless 先行**(捕获器/解析/存储/应答全部可
   单测,金丝雀变红即接入真机),渲染层随后;范围按 §8 最小子集切
3. 金丝雀测试是边界哨兵,长期保留
