//! 回环验证用的显示窗口。
//!
//! W1a 不追求好看的 UI，只要能把解出来的画面贴上去，并显示 fps / 延迟 / 码率。
//! 真正的 Windows 客户端在 W2 之后走 D3D11 SwapChain，这层到时整个换掉。

use std::time::Instant;

use anyhow::Result;
use omrc_core::proto::{ControlMessage, CursorPos, CursorShape};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleDC,
    CreateDIBSection, CreateSolidBrush, DIB_RGB_COLORS, DeleteDC, DeleteObject, EndPaint, FillRect,
    HDC, HBITMAP, HGDIOBJ, InvalidateRect, PAINTSTRUCT, SRCCOPY, SelectObject, StretchDIBits,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GWLP_USERDATA, GetClientRect, GetWindowLongPtrW, IsWindowVisible, MSG,
    PM_REMOVE, PeekMessageW, PostQuitMessage, RegisterClassW, SW_SHOW, SetWindowLongPtrW,
    SetWindowTextW, ShowWindow, TranslateMessage, WM_DESTROY, WM_ERASEBKGND, WM_PAINT, WM_SIZE,
    WNDCLASSW, WS_OVERLAPPEDWINDOW,
};
use windows::core::w;

pub struct App {
    hwnd: HWND,
    width: u32,
    height: u32,
    /// 干净的解码结果。光标直接叠在上面——下一帧会整个覆盖，
    /// 不必为了防拖影再保一份干净的。
    video: Vec<u8>,
    cursor_shape: Option<CursorShape>,
    cursor_pos: CursorPos,
    has_frame: bool,

    frames: u64,
    bytes: u64,
    latency_sum_us: u64,
    latency_n: u64,
    paint_us: u64,
    paint_n: u64,
    last_stat: Instant,
    last_pts_us: i64,
    /// 双缓冲：先画到内存 DC，再一次性贴上去。
    ///
    /// 不加这层的话，每帧“黑底 → 拉伸上屏”中间会赫一帧黑，
    /// 快速运动时就是一直闪黑。
    back_dc: HDC,
    back_bitmap: HBITMAP,
    back_old: HGDIOBJ,
    back_w: i32,
    back_h: i32,
}

impl App {
    pub fn new() -> Self {
        Self {
            hwnd: HWND::default(),
            width: 0,
            height: 0,
            video: Vec::new(),
            cursor_shape: None,
            cursor_pos: CursorPos {
                x: 0,
                y: 0,
                visible: false,
            },
            has_frame: false,
            frames: 0,
            bytes: 0,
            latency_sum_us: 0,
            latency_n: 0,
            paint_us: 0,
            paint_n: 0,
            last_stat: Instant::now(),
            last_pts_us: 0,
            back_dc: HDC::default(),
            back_bitmap: HBITMAP::default(),
            back_old: HGDIOBJ::default(),
            back_w: 0,
            back_h: 0,
        }
    }

