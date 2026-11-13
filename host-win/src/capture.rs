//! DXGI 桌面复制。
//!
//! 一次抓帧的完整动作：`AcquireNextFrame` → 把桌面纹理拷进我们自己的纹理 →
//! 取光标 → `ReleaseFrame`。
//!
//! 为什么要自己拷一份：
//!
//! 1. 拷进 `BIND_RENDER_TARGET` 的纹理，才能在上面叠时间码；
//! 2. 桌面纹理只在 `ReleaseFrame` 之前有效，自己那份可以随便留；
//! 3. 编码器要的是稳定输入，不能给它一张随时会变的纹理。
//!
//! 全屏独占游戏会让复制失败（`ACCESS_LOST`），上层重建即可。

use anyhow::{Context, Result, bail};
use omrc_core::proto::{CursorFormat, CursorPos, CursorShape};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, ID3D11Device,
    ID3D11DeviceContext, ID3D11Texture2D, D3D11_BOX,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
    DXGI_OUTDUPL_POINTER_SHAPE_INFO, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME,
    IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
};
use windows::core::Interface;

use crate::d3d::OutputInfo;

/// 光标这一帧的状态。
#[derive(Debug, Clone)]
pub struct PointerUpdate {
    pub pos: CursorPos,
    /// 只在形状变化时才有值。
    pub shape: Option<CursorShape>,
}

/// 一次 `acquire` 的结果。
pub enum Acquire {
    /// 拿到一帧，目标纹理已更新。
    Frame(PointerUpdate),
    /// 这段时间屏幕没变化。
    Timeout,
    /// 复制链路断了（切分辨率、全屏独占、锁屏、UAC 都算）。需要重建。
    AccessLost,
}

pub struct Duplicator {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    dupl: IDXGIOutputDuplication,
    target: ID3D11Texture2D,
    staging: ID3D11Texture2D,
    width: u32,
    height: u32,
    shape_buf: Vec<u8>,
    last_shape: Option<CursorShape>,
}

impl Duplicator {
    pub fn new(
        device: ID3D11Device,
        context: ID3D11DeviceContext,
        info: &OutputInfo,
    ) -> Result<Self> {
        let output1: IDXGIOutput1 = info.output.cast().context("输出不支持 IDXGIOutput1")?;
        let dupl = unsafe { output1.DuplicateOutput(&device) }
            .context("DuplicateOutput 失败（可能已有别的进程在复制这个输出）")?;

        let width = info.width;
        let height = info.height;
        if width == 0 || height == 0 {
            bail!("输出尺寸为 0：{}", info.label());
        }

        let target = create_texture(
            &device,
            width,
            height,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            D3D11_USAGE_DEFAULT,
            D3D11_BIND_RENDER_TARGET.0 as u32,
            0,
        )?;
        let staging = create_texture(
            &device,
            width,
            height,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            D3D11_USAGE_STAGING,
            0,
            D3D11_CPU_ACCESS_READ.0 as u32,
        )?;

        tracing::info!(width, height, "桌面复制就绪");
        Ok(Self {
            device,
            context,
            dupl,
            target,
            staging,
            width,
            height,
            shape_buf: Vec::new(),
            last_shape: None,
        })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// 可写的目标纹理（叠字、送编码器都读它）。
    pub fn target(&self) -> &ID3D11Texture2D {
        &self.target
    }

    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    pub fn context(&self) -> &ID3D11DeviceContext {
        &self.context
    }

    pub fn acquire(&mut self, timeout_ms: u32) -> Result<Acquire> {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;

        let acquired = unsafe { self.dupl.AcquireNextFrame(timeout_ms, &mut info, &mut resource) };
        if let Err(e) = acquired {
            if e.code() == DXGI_ERROR_WAIT_TIMEOUT {
                return Ok(Acquire::Timeout);
            }
            if e.code() == DXGI_ERROR_ACCESS_LOST {
                return Ok(Acquire::AccessLost);
            }
            return Err(e).context("AcquireNextFrame 失败");
        }

        // 拿到就必须还，中间出错也要还。
        let result = self.capture_frame(&info, resource);
        let _ = unsafe { self.dupl.ReleaseFrame() };
        result
    }

    fn capture_frame(
        &mut self,
        info: &DXGI_OUTDUPL_FRAME_INFO,
        resource: Option<IDXGIResource>,
    ) -> Result<Acquire> {
        let resource = resource.context("AcquireNextFrame 成功但没给纹理")?;
        let texture: ID3D11Texture2D = resource.cast().context("桌面资源不是纹理")?;

        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        if desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
            bail!(
                "桌面格式 {:?} 暂不支持（当前只处理 SDR 的 B8G8R8A8）",
                desc.Format
            );
        }

        // 整个复制：桌面纹理 → 自己的纹理。
        unsafe { self.context.CopyResource(&self.target, &texture) };

        let mut pointer = PointerUpdate {
            pos: CursorPos {
                x: info.PointerPosition.Position.x,
                y: info.PointerPosition.Position.y,
                visible: info.PointerPosition.Visible.as_bool(),
            },
            shape: None,
        };

        // 只有形状变了才重新取。
        if info.PointerShapeBufferSize > 0 {
            match self.read_pointer_shape(info.PointerShapeBufferSize) {
                Ok(Some(shape)) => {
                    if self.last_shape.as_ref() != Some(&shape) {
                        self.last_shape = Some(shape.clone());
                        pointer.shape = Some(shape);
                    }
                }
                Ok(None) => {}
                Err(e) => tracing::debug!("读取光标形状失败: {e:#}"),
            }
        }

        Ok(Acquire::Frame(pointer))
    }

