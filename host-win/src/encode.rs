//! Media Foundation H.264 编码器。
//!
//! 选型理由写在 `docs/03-选型定稿.md`：一条代码通吃 N/A/I 三家。
//! 这里做两件事：
//!
//! 1. 枚举并优先选硬件编码器（`MFT_ENUM_FLAG_HARDWARE`），拿不到就退软件 MFT；
//! 2. 把输入 NV12 帧喂进去，把吐出来的 NAL 归一成 Annex-B。
//!
//! # 为什么有 sync / async 两条路
//!
//! 硬件 MFT（NVENC / AMF / QSV 经由 MF 包装）基本都是异步的：喂一帧要等
//! `METransformNeedInput`，出帧靠 `METransformHaveOutput` 事件。软件 MFT 是同步的，
//! 喂完直接 `ProcessOutput` 就能拿到。两条路必须分开写，硬凑成一条会卡死任意一边。
//!
//! # 低延迟参数
//!
//! 无 B 帧、无限 GOP、CBR、`CODECAPI_AVEncCommonLowLatency`。关键帧靠
//! `CODECAPI_AVEncVideoForceKeyFrame` 按需触发（收端丢包才要），不靠固定间隔。

use std::mem::ManuallyDrop;

use anyhow::{Context, Result, bail};
use omrc_core::transport::EncodedFrame;
use omrc_core::video::EncodeConfig;
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonLowLatency, CODECAPI_AVEncCommonMeanBitRate,
    CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVEncMPVDefaultBPictureCount,
    CODECAPI_AVEncMPVGOPSize, CODECAPI_AVEncVideoForceKeyFrame, ICodecAPI, IMFActivate,
    IMFAttributes, IMFMediaType, IMFSample, IMFTransform, MFMediaType_Video,
    MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG, MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER,
    MFT_ENUM_FLAG_SYNCMFT, MFT_MESSAGE_COMMAND_DRAIN, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_INFO, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO,
    MFVideoFormat_H264, MFVideoFormat_NV12, MFVideoInterlace_Progressive,
    MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE,
    MF_MT_MAJOR_TYPE, MF_MT_MPEG2_PROFILE, MF_MT_MPEG_SEQUENCE_HEADER, MF_MT_PIXEL_ASPECT_RATIO,
    MF_MT_SUBTYPE, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_SA_D3D11_AWARE, MF_TRANSFORM_ASYNC,
    MF_TRANSFORM_ASYNC_UNLOCK,
    MFCreateMediaType,
    MFCreateMemoryBuffer, MFCreateSample, MFTEnumEx, eAVEncCommonRateControlMode_CBR,
    eAVEncH264VProfile_High, METransformHaveOutput, METransformNeedInput,
    IMFDXGIDeviceManager, MFCreateDXGIDeviceManager, MFT_MESSAGE_SET_D3D_MANAGER,
    MFCreateDXGISurfaceBuffer,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::core::{GUID, Interface};

/// 编码器实际跑在哪条路上。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderMode {
    /// 异步 MFT，通常是硬件。
    AsyncHardware,
    /// 同步 MFT，Microsoft H.264 软件编码器。
    SyncSoftware,
}

pub struct Encoder {
    transform: IMFTransform,
    mode: EncoderMode,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
    /// SPS / PPS。有些 MFT 不在码流里带，关键帧要自己补。
    seq_header: Vec<u8>,
    /// 异步 MFT 的输入闸门。
    can_input: bool,
    /// 异步 MFT 是否对外提供输出样本。
    provides_samples: bool,
    nv12_len: usize,
    keyframe_pending: bool,
    frame_serial: u64,
    /// 挂着 D3D11 设备的管理器。硬件 MFT 靠它做零拷贝，也是异步模式能持续工作的前提。
    d3d_manager: Option<IMFDXGIDeviceManager>,
}

/// 编码器优先级。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderPreference {
    /// 只接受硬件，拿不到就报错。
    Hardware,
    /// 只用软件。
    Software,
    /// 硬件优先，不成就退软件。
    Auto,
}

