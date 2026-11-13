//! D3D11 设备、适配器与输出枚举。
//!
//! 桌面复制要求复制器与输出在同一块适配器上，所以枚举和建设备要连着做：
//! 先找到目标输出挂在哪个适配器，再拿那块适配器建 D3D11 设备。

use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device,
    ID3D11DeviceContext,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_NOT_FOUND, DXGI_OUTPUT_DESC, IDXGIAdapter, IDXGIAdapter1,
    IDXGIFactory1, IDXGIOutput,
};
use windows::core::Interface;

/// 一个可复制的输出（显示器）。
pub struct OutputInfo {
    pub adapter_index: u32,
    pub output_index: u32,
    pub adapter: IDXGIAdapter1,
    pub output: IDXGIOutput,
    pub desc: DXGI_OUTPUT_DESC,
    pub width: u32,
    pub height: u32,
}

impl OutputInfo {
    /// 形如 `\\.\DISPLAY1 1920x1080 @ (0,0)`。
    pub fn label(&self) -> String {
        let r = self.desc.DesktopCoordinates;
        let name = String::from_utf16_lossy(
            &self.desc.DeviceName[..self
                .desc
                .DeviceName
                .iter()
                .position(|c| *c == 0)
                .unwrap_or(self.desc.DeviceName.len())],
        );
        format!(
            "{name} {}x{} @ ({},{}){}",
            self.width,
            self.height,
            r.left,
            r.top,
            if self.desc.AttachedToDesktop.as_bool() {
                ""
            } else {
                " [未连接]"
            }
        )
    }
}

/// 枚举所有适配器上的所有输出。
///
/// 顺序稳定：适配器按索引，输出按索引，和 `--output N` 的编号一一对应。
pub fn enum_outputs() -> Result<Vec<OutputInfo>> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.context("创建 DXGI 工厂失败")?;

    let mut out = Vec::new();
    for adapter_index in 0.. {
        let adapter = match unsafe { factory.EnumAdapters1(adapter_index) } {
            Ok(a) => a,
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(e) => return Err(e).context("枚举适配器失败"),
        };

        for output_index in 0.. {
            let output = match unsafe { adapter.EnumOutputs(output_index) } {
                Ok(o) => o,
                Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(e) => return Err(e).context("枚举输出失败"),
            };
            let desc = unsafe { output.GetDesc() }.context("读取输出描述失败")?;
            let r = desc.DesktopCoordinates;
            out.push(OutputInfo {
                adapter_index,
                output_index,
                adapter: adapter.clone(),
                output,
                desc,
                width: (r.right - r.left).max(0) as u32,
                height: (r.bottom - r.top).max(0) as u32,
            });
        }
    }

    if out.is_empty() {
        bail!("没有找到任何显示器输出");
    }
    Ok(out)
}

/// 在指定适配器上建 D3D11 设备。
///
/// `BGRA_SUPPORT` 是桌面复制与 GDI 互操作的前提，必须开。
pub fn create_device(adapter: &IDXGIAdapter1) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let adapter_base: IDXGIAdapter = adapter.cast().context("适配器类型转换失败")?;
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    let mut level = D3D_FEATURE_LEVEL_11_0;

    // 传了适配器就必须用 UNKNOWN，传 HARDWARE 会直接 E_INVALIDARG。
    let mut result = unsafe {
        D3D11CreateDevice(
            Some(&adapter_base),
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            Some(&mut level),
            Some(&mut context),
        )
    };

    // 有些老驱动不认 11_1 这一档，会返回 E_INVALIDARG，退一档再来。
    if let Err(first) = &result {
        tracing::debug!(code = %first.code(), "第一次 D3D11CreateDevice 失败，降级重试");
        result = unsafe {
            D3D11CreateDevice(
                Some(&adapter_base),
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut level),
                Some(&mut context),
            )
        };
    }
    result.context("创建 D3D11 设备失败")?;

    let device = device.context("D3D11 设备为空")?;
    let context = context.context("D3D11 上下文为空")?;
    tracing::info!(?level, "D3D11 设备就绪");
    Ok((device, context))
}
