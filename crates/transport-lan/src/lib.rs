//! 内网传输后端。
//!
//! # 设计取向
//!
//! 局域网丢包率接近零，RTT 低于 1ms。这种环境下，公网那套保护机制全是负担：
//!
//! - **不做拥塞控制**：带宽是免费的，发满就行
//! - **不做 pacing**：一帧应该在 0.5ms 内整个发出去，而不是摊平到 16ms 里
//! - **不做抖动缓冲**：到了就解码，等了只是增加延迟
//!
//! 但加密还是要做。局域网也可能是公共 Wi-Fi。用 Noise Protocol（`snow` crate），
//! X25519 + ChaCha20-Poly1305，每包 AEAD 开销在微秒级，不引入任何延迟结构。
//!
//! # 包头布局（32 字节，小端）
//!
//! ```text
//! 偏移  长度  字段
//! 0     4     magic  "CVP1"
//! 4     8     frame_id
//! 12    4     seq        帧内分片序号
//! 16    2     slice_id   保留，当前等于 seq
//! 18    2     slice_count
//! 20    4     flags
//! 24    8     pts_us
//! 32    ...   载荷，最大 MTU - 32 = 1168 字节
//! ```

use connecter_core::transport::EncodedFrame;

/// 单片最大长度，含包头。取 1200 是为了在常见 MTU 1500 下留足余量，
/// 同时避开部分链路上的 PMTU 黑洞。
pub const MTU: usize = 1200;

/// 包头长度。
pub const HEADER_LEN: usize = 32;

/// 单片最大载荷。
pub const MAX_PAYLOAD: usize = MTU - HEADER_LEN;

pub const MAGIC: [u8; 4] = *b"CVP1";

/// 分包标志位。
pub mod flags {
    /// 关键帧。
    pub const KEYFRAME: u32 = 1 << 0;
    /// 本帧的最后一个分片。
    pub const LAST_SLICE: u32 = 1 << 1;
    /// 独立的控制包，不属于任何帧。
    pub const CONTROL: u32 = 1 << 2;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub frame_id: u64,
    pub seq: u32,
    pub slice_id: u16,
    pub slice_count: u16,
    pub flags: u32,
    pub pts_us: u64,
}

impl Header {
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut buf = [0u8; HEADER_LEN];
        buf[0..4].copy_from_slice(&MAGIC);
        buf[4..12].copy_from_slice(&self.frame_id.to_le_bytes());
        buf[12..16].copy_from_slice(&self.seq.to_le_bytes());
        buf[16..18].copy_from_slice(&self.slice_id.to_le_bytes());
        buf[18..20].copy_from_slice(&self.slice_count.to_le_bytes());
        buf[20..24].copy_from_slice(&self.flags.to_le_bytes());
        buf[24..32].copy_from_slice(&self.pts_us.to_le_bytes());
        buf
    }

    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < HEADER_LEN || buf[0..4] != MAGIC {
            return None;
        }
        Some(Self {
            frame_id: u64::from_le_bytes(buf[4..12].try_into().ok()?),
            seq: u32::from_le_bytes(buf[12..16].try_into().ok()?),
            slice_id: u16::from_le_bytes(buf[16..18].try_into().ok()?),
            slice_count: u16::from_le_bytes(buf[18..20].try_into().ok()?),
            flags: u32::from_le_bytes(buf[20..24].try_into().ok()?),
            pts_us: u64::from_le_bytes(buf[24..32].try_into().ok()?),
        })
    }

    pub fn is_keyframe(&self) -> bool {
        self.flags & flags::KEYFRAME != 0
    }
}

/// 把一个编码帧切成若干分片。
///
/// TODO(W1): 实现。返回每个分片的完整字节（含包头）。
pub fn fragment(_frame: &EncodedFrame) -> Vec<Vec<u8>> {
    unimplemented!("W1")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 包头往返一致() {
        let h = Header {
            frame_id: 1234567890123,
            seq: 7,
            slice_id: 7,
            slice_count: 12,
            flags: flags::KEYFRAME,
            pts_us: 9_999_999,
        };
        assert_eq!(Header::decode(&h.encode()), Some(h));
        assert!(h.is_keyframe());
    }

    #[test]
    fn 拒绝错误魔数() {
        let mut buf = [0u8; HEADER_LEN];
        buf[0..4].copy_from_slice(b"XXXX");
        assert_eq!(Header::decode(&buf), None);
    }
}