    pub fn create_window(&mut self, title: &str, width: i32, height: i32) -> Result<()> {
        unsafe {
            let instance = GetModuleHandleW(None)?;
            let class_name = w!("omrc_client_window");

            let wc = WNDCLASSW {
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(wndproc),
                hInstance: instance.into(),
                lpszClassName: class_name,
                ..Default::default()
            };
            let atom = RegisterClassW(&wc);
            if atom == 0 {
                anyhow::bail!("RegisterClassW 失败（可能已注册过）");
            }

            let hwnd = CreateWindowExW(
                Default::default(),
                class_name,
                &windows::core::HSTRING::from(title),
                WS_OVERLAPPEDWINDOW,
                100,
                100,
                width,
                height,
                None,
                None,
                Some(instance.into()),
                None,
            )?;

            self.hwnd = hwnd;
            let app_ptr = self as *mut App;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, app_ptr as isize);
            let _ = ShowWindow(hwnd, SW_SHOW);
            Ok(())
        }
    }

    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    pub fn on_control(&mut self, msg: ControlMessage) {
        match msg {
            ControlMessage::CursorShape(shape) => {
                self.cursor_shape = Some(shape);
                self.recompose();
            }
            ControlMessage::CursorPos(pos) => {
                self.cursor_pos = pos;
                self.recompose();
            }
            _ => {}
        }
    }

    pub fn on_frame(&mut self, bgra: &[u8], width: u32, height: u32, pts_us: u64, wire_bytes: u64) {
        if self.width != width || self.height != height {
            self.width = width;
            self.height = height;
            self.video.resize((width as usize * height as usize * 4) as usize, 0);
            self.recompose();
        }
        if bgra.len() != self.video.len() {
            return;
        }
        self.video.copy_from_slice(bgra);
        self.has_frame = true;
        self.frames += 1;
        self.bytes += wire_bytes;

        // 同机回环：pts 和本地时钟同一个纪元，可以直接比。
        let now = now_us();
        if pts_us > 0 && now > pts_us as i64 {
            self.latency_sum_us += (now - pts_us as i64) as u64;
            self.latency_n += 1;
        }
        self.last_pts_us = pts_us as i64;

        self.recompose();
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
    }

    fn recompose(&mut self) {
        if self.video.is_empty() {
            return;
        }
        if !self.cursor_pos.visible || self.width == 0 {
            return;
        }
        let Some(shape) = &self.cursor_shape else {
            return;
        };
        composite_cursor(
            &mut self.video,
            self.width,
            self.height,
            shape,
            self.cursor_pos.x,
            self.cursor_pos.y,
        );
    }

    pub fn tick(&mut self) {
        if self.last_stat.elapsed().as_secs_f64() < 1.0 {
            return;
        }
        let secs = self.last_stat.elapsed().as_secs_f64();
        let fps = self.frames as f64 / secs;
        let mbps = self.bytes as f64 * 8.0 / secs / 1e6;
        let latency = if self.latency_n > 0 {
            self.latency_sum_us as f64 / self.latency_n as f64 / 1000.0
        } else {
            0.0
        };
        let title = format!(
            "OMRC 客户端  {:.0} fps  {:.1} Mbps  端到端 {:.1} ms  {}x{}",
            fps, mbps, latency, self.width, self.height
        );
        unsafe {
            let _ = SetWindowTextW(self.hwnd, &windows::core::HSTRING::from(title));
        }
        tracing::info!(
            "{:.0} fps  {:.1} Mbps  端到端 {:.1} ms  绘制 {:.1} ms",
            fps,
            mbps,
            latency,
            if self.paint_n > 0 {
                self.paint_us as f64 / self.paint_n as f64 / 1000.0
            } else {
                0.0
            }
        );

        self.frames = 0;
        self.bytes = 0;
        self.latency_sum_us = 0;
        self.latency_n = 0;
        self.paint_us = 0;
        self.paint_n = 0;
        self.last_stat = Instant::now();
    }

    /// 确保内存 DC 跟客户区一样大。尺寸变了（拉伸窗口）就重建。
    fn ensure_backbuffer(&mut self, w: i32, h: i32) {
        if self.back_dc.is_invalid() || self.back_w != w || self.back_h != h {
            unsafe {
                if !self.back_dc.is_invalid() {
                    SelectObject(self.back_dc, self.back_old);
                    let _ = DeleteObject(self.back_bitmap.into());
                    let _ = DeleteDC(self.back_dc);
                }
                self.back_dc = CreateCompatibleDC(None);
                let info = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                        biWidth: w,
                        biHeight: -h,
                        biPlanes: 1,
                        biBitCount: 32,
                        biCompression: BI_RGB.0,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
                if let Ok(bitmap) =
                    CreateDIBSection(Some(self.back_dc), &info, DIB_RGB_COLORS, &mut bits, None, 0)
                {
                    self.back_bitmap = bitmap;
                    self.back_old = SelectObject(self.back_dc, bitmap.into());
                    self.back_w = w;
                    self.back_h = h;
                }
            }
        }
    }

    fn paint(&mut self) {
        let t0 = Instant::now();
        unsafe {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(self.hwnd, &mut ps);
            let mut rect = RECT::default();
            let _ = GetClientRect(self.hwnd, &mut rect);

            let w = (rect.right - rect.left).max(1);
            let h = (rect.bottom - rect.top).max(1);
            self.ensure_backbuffer(w, h);
            let target = if self.back_w == w && self.back_h == h {
                self.back_dc
            } else {
                hdc
            };

            // 先整块刷黑（黑边），再等比把画面贴进去，最后一次性 BitBlt，
            // 中间过程用户看不见，不会闪。
            let brush = CreateSolidBrush(COLORREF(0));
            let _ = FillRect(target, &rect, brush);
            let _ = DeleteObject(brush.into());

            if self.has_frame && !self.video.is_empty() && self.width > 0 && self.height > 0 {
                let src_ratio = self.width as f32 / self.height as f32;
                let dst_ratio = w as f32 / h as f32;
                let (dw, dh) = if dst_ratio > src_ratio {
                    ((h as f32 * src_ratio) as i32, h)
                } else {
                    (w, (w as f32 / src_ratio) as i32)
                };
                let dx = (w - dw) / 2;
                let dy = (h - dh) / 2;

                let info = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                        biWidth: self.width as i32,
                        biHeight: -(self.height as i32),
                        biPlanes: 1,
                        biBitCount: 32,
                        biCompression: BI_RGB.0,
                        ..Default::default()
                    },
                    ..Default::default()
                };

                let _ = StretchDIBits(
                    target,
                    dx,
                    dy,
                    dw,
                    dh,
                    0,
                    0,
                    self.width as i32,
                    self.height as i32,
                    Some(self.video.as_ptr() as *const _),
                    &info,
                    DIB_RGB_COLORS,
                    SRCCOPY,
                );
            }

            if target != hdc {
                let _ = BitBlt(hdc, 0, 0, w, h, Some(target), 0, 0, SRCCOPY);
            }
            let _ = EndPaint(self.hwnd, &ps);
        }
        self.paint_us += t0.elapsed().as_micros() as u64;
        self.paint_n += 1;
    }
}

