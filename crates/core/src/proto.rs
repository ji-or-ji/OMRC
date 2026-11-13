//! 线格式定义。
//!
//! 视频帧走传输层自己的分片（见 `omrc-transport-lan`），这里定义的是
//! **控制消息**：光标形状与位置、关键帧请求、心跳之类的小东西。
//!
//! 序列化用 `postcard`：体积小、无自描述开销、解码不需要额外分配。
//! 两端是同一份代码，不存在跨语言兼容问题。

use serde::{Deserialize, Serialize};

use crate::PROTOCOL_VERSION;

/// 握手时的魔数，用来尽早排掉连错端口的情况。
pub const MAGIC: [u8; 4] = *b"CNNR";

/// 握手报文头。
#[repr(C)]
pub struct Hello {
    pub magic: [u8; 4],
    pub version: u16,
    /// 对端支持的能力位图。
    pub capabilities: u32,
}

impl Hello {
    pub fn new(capabilities: u32) -> Self {
        Self {
            magic: MAGIC,
            version: PROTOCOL_VERSION,
            capabilities,
        }
    }
}

/// 能力位。
pub mod caps {
    /// 支持内网裸 UDP 传输。
    pub const LAN_UDP: u32 = 1 << 0;
    /// 支持公网 WebRTC 传输。
    pub const WAN_WEBRTC: u32 = 1 << 1;
    /// 支持虚拟显示器。
    pub const VIRTUAL_DISPLAY: u32 = 1 << 2;
    /// 支持数位板输入。
    pub const PEN_INPUT: u32 = 1 << 3;
    /// 支持终端。
    pub const TERMINAL: u32 = 1 << 4;
    /// 支持文件传输。
    pub const FILE_TRANSFER: u32 = 1 << 5;
}

/// 光标形状的像素格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CursorFormat {
    /// 32 位 BGRA，`a` 为直通 alpha；全 0 alpha 时按不透明处理。
    Bgra,
    /// 单色掩码：先 AND 后 XOR，1bpp，行按 4 字节对齐。
    Mono,
}

/// 光标位图。只在形状变化时发一次，不跟帧走。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorShape {
    pub format: CursorFormat,
    pub width: u16,
    pub height: u16,
    /// 热点相对位图左上角的位置。
    pub hot_x: u16,
    pub hot_y: u16,
    pub pixels: Vec<u8>,
}

/// 光标在屏幕上的位置。每帧随视频一起更新，体积只有十几字节。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorPos {
    pub x: i32,
    pub y: i32,
    pub visible: bool,
}

/// 走控制通道的消息。
///
/// 变体顺序会进线格式，改动必须两端一起改，并且提升 [`PROTOCOL_VERSION`]。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlMessage {
    /// 光标位图变了。
    CursorShape(CursorShape),
    /// 光标位置。每帧一条。
    CursorPos(CursorPos),
    /// 收端请求立刻出一个 IDR。
    RequestKeyframe,
    /// 回执，用于测 RTT。
    Ping { nonce: u64, sent_us: u64 },
    Pong { nonce: u64, sent_us: u64 },
}

impl ControlMessage {
    pub fn encode(&self) -> Vec<u8> {
        postcard::to_allocvec(self).expect("控制消息序列化不会失败")
    }

    pub fn decode(buf: &[u8]) -> Option<Self> {
        postcard::from_bytes(buf).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 控制消息往返一致() {
        let msgs = [
            ControlMessage::CursorPos(CursorPos {
                x: -12,
                y: 900,
                visible: true,
            }),
            ControlMessage::RequestKeyframe,
            ControlMessage::CursorShape(CursorShape {
                format: CursorFormat::Bgra,
                width: 32,
                height: 32,
                hot_x: 1,
                hot_y: 2,
                pixels: vec![0xAB; 32 * 32 * 4],
            }),
        ];
        for m in msgs {
            let bytes = m.encode();
            assert_eq!(ControlMessage::decode(&bytes), Some(m));
        }
    }

    #[test]
    fn 坏数据拒绝解码() {
        assert_eq!(ControlMessage::decode(&[0xFF, 0xFF, 0xFF]), None);
        assert_eq!(ControlMessage::decode(&[]), None);
    }
}
