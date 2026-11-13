# HANDOFF — 新会话从这里开始

> 这是 OMRC 项目的交接文档。读这一份就能接上进度，不用翻聊天记录。

---

## 一、这是什么

**OMRC / Oppose member remote control**

局域网与公网双模式的远程桌面。安卓控 Win，Win 控 Win。个人自用，不商业。

痛点来源：现有远程桌面又难用又贵，且用户只想在自己网络里控。视觉上追求游戏级动效，但只在**没有视频的模式**里用。

---

## 二、当前状态（2026-11-13 深夜）

| 项 | 状态 |
|---|---|
| 仓库 | 已建，双远端 |
| W1a | **已打通**：抓屏 → 硬件编码 → UDP → 重组 → 解码 → 上屏，1920×1080、37~41 fps、端到端 37ms |
| 代码 | 可编译、可测试、可运行 |
| 环境 | Rust + VS Build Tools + Windows SDK 就绪 |
| Android SDK / NDK | **未装**，W1b 需要 |
| Godot | **未装**，W2 才需要 |

远端：

- https://gitee.com/ji-or-ji/OMRC
- https://github.com/ji-or-ji/OMRC

本地路径：`D:\project\software\OMRC`

验证命令（都应通过）：

```powershell
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
cargo test --workspace          # 19 个用例
cargo build --release --workspace
```

跑 W1a 闭环（两个终端，详见 `docs/05-W1a-闭环.md`）：

```powershell
target\release\omrc-client.exe --listen 0.0.0.0:9100
target\release\omrc-host.exe play --peer 127.0.0.1:9100 --seconds 6 --fps 60 --encoder hw
```

---

## 三、已定死的决策

**七条根决定，其余全是推论，不要重新讨论。**

1. 内网与公网双模式，两条传输各自优化到底
2. 只做安卓控 Win 和 Win 控 Win，不做 Mac / Linux / Win 控安卓
3. 后端用 Rust
4. **有视频的模式安静，没视频的模式才好动**
5. 手机控 Win 一律横屏，竖屏只能通过扩展屏模式实现
6. 侧边栏常驻，视频不占满整屏
7. **视频绝不参与 3D 变换**

**技术选型：**

| 层 | 选择 |
|---|---|
| 主机 | Rust |
| 客户端 UI | Godot 4（Android + Windows 共用工程）+ 薄 Kotlin 胶水 |
| 采集 | DXGI Desktop Duplication 主路，Windows.Graphics.Capture 兜底 |
| 编码 | Media Foundation 硬件 MFT 起步，后期 NVENC / AMF 原生 |
| 编码参数 | H.264 High、无 B 帧、CBR、无限 GOP、LowLatency |
| 内网传输 | 裸 UDP + 自定义分片 + Noise（`snow`），**无 pacing、无抖缓冲、无拥塞控制** |
| 公网传输 | `webrtc-rs`，全套 ICE / DTLS-SRTP / NACK / FEC |
| 视频显示 | SurfaceView + `setZOrderMediaOverlay(true)`，压在 Godot 全屏表面之上 |
| 虚拟显示器 | VirtualDrivers/Virtual-Display-Driver（IddCx），不自研驱动 |
| 终端 | ConPTY |
| 输入 | SendInput + InjectTouchInput + InjectSyntheticPointerInput |

**边界原则：Rust 管字节，平台管纹理。**
Rust 不认识 Surface、窗口、纹理。这条纪律保证双端能在同一个 `cargo test` 里跑。

---

## 四、代码结构现状

