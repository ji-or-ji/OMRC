//! Windows 主机端。
//!
//! W1a 的目标只有一条：**抓屏 → 编码 → UDP → 解码上屏**。
//! 不做输入、不做 UI、不做虚拟屏、不做动效。
//!
//! ```text
//! omrc-host list                        列出适配器与显示器
//! omrc-host probe                       连续抓 60 帧，量抓帧开销
//! omrc-host play --peer 127.0.0.1:9100  跑完整链路，往对端推流
//! ```

mod capture;
mod d3d;
mod encode;
mod mft;
mod overlay;
mod video_proc;

use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use capture::Acquire;
use omrc_core::proto::ControlMessage;
use omrc_core::video::{Codec, EncodeConfig};
use omrc_transport_lan::{fragment, fragment_control};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::Media::MediaFoundation::{MFStartup, MFShutdown, MF_VERSION, MFSTARTUP_FULL};

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("help");

    match cmd {
        "list" => list_outputs(),
        "mft" => {
            let _platform = Platform::init()?;
            mft::dump()
        }
        "probe" => probe(),
        "play" => play(&args[1..]),
        _ => {
            println!("omrc-host <list|mft|probe|play>");
            println!("  list                        列出显示器");
            println!("  mft                         列出 H.264 编码器 MFT");
            println!("  probe [--output N]          抓 60 帧，量开销");
            println!("  play --peer IP:PORT [...]   跑完整链路");
            println!("        --output N              目标显示器（默认 0）");
            println!("        --fps N                 帧率上限（默认 60）");
            println!("        --bitrate N             码率 bps（默认 20000000）");
            println!("        --encoder auto|hw|sw    编码器选择（默认 auto）");
            println!("        --seconds N             跑 N 秒后退出（默认一直跑）");
            Ok(())
        }
    }
}

/// COM + Media Foundation 的一次性初始化。
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

fn param<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(|s| s.as_str())
}

