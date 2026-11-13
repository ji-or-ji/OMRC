//! Windows 客户端（W1a 回环验证）。
//!
//! 收到 UDP 分片 → 重组 → H.264 解码 → 贴到窗口。
//!
//! # 线程模型
//!
//! - **收包线程**：死循环 `recv_from`，把完整的帧和控制消息塞进队列。永远不等别的东西，
//!   这样内核接收缓冲里不会积压——积压就是延迟。
//! - **UI 线程**：抽消息、逐帧解码、画最新的一张。
//!
//! 解码必须逐帧按序做（H.264 的参考链不能跳），但**上屏只上最新那张**。
//! 这两件事分开，才能既不花屏又不积延迟。

mod app;
mod decode;

use std::collections::VecDeque;
use std::net::UdpSocket;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use app::App;
use decode::Decoder;
use omrc_core::proto::ControlMessage;
use omrc_transport_lan::{Header, LossAction, LossTracker, Reassembler, fragment_control};
use windows::Win32::Media::MediaFoundation::{MFStartup, MFShutdown, MFSTARTUP_FULL, MF_VERSION};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};

struct Platform;

impl Platform {
    fn init() -> Result<Self> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED)
                .ok()
                .context("CoInitializeEx 失败")?;
            MFStartup(MF_VERSION, MFSTARTUP_FULL).context("MFStartup 失败")?;
        }
        Ok(Self)
    }
}

impl Drop for Platform {
    fn drop(&mut self) {
        unsafe {
            let _ = MFShutdown();
        }
    }
}

/// 一帧待解码的视频。
struct PendingFrame {
    data: Vec<u8>,
    pts_us: u64,
}

enum Item {
    Frame(PendingFrame),
    Control(ControlMessage),
}

#[derive(Default)]
struct Shared {
    queue: Mutex<VecDeque<Item>>,
}

