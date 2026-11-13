//! 传输层抽象。
//!
//! # 为什么抽象切在「帧」而不是「包」
//!
//! 内网模式自己切分片，包头里有 `seq / frame_id / slice_id`。
//! 公网模式交给 WebRTC 做 RTP 打包，包语义完全不同。
//!
//! 如果在包这一层做统一，两套实现会互相打架，最后变成一个四不像。
//! 切在「一帧编码后的数据」这一层，上面写好的编码器和会话层完全感知不到
//! 底下换了传输方式。

use std::time::Duration;

use thiserror::Error;

/// 一帧编码完成的数据，通常是 Annex-B 格式的 H.264。
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    /// 单调递增的帧号。
    pub id: u64,
    /// 采集时刻，微秒。
    pub pts_us: u64,
    /// 是否为关键帧。收端丢包后靠它恢复。
    pub keyframe: bool,
    /// 编码后的裸数据。
    pub data: Vec<u8>,
}

/// 走可靠通道的控制消息。
///
/// 具体载荷类型后面再长，这里先留出结构。
#[derive(Debug, Clone)]
pub enum Message {
    /// 输入事件（鼠标 / 键盘 / 触摸 / 笔）。
    Input(crate::input::InputEvent),
    /// 剪贴板同步。
    Clipboard(ClipboardPayload),
    /// 文件传输的流式数据块。
    FileChunk { handle: u32, offset: u64, data: Vec<u8> },
    /// 终端字节流。
    Terminal { handle: u32, data: Vec<u8> },
    /// 低频预览快照（给主界面卡片用）。
    Snapshot { display: u8, width: u16, height: u16, encoded: Vec<u8> },
    /// 能力协商与心跳。
    Control(Vec<u8>),
}

#[derive(Debug, Clone)]
pub enum ClipboardPayload {
    Text(String),
    Html(String),
    Files(Vec<String>),
}

#[derive(Debug, Clone, Default)]
pub struct TransportStats {
    /// 往返时延。
    pub rtt: Duration,
    /// 最近窗口的丢包率，0.0 ~ 1.0。
    pub loss_ratio: f32,
    /// 当前发送码率，比特每秒。
    pub send_bps: u32,
}

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("通道已关闭")]
    Closed,
    #[error("对端不可达")]
    Unreachable,
    #[error("码率超出允许范围: {0}")]
    BitrateOutOfRange(u32),
    #[error("底层错误: {0}")]
    Io(String),
}

/// 两种传输后端实现同一个 trait：内网的裸 UDP，公网的 WebRTC。
pub trait Transport: Send + Sync {
    fn send_frame(&self, frame: EncodedFrame) -> Result<(), TransportError>;
    fn send_message(&self, msg: Message) -> Result<(), TransportError>;

    /// 调整目标码率。内网模式可开到很高，公网模式受拥塞控制约束。
    fn set_bitrate(&self, bps: u32) -> Result<(), TransportError>;

    /// 请求对方立刻出一个关键帧。
    fn request_keyframe(&self) -> Result<(), TransportError>;

    fn stats(&self) -> TransportStats;
}