fn param_parse<T: std::str::FromStr>(args: &[String], name: &str, default: T) -> T {
    param(args, name)
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn list_outputs() -> Result<()> {
    for (i, o) in d3d::enum_outputs()?.iter().enumerate() {
        println!(
            "[{i}] 适配器 {} 输出 {}  {}",
            o.adapter_index,
            o.output_index,
            o.label()
        );
    }
    Ok(())
}

fn probe() -> Result<()> {
    let _platform = Platform::init()?;
    let args: Vec<String> = std::env::args().skip(2).collect();
    let index = param_parse(&args, "--output", 0usize);

    let outputs = d3d::enum_outputs()?;
    let info = outputs
        .get(index)
        .with_context(|| format!("没有第 {index} 个输出"))?;
    tracing::info!("探测 {}（{}x{}）", info.label(), info.width, info.height);

    let (device, context) = d3d::create_device(&info.adapter)?;
    let mut dupl = capture::Duplicator::new(device, context, info)?;

    let mut nv12 = Vec::new();
    let mut frames = 0;
    let mut timeouts = 0;
    let start = Instant::now();
    let mut copy_us = 0u128;
    let mut convert_us = 0u128;

    while start.elapsed() < Duration::from_secs(3) {
        let t0 = Instant::now();
        match dupl.acquire(16)? {
            Acquire::Frame(_) => {
                frames += 1;
                let t1 = Instant::now();
                dupl.read_nv12(&mut nv12)?;
                let t2 = Instant::now();
                copy_us += (t1 - t0).as_micros();
                convert_us += (t2 - t1).as_micros();
            }
            Acquire::Timeout => timeouts += 1,
            Acquire::AccessLost => {
                tracing::warn!("复制链路断开");
                break;
            }
        }
    }

    let secs = start.elapsed().as_secs_f64();
    println!("3 秒内抓到 {frames} 帧，超时 {timeouts} 次，{:.1} fps", frames as f64 / secs);
    if frames > 0 {
        println!(
            "平均抓帧+拷贝 {:.2} ms，NV12 转换 {:.2} ms，NV12 大小 {} 字节",
            copy_us as f64 / 1000.0 / frames as f64,
            convert_us as f64 / 1000.0 / frames as f64,
            nv12.len()
        );
    }
    Ok(())
}

fn play(args: &[String]) -> Result<()> {
    let peer: SocketAddr = param(args, "--peer")
        .context("缺少 --peer IP:PORT")?
        .parse()
        .context("--peer 不是合法地址")?;
    let output_index = param_parse(args, "--output", 0usize);
    let fps = param_parse(args, "--fps", 60u32);
    let bitrate = param_parse(args, "--bitrate", 20_000_000u32);
    let seconds = param_parse(args, "--seconds", 0u64);
    let encoder_pref = match param(args, "--encoder").unwrap_or("auto") {
        "hw" | "hardware" => encode::EncoderPreference::Hardware,
        "sw" | "software" => encode::EncoderPreference::Software,
        _ => encode::EncoderPreference::Auto,
    };

    let _platform = Platform::init()?;

    let outputs = d3d::enum_outputs()?;
    let info = outputs
        .get(output_index)
        .with_context(|| format!("没有第 {output_index} 个输出"))?;
    tracing::info!("抓取 {}（{}x{}）", info.label(), info.width, info.height);

    let (device, context) = d3d::create_device(&info.adapter)?;
    let mut dupl = capture::Duplicator::new(device, context, info)?;

    let cfg = EncodeConfig {
        codec: Codec::H264,
        width: dupl.width(),
        height: dupl.height(),
        fps,
        bitrate_bps: bitrate,
        keyframe_interval: 0,
    };
    let mut encoder = encode::Encoder::new(&cfg, encoder_pref, Some(dupl.device()))?;
    println!("编码器：{:?}", encoder.mode());

    // 硬件编码器要的是显存里的 NV12 纹理；软件编码器退而求其次吃系统内存。
    let converter = if encoder.wants_texture() {
        Some(video_proc::Nv12Converter::new(
            dupl.device(),
            dupl.context(),
            dupl.target(),
            dupl.width(),
            dupl.height(),
            fps,
        )?)
    } else {
        None
    };

    let overlay_width = 340;
    let overlay_height = 56;
    let mut overlay = overlay::TimecodeOverlay::new(overlay_width, overlay_height)?;

    let socket = UdpSocket::bind("0.0.0.0:0").context("绑定本地 UDP 失败")?;
    socket.connect(peer).context("connect 到对端失败")?;
    // 视频包一帧几十个，发送缓冲开大点，别在 127.0.0.1 上自己堵自己。
    let _ = socket.set_write_timeout(Some(Duration::from_millis(2)));

    tracing::info!(%peer, "开始推流");

    let mut nv12 = Vec::new();
    let mut frame_id = 0u64;
    let mut control_id = 1_000_000u64;
    let mut stat_frames = 0u64;
    let mut stat_captured = 0u64;
    let mut stat_bytes = 0u64;
    let mut stat_encode_us = 0u128;
    let mut stat_capture_us = 0u128;
    let mut stat_encoder_lag_us = 0u128;
    let mut last_stat = Instant::now();
    let started = Instant::now();
    let frame_budget = Duration::from_micros(1_000_000 / fps.max(1) as u64);
    let mut next_frame_at = Instant::now();

    loop {
        if seconds > 0 && started.elapsed() >= Duration::from_secs(seconds) {
            break;
        }

        let t0 = Instant::now();
        match dupl.acquire(4)? {
            Acquire::Frame(pointer) => {
                // 帧率上限：桌面复制本身跟着显示器刷新，管不住；这里按预算丢。
                if t0 < next_frame_at {
                    continue;
                }
                next_frame_at = t0 + frame_budget;
                stat_captured += 1;

                let captured_us = now_us();
                let total_secs = captured_us / 1_000_000;
                let text = format!(
                    "{:02}:{:02}:{:02}.{:03}  #{}",
                    total_secs % 86400 / 3600,
                    total_secs % 3600 / 60,
                    total_secs % 60,
                    captured_us / 1000 % 1000,
                    frame_id
                );
                let pixels = overlay.render(&text);
                dupl.blit_overlay(pixels, overlay_width, overlay_height, 8, 8)?;
                let t1 = Instant::now();

                let frames = if let Some(conv) = &converter {
                    conv.convert()?;
                    encoder.encode_texture(&conv.texture, captured_us)?
                } else {
                    dupl.read_nv12(&mut nv12)?;
                    encoder.encode(&nv12, captured_us)?
                };
                let t3 = Instant::now();

                stat_capture_us += (t3 - t1).as_micros();
                stat_encode_us += (t3 - t1).as_micros();

                for f in &frames {
                    let mut sent = 0usize;
                    for pkt in fragment(f) {
                        match socket.send(&pkt) {
                            Ok(n) => sent += n,
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                            Err(e) => return Err(e).context("发送视频包失败"),
                        }
                    }
                    stat_bytes += sent as u64;
                    stat_frames += 1;
                    // 编码器从收到输入到吐出这一帧的气泡，直接反映链路里最大的延迟源。
                    let age = now_us() - f.pts_us as i64;
                    if age > 0 {
                        stat_encoder_lag_us += age as u128;
                    }
                }

                // 光标：位置每帧都发，形状变了才发。
                if let Some(shape) = pointer.shape {
                    let msg = ControlMessage::CursorShape(shape);
                    for pkt in fragment_control(&msg, control_id, captured_us as u64) {
                        let _ = socket.send(&pkt);
                    }
                    control_id += 1;
                }
                {
                    let msg = ControlMessage::CursorPos(pointer.pos);
                    for pkt in fragment_control(&msg, control_id, captured_us as u64) {
                        let _ = socket.send(&pkt);
                    }
                    control_id += 1;
                }

                frame_id += 1;
            }
            Acquire::Timeout => {}
            Acquire::AccessLost => {
                tracing::warn!("复制链路断开，尝试重建");
                // 简化处理：W1a 直接退出，让外壳重启。重建逻辑留给 W2。
                bail!("桌面复制断开（切分辨率 / 全屏独占 / 锁屏）");
            }
        }

        if last_stat.elapsed() >= Duration::from_secs(1) {
            let secs = last_stat.elapsed().as_secs_f64();
            if stat_frames > 0 {
                tracing::info!(
                    "采 {} 帧 → 编 {} 帧  {:.1} Mbps  采集+转码 {:.2}ms  编码 {:.2}ms  编码器滞后 {:.1}ms",
                    stat_captured,
                    stat_frames,
                    stat_bytes as f64 * 8.0 / secs / 1e6,
                    stat_capture_us as f64 / 1000.0 / stat_frames.max(1) as f64,
                    stat_encode_us as f64 / 1000.0 / stat_frames.max(1) as f64,
                    stat_encoder_lag_us as f64 / 1000.0 / stat_frames.max(1) as f64,
                );
            } else {
                tracing::info!("屏幕无变化");
            }
            stat_frames = 0;
            stat_captured = 0;
            stat_bytes = 0;
            stat_capture_us = 0;
            stat_encode_us = 0;
            stat_encoder_lag_us = 0;
            last_stat = Instant::now();
        }
    }

    if let Ok(rest) = encoder.flush() {
        for f in rest {
            for pkt in fragment(&f) {
                let _ = socket.send(&pkt);
            }
        }
    }
    tracing::info!("退出");
    Ok(())
}

fn now_us() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}
