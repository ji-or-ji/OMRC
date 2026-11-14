# HANDOFF — 新会话从这里开始

> 这是 OMRC 项目的交接文档。读这一份就能接上进度，不用翻聊天记录。
> 最后更新：2026-11-14 上午（W1a 完成 + 自拼路线定稿）

---

## 一、这是什么

**OMRC / Oppose member remote control**

局域网与公网双模式的远程桌面。安卓控 Win，Win 控 Win。个人自用，不商业。

痛点来源：现有远程桌面又难用又贵，且**用户的画面不该经过别人的服务器**。视觉上追求游戏级动效，但只在**没有视频的模式**里用。

---

## 二、当前状态

| 项 | 状态 |
|---|---|
| 仓库 | 双远端，最新 commit 见 `git log -1` |
| W1a 链路 | **已打通**：抓屏 → 硬编 → UDP → 重组 → 解码 → 上屏 |
| 实测（本机回环 1080p） | 35~43 fps，端到端 36~40 ms，客户端解码 8ms、绘制 2.7ms |
| 丢包补救 | NACK 两轮重传 → 补不回来要 IDR；5% 人为丢包下解码零报错 |
| 断线重建 | 切分辨率 / 独占全屏 / 锁屏后自动整套重建 |
| 锁屏与卡死 | 锁屏期间暂停采集；主循环卡死 3 秒自杀，配监督脚本自动拉起 |
| 编码后端 | 目前 Media Foundation（**编码滞后 25ms，是最大瓶颈**） |
| 输入注入 | **未做**（所以现在只能看不能控） |
| 安卓端 | 未开始，环境已就绪 |
| 环境 | Rust + VS Build Tools + Windows SDK + Android SDK/NDK + cargo-ndk 全部就绪 |

远端：

- https://gitee.com/ji-or-ji/OMRC
- https://github.com/ji-or-ji/OMRC

本地路径：`D:\project\software\OMRC`

验证：

```powershell
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
cargo test --workspace          # 22 个用例
cargo build --release --workspace
```

跑闭环（详见 `docs/05-W1a-闭环.md`）：

```powershell
target\release\omrc-client.exe --listen 0.0.0.0:9100 --size 1600x900
target\release\omrc-host.exe play --peer 127.0.0.1:9100 --fps 60 --encoder hw
```

压丢包：客户端加 `--loss 0.05`。主机端卡死重启：`tools\host_supervisor.ps1`。

---

## 三、已定死的决策

**七条根决定（不变）**

1. 内网与公网双模式，两条传输各自优化到底
2. 只做安卓控 Win 和 Win 控 Win
3. 后端用 Rust
4. **有视频的模式安静，没视频的模式才好动**
5. 手机控 Win 一律横屏，竖屏只能通过扩展屏模式实现
6. 侧边栏常驻，视频不占满整屏
7. **视频绝不参与 3D 变换**

**2026-11-13 晚新增，已拍板**

- **上自拼，不借壳。** 不整机套 Sunshine，主机端自己掌控。理由是项目初衷：画面不经过第三方。
- **公网部分交给朋友做。** 我们只把内网做到骨头，传输抽象切在「编码后的帧」，他接 WebRTC / 自建隧道 / Tailscale 都不影响核心。
  附带的便宜：对方若用虚拟组网，我们的内网模式**一行不改就能跨公网跑**（MTU 1200 + 32 包头，塞得进 1280 的隧道）。
- **编码后端三层可插拔**：NVENC 直连（N 卡）→ MF 硬件 MFT（三家通用兜底）→ 软编（最后手段，560ms 固有时延）。
  朋友是 AMD 核显，走第二层即可用；AMF/QSV 直连等真有卡再做。
- **许可纪律**：只用宽松许可的轮子（`moq-nvenc` MIT、`windows-capture` MIT、`webrtc-rs` MIT/Apache）。
  GPL/AGPL 项目只读文档与行为规格，不读源码；读了写进 `docs/SOURCES.md` 留痕。
- **H.264 专利与 GPL 无关**：对外发布时只走硬件编码 + 系统解码，专利费由芯片与系统厂商承担。

---

## 四、代码结构

