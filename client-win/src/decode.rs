//! H.264 解码。
//!
//! W1a 先用 Media Foundation 的同步解码器（软件，稳），把链路打通。
//! 上真机时 Windows 客户端会换 D3D11 硬解 + SwapChain，这里只求正确。
//!
//! 输入是 Annex-B（主机侧已归一），输出统一成 BGRA，交给 GDI 贴到窗口。

use std::mem::ManuallyDrop;

use anyhow::{Context, Result};
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFMediaType, IMFTransform, MFMediaType_Video, MFT_CATEGORY_VIDEO_DECODER,
    MFT_ENUM_FLAG, MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_INFO, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
    MFVideoFormat_H264, MFVideoFormat_NV12, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE,
    MF_MT_MINIMUM_DISPLAY_APERTURE, MF_MT_SUBTYPE, MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_E_TRANSFORM_STREAM_CHANGE, MFCreateMemoryBuffer, MFCreateMediaType, MFCreateSample, MFTEnumEx,
};
use windows::Win32::System::Com::CoTaskMemFree;

use omrc_core::pixel::nv12_to_bgra_strided;

pub struct Decoder {
    transform: IMFTransform,
    provides_samples: bool,
    width: u32,
    height: u32,
    coded_width: u32,
    coded_height: u32,
    nv12: Vec<u8>,
    bgra: Vec<u8>,
    inputs: u64,
    outputs: u64,
    rejects: u64,
    present_index: u64,
}

