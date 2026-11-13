# 安卓客户端

Godot 4 工程，尚未创建。

## 待办

- 安装 Godot 4（含 Android 导出模板）
- 安装 Android SDK + NDK，配置 `cargo-ndk`
- 建立 Godot 工程骨架：主界面（3D 卡片）、侧边栏、内容区 SurfaceView
- `core-ffi/gdext` 把 Rust 核心编成 `.so` 挂进来

## 关键约束

- 横屏锁定，竖屏只能通过扩展屏模式实现
- 侧边栏常驻，视频贴内容区，不占满整屏
- 视频用 `SurfaceView` + `setZOrderMediaOverlay(true)`，压在 Godot 全屏表面之上
- 有视频的模式不做动效，没视频的模式（主界面、终端）才做