impl Encoder {
    pub fn new(
        cfg: &EncodeConfig,
        preference: EncoderPreference,
        device: Option<&windows::Win32::Graphics::Direct3D11::ID3D11Device>,
    ) -> Result<Self> {
        let input_info = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_NV12,
        };
        let output_info = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_H264,
        };

        if preference != EncoderPreference::Software {
            match Self::activate_first(
                MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
                &input_info,
                &output_info,
            )
            .and_then(|opt| match opt {
                Some(a) => Self::from_activate(a, cfg, device).map(Some),
                None => Ok(None),
            }) {
                Ok(Some(enc)) => {
                    tracing::info!(mode = ?enc.mode, "硬件 H.264 编码器就绪");
                    return Ok(enc);
                }
                Ok(None) => tracing::warn!("没枚举到硬件 H.264 编码器"),
                Err(e) => {
                    if preference == EncoderPreference::Hardware {
                        return Err(e).context("硬件编码器初始化失败");
                    }
                    tracing::warn!("硬件编码器初始化失败，退软件: {e:#}");
                }
            }
        }

        let activate = Self::activate_first(
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
            &input_info,
            &output_info,
        )?
        .context("连软件 H.264 编码器都找不到")?;
        let enc = Self::from_activate(activate, cfg, None)?;
        tracing::info!(mode = ?enc.mode, "软件 H.264 编码器就绪");
        Ok(enc)
    }

    fn activate_first(
        flags: MFT_ENUM_FLAG,
        input: &MFT_REGISTER_TYPE_INFO,
        output: &MFT_REGISTER_TYPE_INFO,
    ) -> Result<Option<IMFActivate>> {
        let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        unsafe {
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_ENCODER,
                flags,
                Some(input),
                Some(output),
                &mut activates,
                &mut count,
            )
        }
        .context("MFTEnumEx 失败")?;

        if count == 0 || activates.is_null() {
            return Ok(None);
        }

        let list = unsafe { std::slice::from_raw_parts(activates, count as usize) };
        let first = list.iter().find_map(|a| a.clone());

        // MFTEnumEx 用 CoTaskMemAlloc 分配数组，元素本身是 COM 引用，clone 过了可以直接放。
        unsafe { CoTaskMemFree(Some(activates as *const _)) };
        Ok(first)
    }

    fn from_activate(
        activate: IMFActivate,
        cfg: &EncodeConfig,
        device: Option<&windows::Win32::Graphics::Direct3D11::ID3D11Device>,
    ) -> Result<Self> {
        // 先尝试在 activate 上置位，再把 MFT 取出来。
        // NVIDIA / Intel 的硬件 MFT 在 QueryInterface(IMFAttributes) 上不老实，
        // activate 则总是 IMFAttributes，这条路稳。
        unsafe {
            let _ = activate.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1);
        }
        let transform: IMFTransform =
            unsafe { activate.ActivateObject() }.context("激活编码器 MFT 失败")?;

        // 模式以能力为准：有事件源就是异步 MFT。
        //
        // 注意：硬件 MFT（NVIDIA / Intel 的代理对象）往往不实现 IMFAttributes，
        // QueryInterface 会返回 E_NOINTERFACE。属性存储要走 IMFTransform::GetAttributes，
        // 这条路才是文档上的正途。
        let attributes = unsafe { transform.GetAttributes() }
            .ok()
            .or_else(|| transform.cast::<IMFAttributes>().ok());
        let has_events = transform
            .cast::<windows::Win32::Media::MediaFoundation::IMFMediaEventGenerator>()
            .is_ok();
        let is_async = attributes
            .as_ref()
            .and_then(|a| unsafe { a.GetUINT32(&MF_TRANSFORM_ASYNC) }.ok())
            .map(|v| v != 0)
            .unwrap_or(has_events);
        if is_async {
            if let Some(a) = &attributes {
                unsafe {
                    let _ = a.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1);
                }
            }
        }
        tracing::debug!(
            attrs = attributes.is_some(),
            async_attr = ?attributes
                .as_ref()
                .and_then(|a| unsafe { a.GetUINT32(&MF_TRANSFORM_ASYNC) }.ok()),
            is_async,
            "MFT 诊断"
        );
        let mode = if is_async {
            EncoderMode::AsyncHardware
        } else {
            EncoderMode::SyncSoftware
        };

        let mut enc = Self {
            transform,
            mode,
            width: cfg.width,
            height: cfg.height,
            fps: cfg.fps.max(1),
            bitrate: cfg.bitrate_bps,
            seq_header: Vec::new(),
            can_input: true,
            provides_samples: false,
            nv12_len: (cfg.width as usize * cfg.height as usize * 3) / 2,
            keyframe_pending: false,
            frame_serial: 0,
            d3d_manager: None,
        };

        // D3D11 感知的硬件 MFT 必须先拿到设备管理器，否则处理阶段直接 E_UNEXPECTED。
        if let Some(dev) = device {
            let aware = attributes
                .as_ref()
                .and_then(|a| unsafe { a.GetUINT32(&MF_SA_D3D11_AWARE) }.ok())
                .unwrap_or(0)
                != 0;
            tracing::info!(aware, "准备给编码器挂 D3D11 设备管理器");
            if aware {
                let mut token = 0u32;
                let mut raw: Option<IMFDXGIDeviceManager> = None;
                unsafe {
                    MFCreateDXGIDeviceManager(&mut token, &mut raw)
                        .context("创建 DXGI 设备管理器失败")?;
                    let manager = raw.context("DXGI 设备管理器为空")?;
                    manager
                        .ResetDevice(dev, token)
                        .context("把 D3D11 设备交给管理器失败")?;
                    enc.transform
                        .ProcessMessage(
                            MFT_MESSAGE_SET_D3D_MANAGER,
                            windows::core::Interface::as_raw(&manager) as usize,
                        )
                        .context("向编码器设置 D3D 管理器失败")?;
                    tracing::info!("编码器已挂上 D3D11 设备管理器");
                    enc.d3d_manager = Some(manager);
                }
            }
        }

        enc.configure()?;
        Ok(enc)
    }

    fn configure(&mut self) -> Result<()> {
        unsafe {
            let out_type = MFCreateMediaType()?;
            out_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            out_type.SetUINT64(&MF_MT_FRAME_SIZE, pack2(self.width, self.height))?;
            out_type.SetUINT64(&MF_MT_FRAME_RATE, pack2(self.fps, 1))?;
            out_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack2(1, 1))?;
            out_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            out_type.SetUINT32(&MF_MT_AVG_BITRATE, self.bitrate)?;
            out_type.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
            self.transform
                .SetOutputType(0, &out_type, 0)
                .context("设置编码器输出类型失败")?;

            let in_type = MFCreateMediaType()?;
            in_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            in_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            in_type.SetUINT64(&MF_MT_FRAME_SIZE, pack2(self.width, self.height))?;
            in_type.SetUINT64(&MF_MT_FRAME_RATE, pack2(self.fps, 1))?;
            in_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack2(1, 1))?;
            in_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            self.transform
                .SetInputType(0, &in_type, 0)
                .context("设置编码器输入类型失败")?;

            // 序列头（SPS/PPS）。拿不到不算错，关键帧里可能自带。
            match read_blob(&out_type, &MF_MT_MPEG_SEQUENCE_HEADER) {
                Some(seq) if !seq.is_empty() => {
                    tracing::debug!("编码器序列头 {} 字节", seq.len());
                    self.seq_header = seq;
                }
                _ => {}
            }

            let info: MFT_OUTPUT_STREAM_INFO = self.transform.GetOutputStreamInfo(0)?;
            self.provides_samples =
                info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
            tracing::info!(
                flags = info.dwFlags,
                cb = info.cbSize,
                alignment = info.cbAlignment,
                provides_samples = self.provides_samples,
                "编码器输出流信息"
            );
            if let Ok(attrs) = self.transform.GetAttributes() {
                let d3d_aware = attrs.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0);
                tracing::info!(d3d_aware, "编码器 D3D11 感知");
            }

            // 低延迟与码率控制。硬件 MFT 支持得多，软件 MFT 支持得少，逐个试。
            if let Ok(codec) = self.transform.cast::<ICodecAPI>() {
                tracing::info!(
                    low_latency = set_codec_bool(&codec, &CODECAPI_AVEncCommonLowLatency, true),
                    real_time = set_codec_bool(
                        &codec,
                        &windows::Win32::Media::MediaFoundation::CODECAPI_AVEncCommonRealTime,
                        true
                    ),
                    "低延迟参数"
                );
                set_codec_u32(
                    &codec,
                    &CODECAPI_AVEncCommonRateControlMode,
                    eAVEncCommonRateControlMode_CBR.0 as u32,
                );
                set_codec_u32(&codec, &CODECAPI_AVEncCommonMeanBitRate, self.bitrate);
                set_codec_u32(&codec, &CODECAPI_AVEncMPVDefaultBPictureCount, 0);
                // 无限 GOP：0xFFFF_FFFF 表示不自动插关键帧。
                set_codec_u32(&codec, &CODECAPI_AVEncMPVGOPSize, u32::MAX);
            }

            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }
        Ok(())
    }

    pub fn mode(&self) -> EncoderMode {
        self.mode
    }

    /// 帧号到关键帧的请求。收端丢包之后会用到（W2 接 NACK 时启用）。
    #[allow(dead_code)]
    pub fn request_keyframe(&mut self) {
        self.keyframe_pending = true;
    }

    /// 喂一帧 NV12，把这一帧产生的所有 NAL 返回。
    ///
    /// 一次输入可能吐出 0 个或多个 `EncodedFrame`（硬件编码器偶尔一次出两帧）。
    pub fn encode(&mut self, nv12: &[u8], pts_us: i64) -> Result<Vec<EncodedFrame>> {
        if nv12.len() != self.nv12_len {
            bail!(
                "NV12 长度不对：{} != {}（{}x{}）",
                nv12.len(),
                self.nv12_len,
                self.width,
                self.height
            );
        }

        let sample = unsafe {
            let buffer = MFCreateMemoryBuffer(nv12.len() as u32)?;
            let mut ptr = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None)?;
            std::ptr::copy_nonoverlapping(nv12.as_ptr(), ptr, nv12.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(nv12.len() as u32)?;

            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample
        };
        self.feed(sample, pts_us)
    }

    /// 硬件 MFT 的零拷贝路径：直接把 NV12 纹理交出去。
    pub fn encode_texture(
        &mut self,
        texture: &ID3D11Texture2D,
        pts_us: i64,
    ) -> Result<Vec<EncodedFrame>> {
        let sample = unsafe {
            let buffer = MFCreateDXGISurfaceBuffer(
                &<ID3D11Texture2D as Interface>::IID,
                texture,
                0,
                false,
            )
            .context("把纹理包成 MF 缓冲区失败")?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample
        };
        self.feed(sample, pts_us)
    }

    /// 该走纹理输入还是系统内存输入。
    pub fn wants_texture(&self) -> bool {
        self.d3d_manager.is_some()
    }

    fn feed(&mut self, sample: IMFSample, pts_us: i64) -> Result<Vec<EncodedFrame>> {
        let mut out = Vec::new();

        // MF 的时间单位是 100ns。
        unsafe {
            sample.SetSampleTime(pts_us * 10)?;
            sample.SetSampleDuration(10_000_000 / self.fps as i64)?;
        }

        if self.mode == EncoderMode::AsyncHardware {
            // 异步 MFT 的规矩：只有在它要输入的时候才能喂。
            self.pump(pts_us, &mut out)?;
            if !self.can_input {
                // 它还在消化上一帧，这一帧先不喂。实时流宁丢勿积。
                tracing::debug!("编码器未就绪，丢一帧");
                return Ok(out);
            }
        }

        if self.keyframe_pending {
            self.force_keyframe();
            self.keyframe_pending = false;
        }

        unsafe { self.transform.ProcessInput(0, &sample, 0) }
            .context("ProcessInput 失败")?;
        if self.mode == EncoderMode::AsyncHardware {
            self.can_input = false;
            // 异步 MFT 必须等 METransformHaveOutput 事件才能取输出，
            // 直接 ProcessOutput 会被判为非法调用（E_UNEXPECTED）。
            self.pump(pts_us, &mut out)?;
        } else {
            self.drain(pts_us, &mut out)?;
        }
        self.frame_serial += 1;
        Ok(out)
    }

    /// 收尾：把编码器里剩的帧挤出来。
    pub fn flush(&mut self) -> Result<Vec<EncodedFrame>> {
        let mut out = Vec::new();
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)?;
        }
        self.pump(0, &mut out)?;
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
        }
        Ok(out)
    }

    fn force_keyframe(&self) {
        if let Ok(codec) = self.transform.cast::<ICodecAPI>() {
            if !set_codec_u32(&codec, &CODECAPI_AVEncVideoForceKeyFrame, 1) {
                tracing::debug!("编码器不支持强制关键帧");
            }
        }
    }

    fn pump(&mut self, pts_us: i64, out: &mut Vec<EncodedFrame>) -> Result<()> {
        if self.mode != EncoderMode::AsyncHardware {
            return Ok(());
        }
        let generator = self
            .transform
            .cast::<windows::Win32::Media::MediaFoundation::IMFMediaEventGenerator>()
            .context("异步 MFT 不支持事件")?;

        loop {
            let event = match unsafe {
                generator.GetEvent(
                    windows::Win32::Media::MediaFoundation::MF_EVENT_FLAG_NO_WAIT,
                )
            } {
                Ok(e) => e,
                Err(_) => break,
            };
            let kind = unsafe { event.GetType() }.unwrap_or(0);
            tracing::debug!(kind, "MFT 事件");
            if kind == METransformNeedInput.0 as u32 {
                self.can_input = true;
            } else if kind == METransformHaveOutput.0 as u32 {
                self.drain(pts_us, out)?;
            }
        }
        Ok(())
    }

    fn drain(&self, _pts_us: i64, out: &mut Vec<EncodedFrame>) -> Result<()> {
        loop {
            let sample = if self.provides_samples {
                None
            } else {
                Some(unsafe {
                    let info: MFT_OUTPUT_STREAM_INFO = self.transform.GetOutputStreamInfo(0)?;
                    let size = info.cbSize.max((self.nv12_len as u32) * 2);
                    let buffer = MFCreateMemoryBuffer(size)?;
                    let sample = MFCreateSample()?;
                    sample.AddBuffer(&buffer)?;
                    sample
                })
            };

            let mut buffer = MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(sample),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            };
            let mut status = 0u32;

            let result = unsafe { self.transform.ProcessOutput(0, std::slice::from_mut(&mut buffer), &mut status) };
            if let Err(e) = &result {
                tracing::debug!(code = %e.code(), status, provides = self.provides_samples, "ProcessOutput 返回");
                // NVIDIA 的 MFT 在没有待输出帧时会丢一个 E_UNEXPECTED 出来，
                // 而不是规范里的 NEED_MORE_INPUT。没喂过帧就当成“没东西可拿”。
                if e.code() == windows::Win32::Foundation::E_UNEXPECTED {
                    return Ok(());
                }
            }
            if let Err(e) = result {
                if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT {
                    return Ok(());
                }
                return Err(e).context("ProcessOutput 失败");
            }

            let produced = buffer.pSample.take();
            let produced = match produced {
                Some(s) => s,
                None => return Ok(()),
            };

            let data = sample_bytes(&produced)?;
            if data.is_empty() {
                continue;
            }

            let annexb = normalize_to_annexb(&data);
            if annexb.is_empty() {
                continue;
            }

            let keyframe = contains_nal_type(&annexb, 5) || contains_nal_type(&annexb, 7);
            let mut payload = annexb;
            if keyframe && !contains_nal_type(&payload, 7) && !self.seq_header.is_empty() {
                let mut with_header = Vec::with_capacity(self.seq_header.len() + payload.len());
                with_header.extend_from_slice(&self.seq_header);
                with_header.extend_from_slice(&payload);
                payload = with_header;
            }

            // pts 以样本时间为准；硬件 MFT 会给回它自己的时钟。
            let pts_us = unsafe { produced.GetSampleTime() }.unwrap_or(0) / 10;

            out.push(EncodedFrame {
                id: self.frame_serial,
                pts_us: pts_us.max(0) as u64,
                keyframe,
                data: payload,
            });
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
        }
    }
}

