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

use std::collections::{HashMap, VecDeque};

use omrc_core::proto::ControlMessage;
use omrc_core::transport::EncodedFrame;

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

    pub fn is_last_slice(&self) -> bool {
        self.flags & flags::LAST_SLICE != 0
    }

    /// 控制包要送去控制侧的重组器，不能混进视频帧。
    pub fn is_control(&self) -> bool {
        self.flags & flags::CONTROL != 0
    }
}

/// 重组完成的一帧。
#[derive(Debug, Clone)]
pub struct CompletedFrame {
    pub frame_id: u64,
    pub pts_us: u64,
    pub keyframe: bool,
    pub data: Vec<u8>,
}

/// 一个编码帧需要切成多少片。
pub fn slice_count(len: usize) -> usize {
    len.div_ceil(MAX_PAYLOAD)
}

/// 把一个编码帧切成若干分片，每片自带包头。
///
/// 返回的每片都是可以直接塞进 socket 的完整字节。
pub fn fragment(frame: &EncodedFrame) -> Vec<Vec<u8>> {
    let mut extra = 0u32;
    if frame.keyframe {
        extra |= flags::KEYFRAME;
    }
    fragment_with(frame.id, frame.pts_us, &frame.data, extra)
}

/// 把一条控制消息切成若干分片。
///
/// 控制消息和视频帧共用同一套分片与重组代码，只靠包头里的 `CONTROL` 位区分。
/// 这样光标位图（最大几十 KB）也不用为它单开一条通道。
pub fn fragment_control(msg: &ControlMessage, id: u64, pts_us: u64) -> Vec<Vec<u8>> {
    fragment_with(id, pts_us, &msg.encode(), flags::CONTROL)
}

fn fragment_with(frame_id: u64, pts_us: u64, data: &[u8], extra_flags: u32) -> Vec<Vec<u8>> {
    if data.is_empty() {
        return Vec::new();
    }

    let total = slice_count(data.len());
    assert!(total <= u16::MAX as usize, "单帧过大，分片数超出 u16");

    let mut out = Vec::with_capacity(total);
    for (i, chunk) in data.chunks(MAX_PAYLOAD).enumerate() {
        let mut f = extra_flags;
        if i + 1 == total {
            f |= flags::LAST_SLICE;
        }

        let h = Header {
            frame_id,
            seq: i as u32,
            slice_id: i as u16,
            slice_count: total as u16,
            flags: f,
            pts_us,
        };

        let mut pkt = Vec::with_capacity(HEADER_LEN + chunk.len());
        pkt.extend_from_slice(&h.encode());
        pkt.extend_from_slice(chunk);
        out.push(pkt);
    }
    out
}

#[derive(Debug)]
struct FrameBuf {
    slice_count: u16,
    keyframe: bool,
    pts_us: u64,
    parts: Vec<Option<Vec<u8>>>,
    got: u16,
}

/// 收端重组器。
///
/// 容忍乱序和重复，不骗人：缺片就是不完整，不会把半帧吐出去。
/// 上层拿到 `None` 时可以用 `missing()` 发 NACK。
#[derive(Debug)]
pub struct Reassembler {
    frames: HashMap<u64, FrameBuf>,
    order: VecDeque<u64>,
    max_inflight: usize,
}

impl Default for Reassembler {
    fn default() -> Self {
        Self::new(8)
    }
}

impl Reassembler {
    /// `max_inflight` 是同时等待重组的帧数上限。超了就丢最旧的那个，
    /// 防止网络抖动时内存无限增长。
    pub fn new(max_inflight: usize) -> Self {
        Self {
            frames: HashMap::new(),
            order: VecDeque::new(),
            max_inflight: max_inflight.max(1),
        }
    }

    /// 喂一个视频包（含包头）。凑齐一帧就返回它。
    ///
    /// 控制包会被忽略，控制包请交给另一个重组器用 [`Reassembler::push_control`]。
    pub fn push(&mut self, pkt: &[u8]) -> Option<CompletedFrame> {
        self.push_inner(pkt, false)
    }

    /// 喂一个控制包。与视频重组器用同一套逻辑，只是取消了对 `CONTROL` 位的过滤。
    pub fn push_control(&mut self, pkt: &[u8]) -> Option<CompletedFrame> {
        self.push_inner(pkt, true)
    }