/// 队列上限。超过说明 UI 线程跟不上，丢最旧的帧（会花屏，但至少不积延迟）。
const MAX_QUEUED_FRAMES: usize = 120;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();

    // 开发用：把一个抓下来的码流文件离线喂给解码器，量输入/输出比与耗时。
    if let Some(idx) = args.iter().position(|a| a == "--decode-dump") {
        let path = args.get(idx + 1).context("--decode-dump 需要文件路径")?;
        let _platform = Platform::init()?;
        return offline_decode(path);
    }

    let listen = args
        .iter()
        .position(|a| a == "--listen")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| "0.0.0.0:9100".to_string());

    let _platform = Platform::init()?;

    let socket = UdpSocket::bind(&listen).with_context(|| format!("绑定 {listen} 失败"))?;
    enlarge_recv_buffer(&socket, 4 * 1024 * 1024);
    tracing::info!("监听 {listen}");

    let shared = Arc::new(Shared::default());
    let dump_path = std::env::var_os("OMRC_DUMP");
    let loss = args
        .iter()
        .position(|a| a == "--loss")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(0.0);

    {
        let shared = Arc::clone(&shared);
        let socket = socket.try_clone().context("复制 socket 失败")?;
        std::thread::Builder::new()
            .name("recv".into())
            .spawn(move || recv_loop(socket, shared, dump_path, loss))?;
    }

    let mut app = App::new();
    let (win_w, win_h) = args
        .iter()
        .position(|a| a == "--size")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| {
            let (w, h) = v.split_once('x')?;
            Some((w.parse::<i32>().ok()?, h.parse::<i32>().ok()?))
        })
        .unwrap_or((1600, 900));
    app.create_window("OMRC 客户端", win_w, win_h)?;
    let mut decoder = Decoder::new()?;

    let mut decode_us = 0u128;
    let mut decode_n = 0u64;
    let mut shown = 0u64;
    let mut drained = 0u64;
    let mut controls = 0u64;
    let mut last_report = Instant::now();
    let mut busy_us = 0u128;
    let mut wall_start = Instant::now();

    app::run_message_loop(&mut app, |app| {
        let tick_start = Instant::now();
        let items = {
            let mut q = shared.queue.lock().unwrap();
            std::mem::take(&mut *q)
        };

        let mut newest: Option<(Vec<u8>, u64, u64)> = None;
        for item in items {
            match item {
                Item::Control(msg) => {
                    controls += 1;
                    app.on_control(msg)
                }
                Item::Frame(frame) => {
                    drained += 1;
                    let wire = frame.data.len() as u64;
                    let t0 = Instant::now();
                    let decoded = decoder.decode(&frame.data, frame.pts_us);
                    decode_us += t0.elapsed().as_micros();
                    decode_n += 1;
                    match decoded {
                        Ok(Some(bgra)) => {
                            newest = Some((bgra.to_vec(), frame.pts_us, wire));
                        }
                        Ok(None) => {}
                        Err(e) => tracing::warn!("解码失败: {e:#}"),
                    }
                }
            }
        }

        if let Some((bgra, pts, wire)) = newest {
            let (w, h) = (decoder.width(), decoder.height());
            shown += 1;
            app.on_frame(&bgra, w, h, pts, wire);
        }

        if last_report.elapsed().as_secs_f64() >= 1.0 {
            let wall = wall_start.elapsed().as_micros();
            let (_i, outs, rej) = decoder.stats();
            tracing::info!(
                "收 {} 帧 / 解 {} 帧（{:.1} ms/帧）/ 显示 {} / 控制 {} / 拒收 {} / 占用 {:.0}%",
                drained,
                decode_n,
                if decode_n > 0 {
                    decode_us as f64 / 1000.0 / decode_n as f64
                } else {
                    0.0
                },
                shown,
                controls,
                rej,
                busy_us as f64 * 100.0 / wall.max(1) as f64
            );
            let _ = outs;
            controls = 0;
            decode_us = 0;
            decode_n = 0;
            shown = 0;
            drained = 0;
            busy_us = 0;
            wall_start = Instant::now();
            last_report = Instant::now();
        }
        busy_us += tick_start.elapsed().as_micros();

        // 队列空的时候让出 CPU，不要空转。
        std::thread::sleep(Duration::from_millis(1));
    })?;

    app::destroy_window(app.hwnd());
    Ok(())
}