```
crates/core/              纯 Rust，零平台依赖
  proto.rs                控制消息（光标形状/位置、关键帧请求、心跳），postcard 序列化
  transport.rs            Transport trait、EncodedFrame、Message
  input.rs                统一输入事件（归一化坐标）
  video.rs                Codec / QualityProfile / EncodeConfig
  session.rs              状态机，含 Migrating 态（网络迁移用）
  pixel.rs                BGRA ↔ NV12 换算（两端共用，带行距版本）
crates/transport-lan/     分片与重组，14 个用例；控制包走同一套分片、按 CONTROL 位分流
host-win/                 主机端
  d3d.rs                  适配器/输出枚举、D3D11 设备
  capture.rs              DXGI 桌面复制、光标形状提取
  overlay.rs              时间码叠字（GDI 小 DIB → UpdateSubresource）
  video_proc.rs           D3D11 Video Processor：BGRA → NV12（显存内）
  encode.rs               MF 硬件/软件 H.264，异步 MFT 支持
  mft.rs                  MFT 诊断（omrc-host mft）
client-win/               W1a 回环接收端（不是最终 Windows 客户端）
  decode.rs               MF 软解 + 宏块对齐裁剪
  app.rs                  GDI 窗口、光标合成
  main.rs                 收包线程 + UI 线程、离线回放
client-android/           只有 README
client-windows/           只有 README（最终客户端 W2 再写）
```

关键实现细节见 `docs/05-W1a-闭环.md` 第四节（六个坑，全是会再踩的）。

---

## 五、下一步：W1b（换成安卓）

W1a 的验收已经过了（硬件路径 37ms < 80ms）。接下来两条路，按 `docs/03` 的顺序走：

**W1b：安卓端**

1. 装 Android SDK + NDK（见 `docs/04-环境清单.md`）
2. `rustup target add aarch64-linux-android x86_64-linux-android`
3. `cargo install cargo-ndk`
4. 最小 Android 工程：Activity + SurfaceView + MediaCodec 异步模式
5. `cargo ndk` 编出 `.so`，JNI 桥接

**W1a 还剩的收尾（可选，不急）**

- NACK 与关键帧请求：`Reassembler::missing()` 和 `Encoder::request_keyframe()` 都已就位，缺一条回传链路
- 复制链路断开（切分辨率 / 全屏独占 / 锁屏）后自动重建，现在直接退出
- 多显示器、负坐标

### Rust 依赖现状

已加：`windows`（DXGI / D3D11 / MF）、`anyhow`、`serde` + `postcard`、`tracing`。
未加：`tokio`（W1a 用 std 线程够）、`snow`（Noise 加密，等控制通道定下来再上）。

---

## 六、环境细节（新会话必须知道）

**cargo 不在默认 PATH**，每条命令前要加：

```powershell
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
```

**VS Build Tools**：`C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools`，带 Windows SDK。rustc 能自动找到链接器。

**提交信息规范**（有钩子拦截）：不许用 `feat:` `fix:` `docs:` 这类类型前缀，要用模块名或工作线名，例如 `omrc: ...`、`transport-lan: ...`。

**工作目录**：如果会话记录的 cwd 与 `D:\project\software\OMRC` 不一致，用绝对路径或给命令传 `workdir`。

---

## 七、五个待验证的风险

| # | 风险 | 何时验 |
|---|---|---|
| 1 | Godot 安卓端 IME，终端打中文能不能用 | W3 |
| 2 | VDD 能不能报自定义 DPI / 物理尺寸（不解决则扩展屏字太小） | W9 |
| 3 | SurfaceView + ZOrderMediaOverlay 与 Godot 并存的真机表现 | W2 |
| 4 | 横竖切换时 Surface 重建，解码器怎么续 | W9 |
| 5 | `webrtc-rs` 成熟度与硬编对接 | W8 |

第 1 条是唯一可能动摇 Godot 这个选择的。退路：终端输入框用原生 EditText 盖上去。

---

## 八、文档索引

| 文件 | 状态 |
|---|---|
| `docs/05-W1a-闭环.md` | **本轮记录**：实测数据、链路结构、六个坑 |
| `docs/03-选型定稿.md` | **当前施工图，权威** |
| `docs/04-环境清单.md` | 环境状态与安装步骤 |
| `docs/02-分层与构建.md` | 代码边界与三端结构，有效 |
| `docs/01-视觉方案与UI层选型.md` | 陀螺仪伪 3D 的技术拆解，有效；框架结论以 03 为准 |
| `docs/00-架构调研.md` | 早期调研，历史参考 |

---

## 九、给新会话的开场白

> 读 `HANDOFF.md` 和 `docs/05-W1a-闭环.md`，W1a 已经跑通。
> 下一步是 W1b（安卓端），先装 Android SDK / NDK。
> 七条根决定不要重新讨论，有疑问先问再改。