impl Decoder {
    pub fn new() -> Result<Self> {
        let input_info = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_H264,
        };
        let output_info = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_NV12,
        };

        let activate = Self::find(MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER, &input_info, &output_info)?
            .or(Self::find(MFT_ENUM_FLAG_SORTANDFILTER, &input_info, &output_info)?)
            .context("找不到 H.264 解码器 MFT")?;

        let transform: IMFTransform = unsafe { activate.ActivateObject() }
            .context("激活解码器 MFT 失败")?;

        unsafe {
            let in_type = MFCreateMediaType()?;
            in_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            in_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            transform
                .SetInputType(0, &in_type, 0)
                .context("解码器设置输入类型失败")?;
        }

        let info: MFT_OUTPUT_STREAM_INFO = unsafe { transform.GetOutputStreamInfo(0)? };
        let provides_samples =
            info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
        unsafe {
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }

        let mut decoder = Self {
            transform,
            provides_samples,
            width: 0,
            height: 0,
            coded_width: 0,
            coded_height: 0,
            nv12: Vec::new(),
            bgra: Vec::new(),
            inputs: 0,
            outputs: 0,
            rejects: 0,
            present_index: 0,
        };

        // 有的解码器要等第一帧才知道分辨率，那时会报 STREAM_CHANGE。
        decoder.try_set_output_type();
        Ok(decoder)
    }

    fn find(
        flags: MFT_ENUM_FLAG,
        input: &MFT_REGISTER_TYPE_INFO,
        output: &MFT_REGISTER_TYPE_INFO,
    ) -> Result<Option<IMFActivate>> {
        let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        unsafe {
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_DECODER,
                flags,
                Some(input),
                Some(output),
                &mut activates,
                &mut count,
            )
        }
        .context("MFTEnumEx（解码器）失败")?;

        if count == 0 || activates.is_null() {
            return Ok(None);
        }
        let list = unsafe { std::slice::from_raw_parts(activates, count as usize) };
        let first = list.iter().find_map(|a| a.clone());
        unsafe { CoTaskMemFree(Some(activates as *const _)) };
        Ok(first)
    }

    fn try_set_output_type(&mut self) {
        unsafe {
            let Ok(out_type) = self.transform.GetOutputAvailableType(0, 0) else {
                return;
            };
            if self.transform.SetOutputType(0, &out_type, 0).is_err() {
                return;
            }
            self.read_size(&out_type);
        }
    }

    fn read_size(&mut self, mt: &IMFMediaType) {
        if let Ok(packed) = unsafe { mt.GetUINT64(&MF_MT_FRAME_SIZE) } {
            self.coded_width = (packed >> 32) as u32;
            self.coded_height = (packed & 0xFFFF_FFFF) as u32;
            self.width = self.coded_width;
            self.height = self.coded_height;
        }

        // 可见区域（宏块对齐时会比 coded 小，比如 1080 编成 1088）。
        if let Ok(size) = unsafe { mt.GetBlobSize(&MF_MT_MINIMUM_DISPLAY_APERTURE) } {
            if size >= 16 {
                let mut buf = [0u8; 16];
                let mut written = 0u32;
                if unsafe { mt.GetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, &mut buf, Some(&mut written)) }
                    .is_ok()
                {
                    // MFVideoArea { MFOffset {fract:u16, value:i16}, SIZE {cx:i32, cy:i32} }
                    let cx = i32::from_ne_bytes([buf[8], buf[9], buf[10], buf[11]]);
                    let cy = i32::from_ne_bytes([buf[12], buf[13], buf[14], buf[15]]);
                    if cx > 0 && cy > 0 && cx as u32 <= self.coded_width {
                        self.width = cx as u32;
                        self.height = cy as u32;
                    }
                }
            }
        }
        tracing::info!(
            visible_w = self.width,
            visible_h = self.height,
            coded_w = self.coded_width,
            coded_h = self.coded_height,
            "解码器输出尺寸"
        );
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn stats(&self) -> (u64, u64, u64) {
        (self.inputs, self.outputs, self.rejects)
    }

    /// 喂一帧 Annex-B，返回最新解出的 BGRA。
    ///
    /// 解码器可能一次吐多帧（或一帧都不吐），这里会把它抽干，只留最新的那张：
    /// 实时流宁丢旧不积压。
    pub fn decode(&mut self, annexb: &[u8], pts_us: u64) -> Result<Option<&[u8]>> {
        let sample = unsafe {
            let buffer = MFCreateMemoryBuffer(annexb.len() as u32)?;
            let mut ptr = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None)?;
            std::ptr::copy_nonoverlapping(annexb.as_ptr(), ptr, annexb.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(annexb.len() as u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            if pts_us > 0 {
                // 用本地单调计数器做时间戳：主机给的 pts 是墙钟微秒，跳变会被
                // 解码器当成乱序帧丢掉。
                sample.SetSampleTime(self.present_index as i64 * 166_666)?;
                sample.SetSampleDuration(166_666)?;
            }
            sample
        };

        let mut got = false;
        if let Err(_e) = unsafe { self.transform.ProcessInput(0, &sample, 0) } {
            self.rejects += 1;
            // 上一帧还卡在里面：先抽干输出（那里面的一帧不能丢），再重试一次。
            got = self.drain_all()?;
            unsafe { self.transform.ProcessInput(0, &sample, 0) }
                .context("解码 ProcessInput 失败")?;
        }
        self.inputs += 1;
        self.present_index += 1;

        got |= self.drain_all()?;
        Ok(if got { Some(&self.bgra) } else { None })
    }

    /// 把解码器里能拿的输出全拿完，只保留最后一张。
    fn drain_all(&mut self) -> Result<bool> {
        let mut got = false;
        loop {
            let sample = if self.provides_samples {
                None
            } else {
                // 缓冲区要按 **编码尺寸** 算，不是可见尺寸：1080 的帧解码出来是 1088 行，
                // 按可见尺寸给会短一截，解码器报 CopyDecodedFrame failed。
                let need = if self.coded_width > 0 && self.coded_height > 0 {
                    self.coded_width as usize * self.coded_height as usize * 3 / 2
                } else {
                    8 << 20
                };
                Some(unsafe {
                    let buffer = MFCreateMemoryBuffer(need as u32)?;
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
            let result = unsafe {
                self.transform
                    .ProcessOutput(0, std::slice::from_mut(&mut buffer), &mut status)
            };

            match result {
                Ok(()) => {
                    let Some(sample) = buffer.pSample.take() else {
                        return Ok(got);
                    };
                    let data = unsafe {
                        let buf = sample.ConvertToContiguousBuffer()?;
                        let mut ptr = std::ptr::null_mut();
                        let mut len = 0u32;
                        buf.Lock(&mut ptr, None, Some(&mut len))?;
                        let v = std::slice::from_raw_parts(ptr as *const u8, len as usize).to_vec();
                        let _ = buf.Unlock();
                        v
                    };

                    if self.width == 0 || self.height == 0 {
                        continue;
                    }
                    let coded_w = self.coded_width as usize;
                    let coded_h = self.coded_height as usize;
                    let y_size = coded_w * coded_h;
                    if data.len() < y_size + y_size / 2 {
                        continue;
                    }
                    let (y, uv) = data.split_at(y_size);
                    let (w, h) = (self.width as usize, self.height as usize);
                    let need = w * h * 4;
                    if self.bgra.len() != need {
                        self.bgra.resize(need, 0);
                    }
                    // 解码器吐的是宏块对齐尺寸（1080 → 1088），按真实宽高抠出可见区。
                    nv12_to_bgra_strided(
                        y,
                        coded_w,
                        uv,
                        coded_w,
                        w,
                        h,
                        &mut self.bgra,
                    );
                    self.outputs += 1;
                    got = true;
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(got),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    self.try_set_output_type();
                    continue;
                }
                Err(e) => return Err(e).context("解码 ProcessOutput 失败"),
            }
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        let _ = self.nv12.capacity();
    }
}
