# Windows 客户端

与安卓端共用同一套 Godot 工程，两处不同：

- 视差触发源从陀螺仪换成**鼠标悬停位置**
- 视频走 D3D11 SwapChain + Media Foundation 硬解

## 待办

- 安装 Godot 4
- 建立共享场景与主题
- 接入 Rust 核心（`gdext` 或 C ABI）