    fn push_inner(&mut self, pkt: &[u8], allow_control: bool) -> Option<CompletedFrame> {
        let h = Header::decode(pkt)?;
        if h.is_control() != allow_control {
            return None;
        }
        if h.slice_count == 0 || h.slice_id >= h.slice_count {
            return None;
        }

        if !self.frames.contains_key(&h.frame_id) {
            self.evict_if_needed();
            self.frames.insert(
                h.frame_id,
                FrameBuf {
                    slice_count: h.slice_count,
                    keyframe: h.is_keyframe(),
                    pts_us: h.pts_us,
                    parts: (0..h.slice_count).map(|_| None).collect(),
                    got: 0,
                },
            );
            self.order.push_back(h.frame_id);
        }

        let payload = &pkt[HEADER_LEN..];
        let buf = self.frames.get_mut(&h.frame_id)?;

        // 分片数不一致说明对端换了帧又复用了 frame_id，直接丢弃这一片。
        if buf.slice_count != h.slice_count {
            return None;
        }

        let slot = &mut buf.parts[h.slice_id as usize];
        if slot.is_some() {
            return None; // 重复片，忽略
        }
        *slot = Some(payload.to_vec());
        buf.got += 1;

        if buf.got < buf.slice_count {
            return None;
        }

        // 凑齐了，按序拼起来
        let buf = self.frames.remove(&h.frame_id)?;
        self.order.retain(|id| *id != h.frame_id);

        let mut data = Vec::with_capacity(buf.slice_count as usize * MAX_PAYLOAD);
        for part in buf.parts {
            data.extend_from_slice(&part?);
        }

        Some(CompletedFrame {
            frame_id: h.frame_id,
            pts_us: buf.pts_us,
            keyframe: buf.keyframe,
            data,
        })
    }

    /// 某帧还缺哪些片。用来发 NACK。
    pub fn missing(&self, frame_id: u64) -> Vec<u16> {
        match self.frames.get(&frame_id) {
            None => Vec::new(),
            Some(buf) => buf
                .parts
                .iter()
                .enumerate()
                .filter(|(_, p)| p.is_none())
                .map(|(i, _)| i as u16)
                .collect(),
        }
    }

    /// 主动放弃一帧。收到更新的关键帧后调用，省得为旧帧白等。
    pub fn drop_frame(&mut self, frame_id: u64) {
        self.frames.remove(&frame_id);
        self.order.retain(|id| *id != frame_id);
    }

    /// 丢弃比 `frame_id` 更旧的所有半成品。
    pub fn drop_before(&mut self, frame_id: u64) {
        let stale: Vec<u64> = self
            .frames
            .keys()
            .copied()
            .filter(|id| *id < frame_id)
            .collect();
        for id in stale {
            self.drop_frame(id);
        }
    }

    pub fn inflight(&self) -> usize {
        self.frames.len()
    }