/// 收包线程：只做「收 → 重组 → 入队」，不碰 UI，也不做解码。
///
/// 丢包补救也在这里做：补片请求（NACK）和要关键帧都是控制包，发回去就行。
fn recv_loop(
    socket: UdpSocket,
    shared: Arc<Shared>,
    dump_path: Option<std::ffi::OsString>,
    loss: f64,
) {
    let mut video = Reassembler::new(16);
    let mut control = Reassembler::new(8);
    let mut tracker = LossTracker::default();
    let mut buf = vec![0u8; 2048];
    // 第一次收到包之前用它没有意义，所以不初始化。
    let mut host_addr: std::net::SocketAddr;
    let mut ctrl_id = 1_000_000u64;
    let mut rng: u64 = 0x2545_F491_4F6C_DD1D;
    let mut dump = dump_path.map(|p| {
        std::fs::File::create(std::path::PathBuf::from(p)).expect("无法创建 dump 文件")
    });
    use std::io::Write as _;

    loop {
        let (n, from) = match socket.recv_from(&mut buf) {
            Ok(v) => v,
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(e) => {
                tracing::warn!("收包失败: {e}");
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
        };
        host_addr = from;

        // 模拟丢包（调试 NACK 用）：按概率直接扔。
        if loss > 0.0 {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            if (rng % 10_000) as f64 / 10_000.0 < loss {
                continue;
            }
        }

        let Some(header) = Header::decode(&buf[..n]) else {
            continue;
        };

        if header.is_control() {
            if let Some(frame) = control.push_control(&buf[..n]) {
                if let Some(msg) = ControlMessage::decode(&frame.data) {
                    push(&shared, Item::Control(msg));
                }
            }
            continue;
        }

        tracker.observe(header.frame_id);
        if tracker.is_abandoned(header.frame_id) {
            continue;
        }
        if let Some(frame) = video.push(&buf[..n]) {
            tracker.observe_completed(frame.frame_id);
            if frame.keyframe {
                tracker.on_keyframe();
            }
            if let Some(f) = dump.as_mut() {
                let _ = f.write_all(&(frame.data.len() as u32).to_le_bytes());
                let _ = f.write_all(&frame.data);
            }
            push(
                &shared,
                Item::Frame(PendingFrame {
                    data: frame.data,
                    pts_us: frame.pts_us,
                }),
            );
        }

        // 检查有没有补不回来的帧，该发 NACK 发 NACK，该要 IDR 要 IDR。
        for action in tracker.poll(&mut video) {
            let msg = match action {
                LossAction::Nack { frame_id, slices } => {
                    tracing::debug!(frame_id, count = slices.len(), "请求重传");
                    ControlMessage::Nack { frame_id, slices }
                }
                LossAction::Drop { frame_id } => {
                    tracing::debug!(frame_id, "放弃一帧");
                    continue;
                }
                LossAction::RequestKeyframe => {
                    tracing::info!("丢包救不回来，向主机要关键帧");
                    ControlMessage::RequestKeyframe
                }
            };
            if let Some(addr) = Some(host_addr) {
                for pkt in fragment_control(&msg, ctrl_id, 0) {
                    let _ = socket.send_to(&pkt, addr);
                }
                ctrl_id += 1;
            }
        }
    }
}

fn push(shared: &Shared, item: Item) {
    let mut q = shared.queue.lock().unwrap();
    if q.len() >= MAX_QUEUED_FRAMES {
        q.pop_front();
    }
    q.push_back(item);
}

/// 离线解码一个 `u32 长度 + 数据` 帧序列文件。
fn offline_decode(path: &str) -> Result<()> {
    let data = std::fs::read(path).with_context(|| format!("读不到 {path}"))?;
    let mut decoder = Decoder::new()?;

    for pass in 1..=2 {
        let mut off = 0usize;
        let mut inputs = 0u64;
        let mut total_us = 0u128;
        let base_out = decoder.stats().1;
        let start = Instant::now();

        while off + 4 <= data.len() {
            let len =
                u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]) as usize;
            off += 4;
            if off + len > data.len() {
                break;
            }
            let frame = &data[off..off + len];
            off += len;
            inputs += 1;
            let t0 = Instant::now();
            let _ = decoder.decode(frame, 0)?;
            total_us += t0.elapsed().as_micros();
        }

        let (i, o, r) = decoder.stats();
        println!(
            "第 {pass} 遍：{inputs} 帧输入，{} 帧出图，用时 {:.2}s，平均 {:.2} ms/帧，累计输入 {} 输出 {} 拒收 {}",
            o - base_out,
            start.elapsed().as_secs_f64(),
            total_us as f64 / 1000.0 / inputs.max(1) as f64,
            i,
            o,
            r
        );
    }
    Ok(())
}

/// Windows 默认的 UDP 接收缓冲只有 64KB，20Mbps 的突发帧很容易冲爆。
fn enlarge_recv_buffer(socket: &UdpSocket, bytes: usize) {
    use std::os::windows::io::AsRawSocket;
    use windows::Win32::Networking::WinSock::{SOL_SOCKET, SO_RCVBUF, SOCKET, setsockopt};

    let size = bytes as i32;
    let raw = SOCKET(socket.as_raw_socket() as usize);
    let rc = unsafe { setsockopt(raw, SOL_SOCKET, SO_RCVBUF, Some(&size.to_ne_bytes())) };
    if rc != 0 {
        tracing::warn!("设置 SO_RCVBUF 失败，用系统默认值");
    }
}