    fn read_pointer_shape(&mut self, hint_size: u32) -> Result<Option<CursorShape>> {
        let needed = hint_size.max(256) as usize;
        if self.shape_buf.len() < needed {
            self.shape_buf.resize(needed, 0);
        }

        let mut required = 0u32;
        let mut shape_info = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
        let res = unsafe {
            self.dupl.GetFramePointerShape(
                self.shape_buf.len() as u32,
                self.shape_buf.as_mut_ptr() as *mut _,
                &mut required,
                &mut shape_info,
            )
        };

        if res.is_err() {
            // 缓冲区不够就按它要的再要一次。
            if required as usize > self.shape_buf.len() {
                self.shape_buf.resize(required as usize, 0);
                unsafe {
                    self.dupl.GetFramePointerShape(
                        self.shape_buf.len() as u32,
                        self.shape_buf.as_mut_ptr() as *mut _,
                        &mut required,
                        &mut shape_info,
                    )
                }
                .context("读取光标形状失败")?;
            } else {
                return Err(res.err().unwrap()).context("读取光标形状失败");
            }
        }

        if shape_info.Width == 0 || shape_info.Height == 0 {
            return Ok(None);
        }

        let size = required as usize;
        let raw = &self.shape_buf[..size.min(self.shape_buf.len())];
        convert_shape(&shape_info, raw)
    }

    /// 把目标纹理读回 CPU 并直接转成 NV12（编码器要的格式）。
    ///
    /// Map 期间直接做完颜色转换，省一次整帧拷贝。
    pub fn read_nv12(&mut self, out: &mut Vec<u8>) -> Result<()> {
        unsafe { self.context.CopyResource(&self.staging, &self.target) };

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.context
                .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
        }
        .context("映射暂存纹理失败")?;

        let need = (self.width as usize * self.height as usize * 3) / 2;
        if out.len() != need {
            out.resize(need, 0);
        }
        omrc_core::pixel::bgra_to_nv12_strided(
            mapped.pData as *const u8,
            mapped.RowPitch as usize,
            self.width as usize,
            self.height as usize,
            out,
        );
        unsafe { self.context.Unmap(&self.staging, 0) };
        Ok(())
    }

    /// 把目标纹理读回 CPU，BGRA、紧密排列。调试图、测像素时用。
    #[allow(dead_code)]
    pub fn read_bgra(&mut self, out: &mut Vec<u8>) -> Result<()> {
        unsafe { self.context.CopyResource(&self.staging, &self.target) };

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.context
                .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
        }
        .context("映射暂存纹理失败")?;

        let w = self.width as usize;
        let h = self.height as usize;
        let src_stride = mapped.RowPitch as usize;
        out.resize(w * h * 4, 0);
        unsafe {
            let src = mapped.pData as *const u8;
            let dst = out.as_mut_ptr();
            for y in 0..h {
                std::ptr::copy_nonoverlapping(
                    src.add(y * src_stride),
                    dst.add(y * w * 4),
                    w * 4,
                );
            }
            self.context.Unmap(&self.staging, 0);
        }
        Ok(())
    }

    /// 往目标纹理上盖一块 BGRA 小图（时间码）。
    pub fn blit_overlay(
        &self,
        data: &[u8],
        width: u32,
        height: u32,
        x: u32,
        y: u32,
    ) -> Result<()> {
        let box_ = D3D11_BOX {
            left: x,
            top: y,
            front: 0,
            right: (x + width).min(self.width),
            bottom: (y + height).min(self.height),
            back: 1,
        };
        unsafe {
            self.context.UpdateSubresource(
                &self.target,
                0,
                Some(&box_),
                data.as_ptr() as *const _,
                width * 4,
                0,
            )
        };
        Ok(())
    }
}