    fn evict_if_needed(&mut self) {
        while self.frames.len() >= self.max_inflight {
            match self.order.pop_front() {
                Some(old) => {
                    self.frames.remove(&old);
                }
                None => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use omrc_core::proto::{CursorPos, CursorShape};

    fn frame(id: u64, len: usize, keyframe: bool) -> EncodedFrame {
        EncodedFrame {
            id,
            pts_us: id * 16_666,
            keyframe,
            data: (0..len).map(|i| (i % 251) as u8).collect(),
        }
    }

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

    #[test]
    fn 分片数量按载荷上限计算() {
        assert_eq!(slice_count(0), 0);
        assert_eq!(slice_count(1), 1);
        assert_eq!(slice_count(MAX_PAYLOAD), 1);
        assert_eq!(slice_count(MAX_PAYLOAD + 1), 2);
        assert_eq!(slice_count(MAX_PAYLOAD * 10), 10);
    }

    #[test]
    fn 分片头信息正确() {
        let f = frame(42, MAX_PAYLOAD * 3 + 100, true);
        let pkts = fragment(&f);
        assert_eq!(pkts.len(), 4);

        for (i, pkt) in pkts.iter().enumerate() {
            let h = Header::decode(pkt).expect("包头应可解析");
            assert_eq!(h.frame_id, 42);
            assert_eq!(h.slice_id as usize, i);
            assert_eq!(h.slice_count, 4);
            assert_eq!(h.seq as usize, i);
            assert!(h.is_keyframe());
            assert_eq!(h.is_last_slice(), i == 3);
            assert!(pkt.len() <= MTU);
        }
    }

    #[test]
    fn 空帧不产生分片() {
        assert!(fragment(&frame(1, 0, false)).is_empty());
    }

    #[test]
    fn 乱序重组成功() {
        let f = frame(7, MAX_PAYLOAD * 5, false);
        let mut pkts = fragment(&f);
        pkts.reverse();

        let mut r = Reassembler::default();
        let mut done = None;
        for p in &pkts {
            if let Some(x) = r.push(p) {
                done = Some(x);
            }
        }
        let done = done.expect("应能重组");
        assert_eq!(done.frame_id, 7);
        assert_eq!(done.data, f.data);
        assert!(!done.keyframe);
    }

    #[test]
    fn 缺片时不吐半帧并能报出缺失() {
        let f = frame(9, MAX_PAYLOAD * 4, true);
        let pkts = fragment(&f);

        let mut r = Reassembler::default();
        for (i, p) in pkts.iter().enumerate() {
            if i == 2 {
                continue; // 故意丢一片
            }
            assert!(r.push(p).is_none(), "不完整时不该返回帧");
        }
        assert_eq!(r.missing(9), vec![2]);
        assert_eq!(r.inflight(), 1);
    }

    #[test]
    fn 重复片被忽略且不破坏重组() {
        let f = frame(11, MAX_PAYLOAD * 3, false);
        let pkts = fragment(&f);

        let mut r = Reassembler::default();
        assert!(r.push(&pkts[0]).is_none());
        assert!(r.push(&pkts[0]).is_none()); // 重复
        assert!(r.push(&pkts[1]).is_none());
        let done = r.push(&pkts[2]).expect("应能重组");
        assert_eq!(done.data, f.data);
    }

    #[test]
    fn 在途帧数有上限() {
        let mut r = Reassembler::new(2);
        // 三个都只发第一片，永远凑不齐
        for id in 1..=3u64 {
            let f = frame(id, MAX_PAYLOAD * 2, false);
            let pkts = fragment(&f);
            r.push(&pkts[0]);
        }
        assert_eq!(r.inflight(), 2, "超限时应丢掉最旧的");
        assert!(r.missing(1).is_empty(), "最旧的帧应已被丢弃");
    }

    #[test]
    fn 丢旧帧之后新帧照常重组() {
        let old = frame(1, MAX_PAYLOAD * 2, false);
        let new = frame(2, MAX_PAYLOAD * 2, true);

        let mut r = Reassembler::default();
        r.push(&fragment(&old)[0]);
        r.drop_before(2);
        assert_eq!(r.inflight(), 0);

        let mut done = None;
        for p in &fragment(&new) {
            done = r.push(p).or(done);
        }
        assert_eq!(done.expect("新帧应能重组").frame_id, 2);
    }

    #[test]
    fn 控制包跨分片重组且不进视频重组器() {
        let msg = ControlMessage::CursorShape(CursorShape {
            format: omrc_core::proto::CursorFormat::Bgra,
            width: 64,
            height: 64,
            hot_x: 0,
            hot_y: 0,
            pixels: (0..64 * 64 * 4).map(|i| i as u8).collect(),
        });
        let pkts = fragment_control(&msg, 9001, 123);
        assert!(pkts.len() > 1, "光标位图应该需要多个分片");

        let mut video = Reassembler::default();
        let mut control = Reassembler::default();
        let mut got = None;
        for p in pkts.iter().rev() {
            assert!(video.push(p).is_none(), "视频重组器不该吃控制包");
            if let Some(x) = control.push_control(p) {
                got = Some(x);
            }
        }

        let done = got.expect("控制包应能重组");
        assert_eq!(done.frame_id, 9001);
        assert_eq!(ControlMessage::decode(&done.data), Some(msg));
        assert_eq!(video.inflight(), 0);
    }

    #[test]
    fn 视频包不会被控制重组器吃掉() {
        let f = frame(3, MAX_PAYLOAD * 2, true);
        let mut control = Reassembler::default();
        for p in fragment(&f) {
            assert!(control.push_control(&p).is_none());
        }
        assert_eq!(control.inflight(), 0);
    }
}
