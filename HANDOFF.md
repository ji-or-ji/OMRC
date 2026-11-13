# HANDOFF — 新会话从这里开始

> 这是 OMRC 项目的交接文档。读这一份就能接上进度，不用翻聊天记录。

---

## 一、这是什么

**OMRC / Oppose member remote control**

局域网与公网双模式的远程桌面。安卓控 Win，Win 控 Win。个人自用，不商业。

痛点来源：现有远程桌面又难用又贵，且用户只想在自己网络里控。视觉上追求游戏级动效，但只在**没有视频的模式**里用。

---

## 二、当前状态（2026-11-13 晚）

| 项 | 状态 |
|---|---|
| 仓库 | 已建，双远端，最新 commit `a649640` |
| 代码 | 可编译、可测试、可运行，是骨架不是空壳 |
| 环境 | Rust + VS Build Tools + Windows SDK 就绪 |
| Android SDK / NDK | **未装**，W1 的安卓部分需要 |
| Godot | **未装**，W2 才需要 |

远端：

- https://gitee.com/ji-or-ji/OMRC
- https://github.com/ji-or-ji/OMRC

本地路径：`D:\project\software\OMRC`

验证命令（都应通过）：

```powershell
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
cargo check --workspace --all-targets
cargo test --workspace      # 10 个用例
cargo run -p omrc-host
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
  proto.rs                握手魔数与能力位
  transport.rs            Transport trait、EncodedFrame、Message
  input.rs                统一输入事件（归一化坐标）
  video.rs                Codec / QualityProfile / EncodeConfig
  session.rs              状态机，含 Migrating 态（网络迁移用）
crates/transport-lan/     已实现分片与重组，10 个用例通过
host-win/                 目前只打印启动信息
client-android/           只有 README
client-windows/           只有 README
docs/                     设计与调研
```

`transport-lan` 已实现：

- `fragment(&EncodedFrame) -> Vec<Vec<u8>>`，32 字节包头 + 最大 1168 字节载荷
- `Reassembler`，容忍乱序与重复，缺片不吐半帧，能报 `missing()` 供 NACK
- 在途帧数上限，超限丢最旧

---

## 五、下一步：W1

目标：**打通「抓屏 → 编码 → UDP → 解码上屏」的最小链路。**
不做输入、不做 UI、不做虚拟屏、不做动效。

### 建议拆成两段

**W1a：纯 Windows 闭环（不需要安卓）**

先做主机端和一个本地回环接收端，两边都在 Rust 里，用 `127.0.0.1` 通信。这样整条链路能在桌面上调试，不依赖手机。

1. 初始化 D3D11 设备，枚举输出（`IDXGIFactory1` → `EnumAdapters1` → `EnumOutputs`）
2. `IDXGIOutputDuplication::DuplicateOutput` 建立桌面复制
3. 循环 `AcquireNextFrame` 拿 `ID3D11Texture2D`，处理 `DXGI_ERROR_ACCESS_LOST` 与 `WAIT_TIMEOUT`
4. 光标分离：`GetFramePointerShape` + `PointerPosition`，作为独立小消息发送（这一步对主观延迟影响极大，别省）
5. 编码：`MFTEnumEx` 找硬件 H.264 编码器，`CODECAPI_AVLowLatencyMode`，`MFT_MESSAGE_SET_D3D_MANAGER` 走零拷贝
6. 输出 NAL → `fragment()` → UDP 发送
7. 回环接收端：`Reassembler` → 解码 → 显示（可先用简单窗口）

验收：1080p60，端到端延迟 < 80ms，连跑一小时不崩。

测延迟的土办法最准：主机在画面角落渲染每帧变化的毫秒时间码，用另一台高帧率相机拍屏对比。

**W1b：换成安卓**

1. 装 Android SDK + NDK（见 `docs/04-环境清单.md`）
2. `rustup target add aarch64-linux-android x86_64-linux-android`
3. `cargo install cargo-ndk`
4. 最小 Android 工程：Activity + SurfaceView + MediaCodec 异步模式
5. `cargo ndk` 编出 `.so`，JNI 桥接

### Rust 依赖预告

W1 会用到 `windows`（DXGI / Media Foundation）、`tokio`（UDP 收发）、`snow`（Noise，如果要先上加密）。目前都还没加。

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
| `docs/03-选型定稿.md` | **当前施工图，权威** |
| `docs/04-环境清单.md` | 环境状态与安装步骤 |
| `docs/02-分层与构建.md` | 代码边界与三端结构，有效 |
| `docs/01-视觉方案与UI层选型.md` | 陀螺仪伪 3D 的技术拆解，有效；框架结论以 03 为准 |
| `docs/00-架构调研.md` | 早期调研，历史参考 |

---

## 九、给新会话的开场白

> 读 `HANDOFF.md` 和 `docs/03-选型定稿.md`，然后从第五节 W1a 开始做。
> 七条根决定不要重新讨论，有疑问先问再改。
