//! GPU 侧的 BGRA → NV12 转换。
//!
//! 用 D3D11 Video Processor 做色彩空间转换与 4:2:0 下采样，全程在显存里，
//! 不落 CPU。硬件编码器（NVENC / QSV / AMF）要的就是这张 NV12 纹理，
//! 给它系统内存的 NV12 反而不认——那会多绕一圈拷贝，还拿不到低延迟。
//!
//! 这一步替掉了两条老路：`Map` 回读（1080p 约 3~5ms + 一次全帧拷贝）和
//! CPU 色彩转换（约 3~6ms）。

use std::mem::ManuallyDrop;

use anyhow::{Context, Result};
use windows::Win32::Foundation::TRUE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D, ID3D11Device,
    ID3D11DeviceContext, ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoDevice,
    ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView,
    ID3D11VideoProcessorOutputView,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::core::Interface;

pub struct Nv12Converter {
    video_context: ID3D11VideoContext,
    processor: ID3D11VideoProcessor,
    input_view: ID3D11VideoProcessorInputView,
    /// 输出纹理与它对应的视图，轮着用。
    ///
    /// 只给一张会出事：编码器是异步的，它可能还在读上一帧的纹理，
    /// 下一帧的 Blt 就把它盖了——画面会抽黑。三个一起转足够拉开距离。
    outputs: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    next: usize,
    width: u32,
    height: u32,
}

const TEXTURE_POOL: usize = 3;

impl Nv12Converter {
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        source: &ID3D11Texture2D,
        width: u32,
        height: u32,
        fps: u32,
    ) -> Result<Self> {
        ensure_supported_format(source)?;
        let video_device: ID3D11VideoDevice =
            device.cast().context("设备不支持 ID3D11VideoDevice")?;
        let video_context: ID3D11VideoContext =
            context.cast().context("上下文不支持 ID3D11VideoContext")?;

        let rate = DXGI_RATIONAL {
            Numerator: fps.max(1),
            Denominator: 1,
        };
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: width,
            InputHeight: height,
            OutputFrameRate: rate,
            OutputWidth: width,
            OutputHeight: height,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };

        let enumerator: ID3D11VideoProcessorEnumerator = unsafe {
            video_device
                .CreateVideoProcessorEnumerator(&desc)
                .context("创建视频处理器枚举器失败")?
        };
        let processor = unsafe { video_device.CreateVideoProcessor(&enumerator, 0) }
            .context("创建视频处理器失败")?;

        let input_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: 0,
                },
            },
        };
        let mut input_view: Option<ID3D11VideoProcessorInputView> = None;
        unsafe {
            video_device
                .CreateVideoProcessorInputView(source, &enumerator, &input_desc, Some(&mut input_view))
                .context("创建输入视图失败")?
        };
        let input_view = input_view.context("输入视图为空")?;

        let output_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };

        let mut outputs = Vec::with_capacity(TEXTURE_POOL);
        for _ in 0..TEXTURE_POOL {
            let texture = create_nv12_texture(device, width, height)?;
            let mut view: Option<ID3D11VideoProcessorOutputView> = None;
            unsafe {
                video_device
                    .CreateVideoProcessorOutputView(
                        &texture,
                        &enumerator,
                        &output_desc,
                        Some(&mut view),
                    )
                    .context("创建输出视图失败")?
            };
            let view = view.context("输出视图为空")?;
            outputs.push((texture, view));
        }

        tracing::info!("视频处理器就绪（BGRA → NV12，显存内，{} 张纹理轮转）", TEXTURE_POOL);
        Ok(Self {
            video_context,
            processor,
            input_view,
            outputs,
            next: 0,
            width,
            height,
        })
    }

    /// 把源纹理转成 NV12，返回本轮用的那张。
    pub fn convert(&mut self) -> Result<&ID3D11Texture2D> {
        let idx = self.next % self.outputs.len();
        self.next = self.next.wrapping_add(1);

        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: TRUE,
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            ppPastSurfaces: std::ptr::null_mut(),
            pInputSurface: ManuallyDrop::new(Some(self.input_view.clone())),
            ppFutureSurfaces: std::ptr::null_mut(),
            ppPastSurfacesRight: std::ptr::null_mut(),
            pInputSurfaceRight: ManuallyDrop::new(None),
            ppFutureSurfacesRight: std::ptr::null_mut(),
        };

        let result = unsafe {
            self.video_context.VideoProcessorBlt(
                &self.processor,
                &self.outputs[idx].1,
                0,
                std::slice::from_ref(&stream),
            )
        };
        // VideoProcessorBlt 不接管引用计数，手动放掉我们加的那一个。
        unsafe { ManuallyDrop::drop(&mut stream.pInputSurface) };

        result.context("视频处理器 Blt 失败")?;
        let _ = (self.width, self.height);
        Ok(&self.outputs[idx].0)
    }
}

fn create_nv12_texture(device: &ID3D11Device, width: u32, height: u32) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        // 视频处理器的输出必须是渲染目标。
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture: Option<ID3D11Texture2D> = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
        .context("创建 NV12 纹理失败")?;
    texture.context("NV12 纹理为空")
}

/// 源纹理格式检查，尽早把 HDR / 10bit 这类情况挡在门外。
pub fn ensure_supported_format(texture: &ID3D11Texture2D) -> Result<()> {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { texture.GetDesc(&mut desc) };
    if desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
        anyhow::bail!("源纹理格式 {:?} 暂不支持", desc.Format);
    }
    Ok(())
}