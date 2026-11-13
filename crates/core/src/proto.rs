//! 线格式定义。
//!
//! 序列化选型待定（`postcard` 是首选，no_std 友好、体积小）。
//! 定下来之前这里只放常量与握手魔数。

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
