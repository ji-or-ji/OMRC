//! Media Foundation 变换器诊断。
//!
//! 硬件编码器能不能用，取决于厂商驱动把哪些 MFT 注册进来、哪些支持 D3D11。
//! 出问题时先跑这个，比盲猜快。

use anyhow::{Context, Result};
use omrc_core::video::{Codec, EncodeConfig};
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFAttributes, IMFTransform, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG,
    MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT,
    MFT_FRIENDLY_NAME_Attribute, MFT_REGISTER_TYPE_INFO, MFT_TRANSFORM_CLSID_Attribute,
    MFMediaType_Video, MFVideoFormat_H264, MFVideoFormat_NV12, MF_TRANSFORM_ASYNC,
    MF_TRANSFORM_ASYNC_UNLOCK, MFTEnumEx,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::core::Interface;

use crate::encode::{Encoder, EncoderMode, EncoderPreference};

pub fn dump() -> Result<()> {
    for (label, flags) in [
        ("硬件", MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER),
        ("同步", MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER),
        ("全部", MFT_ENUM_FLAG_SORTANDFILTER),
    ] {
        println!("== {label} H.264 编码器 ==");
        let list = enumerate(flags)?;
        if list.is_empty() {
            println!("  （无）");
        }
        for activate in list {
            print_activate(&activate);
        }
    }

    // 实际用哪个、能不能初始化，才是关键。
    println!("== 实际选用 ==");
    let cfg = EncodeConfig {
        codec: Codec::H264,
        width: 1920,
        height: 1080,
        fps: 60,
        bitrate_bps: 20_000_000,
        keyframe_interval: 0,
    };
    match Encoder::new(&cfg, EncoderPreference::Auto, None) {
        Ok(enc) => println!("  初始化成功，模式 {:?}", enc.mode()),
        Err(e) => println!("  初始化失败: {e:#}"),
    }
    let _ = EncoderMode::SyncSoftware;
    Ok(())
}

fn enumerate(flags: MFT_ENUM_FLAG) -> Result<Vec<IMFActivate>> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            Some(&input),
            Some(&output),
            &mut activates,
            &mut count,
        )
    }
    .context("MFTEnumEx 失败")?;

    if count == 0 || activates.is_null() {
        return Ok(Vec::new());
    }
    let list = unsafe { std::slice::from_raw_parts(activates, count as usize) };
    let out = list.iter().filter_map(|a| a.clone()).collect();
    unsafe { CoTaskMemFree(Some(activates as *const _)) };
    Ok(out)
}

fn print_activate(activate: &IMFActivate) {
    let name = unsafe {
        let mut buf = [0u16; 256];
        let mut len = 0u32;
        activate
            .GetString(&MFT_FRIENDLY_NAME_Attribute, &mut buf, Some(&mut len))
            .map(|_| String::from_utf16_lossy(&buf[..len as usize]))
            .unwrap_or_else(|_| "<无名称>".into())
    };

    let set_unlock = unsafe { activate.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) };
    println!("    activate 上设 ASYNC_UNLOCK: {set_unlock:?}");
    let transform: Result<IMFTransform, _> = unsafe { activate.ActivateObject() };
    match transform {
        Ok(t) => {
            let attr_err = t.cast::<IMFAttributes>().err().map(|e| format!("{:?}", e.code()));
            let attrs = t.cast::<IMFAttributes>().ok();
            let is_async = attrs
                .as_ref()
                .and_then(|a| unsafe { a.GetUINT32(&MF_TRANSFORM_ASYNC) }.ok())
                .unwrap_or(0);
            let activate_attrs = activate.cast::<IMFAttributes>().is_ok();
            let event_gen = t
                .cast::<windows::Win32::Media::MediaFoundation::IMFMediaEventGenerator>()
                .is_ok();
            println!(
                "  {name}  异步={}  (transform:IMFAttributes={} err={:?}, activate:IMFAttributes={}, 事件源={})",
                is_async != 0,
                attrs.is_some(),
                attr_err,
                activate_attrs,
                event_gen
            );

            // 另一条路：拿 activate 里的 CLSID 自己 CoCreateInstance，
            // 真身可能比代理多暴露 IMFAttributes。
            if let Ok(clsid) = unsafe { activate.GetGUID(&MFT_TRANSFORM_CLSID_Attribute) } {
                let direct: Result<IMFTransform, _> = unsafe {
                    windows::Win32::System::Com::CoCreateInstance(
                        &clsid,
                        None,
                        windows::Win32::System::Com::CLSCTX_INPROC_SERVER,
                    )
                };
                match direct {
                    Ok(d) => {
                        let d_attrs = d.cast::<IMFAttributes>().is_ok();
                        let d_async = d
                            .cast::<IMFAttributes>()
                            .ok()
                            .and_then(|a| unsafe { a.GetUINT32(&MF_TRANSFORM_ASYNC) }.ok())
                            .unwrap_or(0);
                        println!(
                            "      CLSID={clsid:?} 直接创建: IMFAttributes={} 异步={}",
                            d_attrs,
                            d_async != 0
                        );
                    }
                    Err(e) => println!("      CLSID={clsid:?} 直接创建失败: {e}"),
                }
            }
        }
        Err(e) => println!("  {name}  激活失败: {e}"),
    }
}