/// 把光标位图按 alpha 叠到目标 BGRA 上。
fn composite_cursor(
    dst: &mut [u8],
    width: u32,
    height: u32,
    shape: &CursorShape,
    pos_x: i32,
    pos_y: i32,
) {
    let sw = shape.width as i32;
    let sh = shape.height as i32;
    let origin_x = pos_x - shape.hot_x as i32;
    let origin_y = pos_y - shape.hot_y as i32;

    for y in 0..sh {
        let dy = origin_y + y;
        if dy < 0 || dy >= height as i32 {
            continue;
        }
        for x in 0..sw {
            let dx = origin_x + x;
            if dx < 0 || dx >= width as i32 {
                continue;
            }
            let si = ((y * sw + x) * 4) as usize;
            if si + 3 >= shape.pixels.len() {
                continue;
            }
            let a = shape.pixels[si + 3] as u32;
            if a == 0 {
                continue;
            }
            let di = ((dy as u32 * width + dx as u32) * 4) as usize;
            if di + 3 >= dst.len() {
                continue;
            }
            if a == 255 {
                dst[di] = shape.pixels[si];
                dst[di + 1] = shape.pixels[si + 1];
                dst[di + 2] = shape.pixels[si + 2];
            } else {
                let inv = 255 - a;
                for c in 0..3 {
                    dst[di + c] = ((shape.pixels[si + c] as u32 * a
                        + dst[di + c] as u32 * inv)
                        / 255) as u8;
                }
            }
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        unsafe {
            if !self.back_dc.is_invalid() {
                SelectObject(self.back_dc, self.back_old);
                let _ = DeleteObject(self.back_bitmap.into());
                let _ = DeleteDC(self.back_dc);
            }
        }
    }
}

pub fn now_us() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

unsafe extern "system" fn wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut App;
    match msg {
        WM_PAINT => {
            if !ptr.is_null() {
                unsafe { (*ptr).paint() };
                return LRESULT(0);
            }
        }
        WM_SIZE => {
            if !ptr.is_null() {
                let _ = unsafe { IsWindowVisible(hwnd) };
            }
            return LRESULT(0);
        }
        WM_ERASEBKGND => {
            // 背景由 paint 自己负责（双缓冲），让系统再刷一遍会闪。
            return LRESULT(1);
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            return LRESULT(0);
        }
        _ => {}
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// 起一个消息循环，每轮把控制权交给 `on_tick`（在那里收包、解码）。
pub fn run_message_loop(app: &mut App, mut on_tick: impl FnMut(&mut App)) -> Result<()> {
    unsafe {
        let mut msg = MSG::default();
        loop {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == 0x0012 {
                    // WM_QUIT
                    return Ok(());
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            on_tick(app);
            app.tick();
        }
    }
}

pub fn destroy_window(hwnd: HWND) {
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
}
