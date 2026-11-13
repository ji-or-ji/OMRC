//! 会话状态机。
//!
//! 这里最重要的设计目标是**网络迁移**：笔记本从家里 Wi-Fi 切到手机热点，
//! 或者手机从 Wi-Fi 切到 4G，会话不该断，只换底下那层传输。
//!
//! 所以会话持有的是「逻辑状态」（配对身份、编码参数、打开的文件句柄、
//! 终端会话），而不是 socket。

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// 未连接。
    Idle,
    /// 正在做密钥交换与能力协商。
    Handshaking,
    /// 已连接，正在传输。
    Streaming,
    /// 传输层断了，正在尝试重建。逻辑状态保留。
    Migrating,
    /// 已结束。
    Closed,
}

/// 一台设备的身份。
///
/// 设备 ID 就是公钥指纹的短哈希。首次配对靠二维码交换公钥，
/// 之后免密互认。
#[derive(Debug, Clone)]
pub struct DeviceIdentity {
    /// 公钥指纹的短哈希，用作设备 ID。
    pub id: String,
    /// 设备显示名。
    pub name: String,
}