fn create_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    usage: windows::Win32::Graphics::Direct3D11::D3D11_USAGE,
    bind: u32,
    cpu: u32,
) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: usage,
        BindFlags: bind,
        CPUAccessFlags: cpu,
        MiscFlags: 0,
    };
    let mut texture: Option<ID3D11Texture2D> = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }.context("创建纹理失败")?;
    texture.context("CreateTexture2D 未返回纹理")
}

/// 把 DXGI 给的光标位图转成统一格式。
fn convert_shape(
    info: &DXGI_OUTDUPL_POINTER_SHAPE_INFO,
    raw: &[u8],
) -> Result<Option<CursorShape>> {
    let w = info.Width as usize;
    let h = info.Height as usize;
    if w == 0 || h == 0 {
        return Ok(None);
    }

    if info.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 as u32
        || info.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 as u32
    {
        let pitch = info.Pitch.max((w * 4) as u32) as usize;
        let mut pixels = vec![0u8; w * h * 4];
        for y in 0..h {
            let src_off = y * pitch;
            if src_off + w * 4 > raw.len() {
                bail!("光标位图数据不足：{} < {}", raw.len(), src_off + w * 4);
            }
            pixels[y * w * 4..(y + 1) * w * 4]
                .copy_from_slice(&raw[src_off..src_off + w * 4]);
        }
        if info.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 as u32 {
            // MASKED_COLOR：alpha 为 0 处是按位取反，这里简化为全不透明，
            // 边角会有轻微差异，但不会出现黑块。
            for px in pixels.chunks_exact_mut(4) {
                px[3] = 0xFF;
            }
        }
        return Ok(Some(CursorShape {
            format: CursorFormat::Bgra,
            width: w as u16,
            height: h as u16,
            hot_x: info.HotSpot.x.max(0) as u16,
            hot_y: info.HotSpot.y.max(0) as u16,
            pixels,
        }));
    }

    if info.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32 {
        // 单色光标：先 AND 掩码再 XOR 掩码，都是 1bpp，行按 4 字节对齐。
        let row_bytes = (w + 31) / 32 * 4;
        let mask_size = row_bytes * h;
        if raw.len() < mask_size * 2 {
            bail!("单色光标数据不足");
        }
        let (and_mask, xor_mask) = raw.split_at(mask_size);

        let mut pixels = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let byte = y * row_bytes + x / 8;
                let bit = 7 - (x % 8);
                let a = (and_mask[byte] >> bit) & 1;
                let v = (xor_mask[byte] >> bit) & 1;
                let px = &mut pixels[(y * w + x) * 4..][..4];
                match (a, v) {
                    (1, 0) => px[3] = 0,                        // 透明
                    (1, 1) => px.copy_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]), // 白
                    _ => px.copy_from_slice(&[0x00, 0x00, 0x00, 0xFF]),      // 黑
                }
            }
        }
        return Ok(Some(CursorShape {
            format: CursorFormat::Mono,
            width: w as u16,
            height: h as u16,
            hot_x: info.HotSpot.x.max(0) as u16,
            hot_y: info.HotSpot.y.max(0) as u16,
            pixels,
        }));
    }

    // 其它类型（含未知）不画光标，但位置照常发，客户端可以画个占位。
    Ok(None)
}
