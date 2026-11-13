//! 编解码抽象。
//!
//! 只定义「要什么」，不定义「怎么做」。真正的编码器在 host-win，
//! 解码器在客户端壳里，因为两者都要绑定平台的原生 API。

/// 支持的编码格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    H265,
}

/// 画质档位。
///
/// 内网模式可以给到 `NearLossless`，公网模式最高只到 `Balanced`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QualityProfile {
    /// 低带宽，动态内容优先。
    LowLatency,
    /// 通用。
    Balanced,
    /// 文字清晰优先，高码率。
    Text,
    /// 内网专用，近乎无损。
    NearLossless,
}

/// 编码器期望的输出参数。
#[derive(Debug, Clone, Copy)]
pub struct EncodeConfig {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
    /// 是否强制每个关键帧间隔。0 表示无限 GOP（靠请求触发）。
    pub keyframe_interval: u32,
}
