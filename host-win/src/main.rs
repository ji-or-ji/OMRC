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

use std::collections::VecDeque;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use capture::Acquire;
use omrc_core::proto::ControlMessage;
use omrc_core::video::{Codec, EncodeConfig};
use omrc_transport_lan::{Reassembler, fragment, fragment_control};
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
    let mut converter = build_converter(&dupl, &encoder, fps)?;

    let overlay_width = 340;
    let overlay_height = 56;
    let mut overlay = overlay::TimecodeOverlay::new(overlay_width, overlay_height)?;

    let socket = UdpSocket::bind("0.0.0.0:0").context("绑定本地 UDP 失败")?;
    socket.connect(peer).context("connect 到对端失败")?;
    // 视频包一帧几十个，发送缓冲开大点，别在 127.0.0.1 上自己堵自己。
    let _ = socket.set_write_timeout(Some(Duration::from_millis(2)));
    // 控制回传（NACK / 要 IDR）是非阻塞读的，不占主循环。
    socket.set_nonblocking(true).context("设置非阻塞失败")?;

    tracing::info!(%peer, "开始推流");
    spawn_watchdog();

    let mut nv12 = Vec::new();
    let mut frame_id = 0u64;
    let mut control_id = 1_000_000u64;
    // 最近几帧的分片，留着给 NACK 重传。
    let mut resend_cache: VecDeque<(u64, Vec<Vec<u8>>)> = VecDeque::new();
    let mut ctrl_reasm = Reassembler::new(4);
    let mut ctrl_buf = [0u8; 2048];
    let mut stat_resends = 0u64;
    let mut stat_nacks = 0u64;
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
    let mut paused = false;

    loop {
        if seconds > 0 && started.elapsed() >= Duration::from_secs(seconds) {
            break;
        }

        // 锁屏 / 安全桌面期间 AcquireNextFrame 会挂死，必须提前避开。
        if capture::desktop_locked() {
            if !paused {
                tracing::info!("桌面已锁定，暂停采集");
                paused = true;
            }
            std::thread::sleep(Duration::from_millis(120));
            continue;
        }
        if paused {
            paused = false;
            tracing::info!("桌面已解锁，重建采集链路");
            match rebuild_pipeline(output_index, encoder_pref, fps, bitrate) {
                Ok((d, e, c)) => {
                    dupl = d;
                    encoder = e;
                    converter = c;
                    resend_cache.clear();
                    next_frame_at = Instant::now();
                }
                Err(e) => tracing::error!("解锁后重建失败: {e:#}"),
            }
            continue;
        }

        // 先把收端发来的控制包收干净（NACK、要 IDR）。非阻塞，不影响主循环。
        // 每轮最多收 64 个：万一对面在刷控制包，主循环也得继续跑。
        for _ in 0..64 {
            let Ok(n) = socket.recv(&mut ctrl_buf) else {
                break;
            };
            let Some(frame) = ctrl_reasm.push_control(&ctrl_buf[..n]) else {
                continue;
            };
            let Some(msg) = ControlMessage::decode(&frame.data) else {
                continue;
            };
            match msg {
                ControlMessage::Nack { frame_id, slices } => {
                    stat_nacks += 1;
                    if let Some((_, pkts)) = resend_cache.iter().find(|(id, _)| *id == frame_id)
                    {
                        for s in slices {
                            if let Some(pkt) = pkts.get(s as usize) {
                                let _ = socket.send(pkt);
                                stat_resends += 1;
                            }
                        }
                    }
                }
                ControlMessage::RequestKeyframe => {
                    tracing::debug!("收端请求关键帧");
                    encoder.request_keyframe();
                }
                _ => {}
            }
        }

        let t0 = Instant::now();
        BEAT.fetch_add(1, Ordering::Relaxed);
        STAGE.store(1, Ordering::Relaxed);
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
                STAGE.store(2, Ordering::Relaxed);
                dupl.blit_overlay(pixels, overlay_width, overlay_height, 8, 8)?;
                let t1 = Instant::now();

                let frames = if let Some(conv) = converter.as_mut() {
                    STAGE.store(3, Ordering::Relaxed);
                    let tex = conv.convert()?;
                    STAGE.store(4, Ordering::Relaxed);
                    encoder.encode_texture(tex, captured_us)?
                } else {
                    dupl.read_nv12(&mut nv12)?;
                    STAGE.store(4, Ordering::Relaxed);
                    encoder.encode(&nv12, captured_us)?
                };
                let t3 = Instant::now();

                stat_capture_us += (t3 - t1).as_micros();
                stat_encode_us += (t3 - t1).as_micros();

                for f in &frames {
                    let mut sent = 0usize;
                    STAGE.store(5, Ordering::Relaxed);
                    let pkts = fragment(f);
                    for pkt in &pkts {
                        match socket.send(pkt) {
                            Ok(n) => sent += n,
                            // 非阻塞模式下满缓冲会返回 WouldBlock，丢包比卡循环强。
                            Err(e)
                                if e.kind() == std::io::ErrorKind::WouldBlock
                                    || e.kind() == std::io::ErrorKind::TimedOut =>
                            {
                                break
                            }
                            Err(e) => return Err(e).context("发送视频包失败"),
                        }
                    }
                    stat_bytes += sent as u64;
                    stat_frames += 1;

                    // 缓存最近几帧，给 NACK 用。
                    resend_cache.push_back((f.id, pkts));
                    while resend_cache.len() > 8 {
                        resend_cache.pop_front();
                    }

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
                tracing::warn!("桌面复制断开（切分辨率 / 全屏独占 / 锁屏 / UAC），重建中");
                // 设备、复制器、转换器、编码器整套重建：设备一变，
                // 挂在旧设备上的 D3D 设备管理器就作废了。
                let rebuilt = rebuild_pipeline(output_index, encoder_pref, fps, bitrate);

                match rebuilt {
                    Ok((d, e, c)) => {
                        dupl = d;
                        encoder = e;
                        converter = c;
                        // 分辨率可能变了，帧号重开一段，收端不依赖它的连续性。
                        resend_cache.clear();
                        next_frame_at = Instant::now();
                        tracing::info!(
                            "重建完成，{}x{}",
                            dupl.width(),
                            dupl.height()
                        );
                    }
                    Err(e) => {
                        tracing::error!("重建失败，再试一次: {e:#}");
                        std::thread::sleep(Duration::from_millis(500));
                    }
                }
                continue;
            }
        }

        if last_stat.elapsed() >= Duration::from_secs(1) {
            let secs = last_stat.elapsed().as_secs_f64();
            if stat_frames > 0 {
                tracing::info!(
                    "采 {} 帧 → 编 {} 帧  {:.1} Mbps  采集+转码 {:.2}ms  编码 {:.2}ms  编码器滞后 {:.1}ms  NACK {} 补发 {}",
                    stat_captured,
                    stat_frames,
                    stat_bytes as f64 * 8.0 / secs / 1e6,
                    stat_capture_us as f64 / 1000.0 / stat_frames.max(1) as f64,
                    stat_encode_us as f64 / 1000.0 / stat_frames.max(1) as f64,
                    stat_encoder_lag_us as f64 / 1000.0 / stat_frames.max(1) as f64,
                    stat_nacks,
                    stat_resends,
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
            stat_nacks = 0;
            stat_resends = 0;
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

/// 整套重建：设备 → 复制器 → 编码器 → 转换器。
///
/// 必须整套来：设备一换，挂在旧设备上的 D3D 设备管理器就作废了，
/// 只重建复制器会让编码器对着一个不存在的设备干活。
fn rebuild_pipeline(
    output_index: usize,
    encoder_pref: encode::EncoderPreference,
    fps: u32,
    bitrate: u32,
) -> Result<(capture::Duplicator, encode::Encoder, Option<video_proc::Nv12Converter>)> {
    let outputs = d3d::enum_outputs()?;
    let info = outputs
        .get(output_index)
        .with_context(|| format!("没有第 {output_index} 个输出"))?;
    let (device, context) = d3d::create_device(&info.adapter)?;
    let new_dupl = capture::Duplicator::new(device, context, info)?;
    let new_cfg = EncodeConfig {
        codec: Codec::H264,
        width: new_dupl.width(),
        height: new_dupl.height(),
        fps,
        bitrate_bps: bitrate,
        keyframe_interval: 0,
    };
    let new_encoder = encode::Encoder::new(&new_cfg, encoder_pref, Some(new_dupl.device()))?;
    let new_conv = build_converter(&new_dupl, &new_encoder, fps)?;
    Ok((new_dupl, new_encoder, new_conv))
}

/// 硬件编码器要显存里的 NV12 纹理，软件编码器吃系统内存，所以转换器是可选的。
fn build_converter(
    dupl: &capture::Duplicator,
    encoder: &encode::Encoder,
    fps: u32,
) -> Result<Option<video_proc::Nv12Converter>> {
    if !encoder.wants_texture() {
        return Ok(None);
    }
    Ok(Some(video_proc::Nv12Converter::new(
        dupl.device(),
        dupl.context(),
        dupl.target(),
        dupl.width(),
        dupl.height(),
        fps,
    )?))
}

use std::sync::atomic::{AtomicU64, Ordering};

/// 主循环心跳：给看门狗用。卡住了能知道卡在哪一步。
pub(crate) static BEAT: AtomicU64 = AtomicU64::new(0);
pub(crate) static STAGE: AtomicU64 = AtomicU64::new(0);

fn stage_name(s: u64) -> &'static str {
    match s {
        1 => "acquire",
        2 => "overlay",
        3 => "convert",
        4 => "encode",
        5 => "send",
        11 => "acquire:AcquireNextFrame",
        12 => "acquire:copy",
        13 => "acquire:pointer",
        14 => "acquire:release",
        _ => "idle",
    }
}

/// 主循环 1.5 秒没动就报一次，3 秒就自杀。
///
/// 卡死的是 DXGI 的 `AcquireNextFrame`，它在锁屏、桌面切换、显示器休眠这些
/// 场景下会嵌在内核里不返回，超时参数对它无效。进程内救不回来，
/// 只能退出让外壳重启——反正状态都在收端，重连一次只是一帧的黑。
fn spawn_watchdog() {
    std::thread::spawn(|| {
        let mut last = 0u64;
        let mut stalled = 0u32;
        loop {
            std::thread::sleep(Duration::from_millis(1500));
            let beat = BEAT.load(Ordering::Relaxed);
            if beat == last {
                stalled += 1;
                if stalled == 1 {
                    tracing::error!(
                        stage = stage_name(STAGE.load(Ordering::Relaxed)),
                        "主循环超过 1.5 秒没动"
                    );
                } else if stalled >= 2 {
                    tracing::error!("主循环卡死，退出让外壳重启");
                    std::process::exit(3);
                }
            } else {
                stalled = 0;
            }
            last = beat;
        }
    });
}

fn now_us() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}