/// 两个 u32 拼成一个 u64 属性值（MF 的 FRAME_SIZE / RATIO 都是这个约定）。
fn pack2(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

fn read_blob(attributes: &IMFMediaType, key: &GUID) -> Option<Vec<u8>> {
    unsafe {
        let size = attributes.GetBlobSize(key).ok()?;
        if size == 0 {
            return None;
        }
        let mut buf = vec![0u8; size as usize];
        let mut written = 0u32;
        attributes
            .GetBlob(key, &mut buf, Some(&mut written))
            .ok()?;
        buf.truncate(written as usize);
        Some(buf)
    }
}

fn set_codec_u32(codec: &ICodecAPI, key: &GUID, value: u32) -> bool {
    unsafe {
        let supported = codec.IsSupported(key).is_ok();
        let set = codec
            .SetValue(key, &windows::Win32::System::Variant::VARIANT::from(value))
            .is_ok();
        if !set {
            tracing::debug!(?key, supported, "编码器不支持该参数");
        }
        set
    }
}

fn set_codec_bool(codec: &ICodecAPI, key: &GUID, value: bool) -> bool {
    unsafe {
        codec
            .SetValue(
                key,
                &windows::Win32::System::Variant::VARIANT::from(value),
            )
            .is_ok()
    }
}

fn sample_bytes(sample: &IMFSample) -> Result<Vec<u8>> {
    unsafe {
        let buffer = sample.ConvertToContiguousBuffer()?;
        let mut ptr = std::ptr::null_mut();
        let mut len = 0u32;
        buffer.Lock(&mut ptr, None, Some(&mut len))?;
        let data = std::slice::from_raw_parts(ptr as *const u8, len as usize).to_vec();
        let _ = buffer.Unlock();
        Ok(data)
    }
}

/// 统一成 Annex-B：`00 00 01` 起始码。
///
/// MF 的软件编码器直接吐 Annex-B，部分硬件 MFT 吐 AVCC（4 字节长度前缀）。
/// 这里两种都认，输出一律 Annex-B，客户端解码器（含 Android MediaCodec）都吃这个。
pub fn normalize_to_annexb(data: &[u8]) -> Vec<u8> {
    if find_start_code(data, 0).is_some() {
        return data.to_vec();
    }

    let mut out = Vec::with_capacity(data.len() + 8);
    let mut i = 0;
    while i + 4 <= data.len() {
        let len = u32::from_be_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]) as usize;
        i += 4;
        if len == 0 || i + len > data.len() {
            // 长度前缀不成立，说明既不是 Annex-B 也不是 AVCC，原样返回。
            return data.to_vec();
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&data[i..i + len]);
        i += len;
    }
    out
}