```
crates/core/              纯 Rust，零平台依赖
  proto.rs                控制消息（光标、NACK、要 IDR、心跳），postcard
  transport.rs            Transport trait、EncodedFrame、Message
  input.rs                统一输入事件（未接入）
  video.rs                Codec / QualityProfile / EncodeConfig
  session.rs              状态机，含 Migrating 态
  pixel.rs                BGRA ↔ NV12（两端共用，带行距）
crates/transport-lan/     分片/重组/NACK 决策，15 个用例
host-win/                 主机端
  d3d.rs                  枚举、设备
  capture.rs              DXGI 复制、光标、锁屏探测
  video_proc.rs           D3D11 视频处理器 BGRA→NV12（3 纹理轮转）
  encode.rs               MF 硬件/软件 H.264（异步 MFT）
  overlay.rs              时间码叠字
  mft.rs                  MFT 诊断（omrc-host mft）
  main.rs                 管线、看门狗、NACK 回传处理、重建
client-win/               W1a 回环客户端（不是最终客户端）
  decode.rs / app.rs / main.rs
tools/                    截图、闪烁采样、主机端监督脚本
client-android/           只有 README（W1b 目标）
client-windows/           只有 README
```

---

## 五、下一步：五件事，按收益排序

1. **NVENC 直连。** 用 `moq-nvenc`（MIT，运行时 dlopen，不链 SDK），吃 D3D11 纹理零拷贝。
   目标：编码 25ms → 5ms 以内，整链路 37ms → 15ms 上下。
   接口按 trait 切三层，先写「NVENC + MF 兜底」两档。
2. **采集线程化。** `AcquireNextFrame` 在锁屏/切桌面/显示器休眠时会挂死不返回（超时参数无效）。
   现在靠「锁屏前先探测 + 卡死 3 秒自杀」顶着，线程化才是正解。
3. **输入注入。** SendInput 绝对坐标、多显示器负坐标、DPI、键盘扫描码。没有它永远是播放器。
4. **客户端显示换 D3D11 SwapChain。** GDI 拉伸又糊又慢，且与 vsync 对不齐。
5. **安卓端硬解上屏。** MediaCodec + SurfaceView，W1b。

---

## 六、环境细节

- **cargo 不在默认 PATH**：每条命令前 `$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"`
- **提交信息规范**（有钩子拦截）：不许用 `feat:` `fix:` `docs:` 这类前缀，用模块名或工作线名，例 `omrc: ...`、`transport-lan: ...`
- **Android**：SDK/NDK 在 `C:\Android\sdk`（NDK 27.2.12479018），`cargo ndk` 已实测能出 `.so`
- **不要用 PowerShell 改写带中文的源码**（会变 GBK 乱码并吞换行），一律用 write/edit
- **Hana 插件**：`%USERPROFILE%\.hanako\plugins\`，启动时扫描；已装 pc-status（提供 `machine_state` 工具）

---

## 七、踩过的坑

主机端/编码/显示共 8 条，全部写在 `docs/06-自拼路线.md` 第四节，包括：
`D3D_DRIVER_TYPE_UNKNOWN`、硬件 MFT 不实现 `IMFAttributes`、异步 MFT 不能等事件、
D3D 管理器与纹理输入、纹理池防抽黑、宏块对齐尺寸、GDI 双缓冲、软件编码器 560ms。

传输侧两个坑：NACK 死循环（放弃的帧被晚到的片“复活”）、接收与解码同线程导致缓冲积压。

---

## 八、文档索引

| 文件 | 说明 |
|---|---|
| `docs/06-自拼路线.md` | **最新战略文档**：分工、三层编码、五件事、坑清单、许可纪律 |
| `docs/05-W1a-闭环.md` | W1a 实测数据、链路结构、六个坑、可靠性 |
| `docs/04-环境清单.md` | 环境现状（含 Android 安装记录） |
| `docs/03-选型定稿.md` | 施工图，权威（主机侧选型已被 06 部分更新） |
| `docs/02-分层与构建.md` | 代码边界与三端结构 |
| `docs/01-视觉方案与UI层选型.md` | 陀螺仪伪 3D 拆解 |
| `docs/00-架构调研.md` | 早期调研，历史参考 |

---

## 九、给新会话的开场白

> 读 `HANDOFF.md` 和 `docs/06-自拼路线.md`。
> W1a 已通，下一步第一件事是 NVENC 直连（`moq-nvenc`），把编码滞后从 25ms 压到 5ms。
> 七条根决定不要重新讨论；公网归朋友，我们只管内网。
