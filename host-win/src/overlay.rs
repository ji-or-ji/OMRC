//! 时间码叠字。
//!
//! 用途只有一个：验收时拿另一台高帧率相机拍屏，肉眼比对主机与客户端的毫秒数。
//! 这是远程桌面延迟测量的标准土办法，因为它把所有真实开销（合成、编码、传输、
//! 解码、上屏）全算进去了。
//!
//! 实现上走 GDI 画一块小 DIB，再 `UpdateSubresource` 盖进桌面纹理的角落。
//! 每帧只有几百 KB 以内，比开一个渲染 pass 便宜得多。

use anyhow::{Context, Result};
use windows::Win32::Foundation::COLORREF;
use windows::Win32::Graphics::Gdi::{
    ANTIALIASED_QUALITY, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CLIP_DEFAULT_PRECIS,
    CreateCompatibleDC, CreateDIBSection, CreateFontW, DEFAULT_CHARSET, DIB_RGB_COLORS,
    DeleteDC, DeleteObject, FF_MODERN, FIXED_PITCH, FW_BOLD, HBITMAP, HDC, HFONT, HGDIOBJ,
    OUT_DEFAULT_PRECIS, SelectObject, SetBkMode, SetTextColor, TRANSPARENT, TextOutW,
};

pub struct TimecodeOverlay {
    dc: HDC,
    bitmap: HBITMAP,
    old_bitmap: HGDIOBJ,
    old_font: HGDIOBJ,
    font: HFONT,
    bits: *mut u8,
    pub height: u32,
    stride: usize,
}

impl TimecodeOverlay {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        unsafe {
            let dc = CreateCompatibleDC(None);
            if dc.is_invalid() {
                anyhow::bail!("CreateCompatibleDC 失败");
            }

            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width as i32,
                    // 负高度 = 自上而下，和 D3D 纹理的行序一致。
                    biHeight: -(height as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };

            let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
            let bitmap = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0)
                .context("CreateDIBSection 失败")?;
            if bitmap.is_invalid() || bits.is_null() {
                anyhow::bail!("CreateDIBSection 返回空");
            }
            let old_bitmap = SelectObject(dc, bitmap.into());

            let font = CreateFontW(
                -(height as i32 * 3 / 5),
                0,
                0,
                0,
                FW_BOLD.0 as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                ANTIALIASED_QUALITY,
                (FIXED_PITCH.0 | FF_MODERN.0) as u32,
                windows::core::w!("Consolas"),
            );
            let old_font = SelectObject(dc, font.into());
            SetBkMode(dc, TRANSPARENT);

            Ok(Self {
                dc,
                bitmap,
                old_bitmap,
                old_font,
                font,
                bits: bits as *mut u8,
                height,
                stride: width as usize * 4,
            })
        }
    }

    /// 画一帧时间码，返回 BGRA 像素（可直接盖到纹理上）。
    pub fn render(&mut self, text: &str) -> &[u8] {
        unsafe {
            let pixels = std::slice::from_raw_parts_mut(self.bits, self.stride * self.height as usize);
            for px in pixels.chunks_exact_mut(4) {
                px[0] = 0x18;
                px[1] = 0x14;
                px[2] = 0x10;
                px[3] = 0xFF;
            }
            SetTextColor(self.dc, COLORREF(0x00FF_FFFF));
            let wide: Vec<u16> = text.encode_utf16().collect();
            let _ = TextOutW(self.dc, 6, 2, &wide);
            std::slice::from_raw_parts(self.bits, self.stride * self.height as usize)
        }
    }
}

impl Drop for TimecodeOverlay {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old_bitmap);
            SelectObject(self.dc, self.old_font);
            let _ = DeleteObject(self.font.into());
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.dc);
        }
    }
}