fn find_start_code(data: &[u8], from: usize) -> Option<usize> {
    if data.len() < 3 {
        return None;
    }
    (from..data.len().saturating_sub(2)).find(|&i| {
        data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1
    })
}

/// 扫一遍 Annex-B 码流，看有没有某种 NAL。
pub fn contains_nal_type(data: &[u8], nal_type: u8) -> bool {
    let mut pos = 0;
    while let Some(idx) = find_start_code(data, pos) {
        let header = idx + 3;
        if header >= data.len() {
            return false;
        }
        if data[header] & 0x1F == nal_type {
            return true;
        }
        pos = header;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avcc_转_annexb() {
        // 两个 NAL：类型 7（SPS）与类型 1
        let mut avcc = Vec::new();
        avcc.extend_from_slice(&3u32.to_be_bytes());
        avcc.extend_from_slice(&[0x67, 0x42, 0x00]);
        avcc.extend_from_slice(&2u32.to_be_bytes());
        avcc.extend_from_slice(&[0x65, 0x88]);

        let out = normalize_to_annexb(&avcc);
        assert_eq!(&out[..4], &[0, 0, 0, 1]);
        assert!(contains_nal_type(&out, 7));
        assert!(contains_nal_type(&out, 5));
    }

    #[test]
    fn annexb_原样通过() {
        let data = [0u8, 0, 0, 1, 0x65, 0xAA, 0, 0, 1, 0x41];
        assert_eq!(normalize_to_annexb(&data), data.to_vec());
        assert!(contains_nal_type(&data, 5));
        assert!(!contains_nal_type(&data, 6));
    }
}
