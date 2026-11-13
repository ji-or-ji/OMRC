# OMRC

**Oppose member remote control**

局域网 / 公网双模式的远程桌面。安卓控 Win，Win 控 Win。

## 状态

设计阶段。选型已定稿，见 `docs/03-选型定稿.md`。

**新会话请先读 `HANDOFF.md`。**

## 文档

| 文件 | 内容 |
|---|---|
| `docs/00-架构调研.md` | 需求拆解、路线对比、参考实现清单、风险清单 |
| `docs/01-视觉方案与UI层选型.md` | 陀螺仪伪 3D 的技术拆解、UI 框架对比 |
| `docs/02-分层与构建.md` | 代码边界、三端结构、构建链路、验证清单 |
| `docs/03-选型定稿.md` | **当前唯一有效的施工图** |
| `docs/04-环境清单.md` | 依赖状态与安装步骤 |

## 结构

```
crates/core/              协议、会话、编解码抽象、输入模型（纯 Rust，零平台依赖）
crates/transport-lan/     内网传输：裸 UDP + 自定义分片 + Noise 加密
host-win/                 Windows 主机：抓屏、编码、输入注入、ConPTY、虚拟显示器
client-android/           安卓客户端：Godot 工程
client-windows/           Windows 客户端：同一 Godot 工程
docs/                     设计与调研
tools/                    脚本与辅助工具
```

## 边界原则

**Rust 管字节，平台管纹理。**

Rust 不认识 Surface、窗口、显卡纹理，只负责把画面变成有序的包、把包还原成帧。

## 构建

待补。里程碑 W1 的目标是打通「抓屏 → 编码 → UDP → 安卓解码上屏」的最小链路。

## 许可

[PolyForm Noncommercial License 1.0.0](https://polyformproject.org/licenses/noncommercial/1.0.0)

允许个人学习、研究、试验、业余项目与非商业组织使用，不允许商业用途。
