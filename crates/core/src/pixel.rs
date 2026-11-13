//! 像素格式转换：BGRA ↔ NV12。
//!
//! 纯算术，没有平台依赖，所以放在核心而不是各端各写一份。主机用它把桌面
//! 转成编码器能吃的 NV12，客户端用它把解码结果转回 BGRA 上屏。
//!
//! 系数取 BT.709 有限范围（16~235）。桌面内容是 sRGB，按视频标准压到有限范围，
//! 是编码器的常规做法。

/// BT.709 有限范围，8 位定点。
///
/// ```text
/// Y  = 16  + ( 47R + 157G +  16B + 128) >> 8
/// Cb = 128 + (-26R -  87G + 113B + 128) >> 8
/// Cr = 128 + (113R - 102G -  10B + 128) >> 8
/// ```
#[inline]
fn y_of(r: i32, g: i32, b: i32) -> u8 {
    (((47 * r + 157 * g + 16 * b + 128) >> 8) + 16).clamp(0, 255) as u8
}

#[inline]
fn u_of(r: i32, g: i32, b: i32) -> u8 {
    (((-26 * r - 87 * g + 113 * b + 128) >> 8) + 128).clamp(0, 255) as u8
}

#[inline]
fn v_of(r: i32, g: i32, b: i32) -> u8 {
    (((113 * r - 102 * g - 10 * b + 128) >> 8) + 128).clamp(0, 255) as u8
}

/// 把一张紧密排列的 BGRA 图转成 NV12。
///
/// `out` 需要 `w*h*3/2` 字节。宽高必须是偶数。
pub fn bgra_to_nv12(bgra: &[u8], width: usize, height: usize, out: &mut [u8]) {
    bgra_to_nv12_strided(bgra.as_ptr(), width * 4, width, height, out);
}

/// 同上，但源可以带行距（D3D11 的 `Map` 拿到的 `RowPitch`）。
///
/// # Safety
/// `src` 必须指向至少 `(height-1)*src_stride + width*4` 字节的可读内存。
pub fn bgra_to_nv12_strided(
    src: *const u8,
    src_stride: usize,
    width: usize,
    height: usize,
    out: &mut [u8],
) {
    assert!(width >= 2 && height >= 2, "NV12 要求宽高不小于 2");
    let y_size = width * height;
    let uv_w = width / 2;
    let uv_h = height / 2;
    out[..y_size + uv_w * uv_h * 2].fill(0);

    unsafe {
        // Y 平面：逐行逐像素。
        let dst = out.as_mut_ptr();
        for y in 0..height {
            let row = src.add(y * src_stride) as *const u32;
            for x in 0..width {
                let px = *row.add(x);
                let b = (px & 0xFF) as i32;
                let g = ((px >> 8) & 0xFF) as i32;
                let r = ((px >> 16) & 0xFF) as i32;
                *dst.add(y * width + x) = y_of(r, g, b);
            }
        }

        // UV 平面：每个 2x2 块取样均值，NV12 是 U/V 交织。
        let uvs = out.as_mut_ptr().add(y_size);
        for by in 0..uv_h {
            for bx in 0..uv_w {
                let mut r = 0i32;
                let mut g = 0i32;
                let mut b = 0i32;
                for dy in 0..2 {
                    let row = src.add((by * 2 + dy) * src_stride) as *const u32;
                    for dx in 0..2 {
                        let px = *row.add(bx * 2 + dx);
                        b += (px & 0xFF) as i32;
                        g += ((px >> 8) & 0xFF) as i32;
                        r += ((px >> 16) & 0xFF) as i32;
                    }
                }
                r >>= 2;
                g >>= 2;
                b >>= 2;
                let i = (by * uv_w + bx) * 2;
                *uvs.add(i) = u_of(r, g, b);
                *uvs.add(i + 1) = v_of(r, g, b);
            }
        }
    }
}

/// NV12 → BGRA，客户端上屏用。
///
/// 源按紧密排列处理（行距 = 宽），需要带行距用 [`nv12_to_bgra_strided`]。
pub fn nv12_to_bgra(
    y_plane: &[u8],
    uv_plane: &[u8],
    width: usize,
    height: usize,
    bgra: &mut [u8],
) {
    nv12_to_bgra_strided(y_plane, width, uv_plane, width, width, height, bgra);
}

/// 同上，但 Y / UV 平面可以各自带行距。
///
/// 解码器的输出往往是宏块对齐的尺寸（1080 → 1088）且行尾有填充，
/// 这里按真实行距取样，把可见区域抠出来。
pub fn nv12_to_bgra_strided(
    y_plane: &[u8],
    y_stride: usize,
    uv_plane: &[u8],
    uv_stride: usize,
    width: usize,
    height: usize,
    bgra: &mut [u8],
) {
    let uv_w = width / 2;
    for y in 0..height {
        let y_row = y * y_stride;
        let uv_row = (y / 2) * uv_stride;
        let out_row = y * width * 4;
        for x in 0..width {
            let yy = y_plane[y_row + x] as i32;
            let i = uv_row + (x / 2) * 2;
            let u = uv_plane[i] as i32 - 128;
            let v = uv_plane[i + 1] as i32 - 128;

            // R = 1.164(Y-16) + 1.793(V-128)
            // G = 1.164(Y-16) - 0.213(U-128) - 0.533(V-128)
            // B = 1.164(Y-16) + 2.112(U-128)
            let c = yy - 16;
            let r = (298 * c + 459 * v + 128) >> 8;
            let g = (298 * c - 55 * u - 136 * v + 128) >> 8;
            let b = (298 * c + 541 * u + 128) >> 8;

            let o = out_row + x * 4;
            bgra[o] = b.clamp(0, 255) as u8;
            bgra[o + 1] = g.clamp(0, 255) as u8;
            bgra[o + 2] = r.clamp(0, 255) as u8;
            bgra[o + 3] = 0xFF;
        }
    }
    let _ = uv_w;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 纯白与纯黑的亮度正确() {
        let white = [255u8, 255, 255, 255];
        let black = [0u8, 0, 0, 255];
        let mut out = vec![0u8; 4 * 4 + 2 * 2 * 2];
        bgra_to_nv12(&white.repeat(16), 4, 4, &mut out);
        assert!(out[0] >= 234 && out[0] <= 236, "白: {}", out[0]);

        bgra_to_nv12(&black.repeat(16), 4, 4, &mut out);
        assert_eq!(out[0], 16);

        let grey = [128u8, 128, 128, 255];
        bgra_to_nv12(&grey.repeat(16), 4, 4, &mut out);
        assert!((out[16] as i32 - 128).abs() <= 1);
        assert!((out[17] as i32 - 128).abs() <= 1);
    }

    #[test]
    fn 来回转换误差不大() {
        // 用平滑渐变：4:2:0 的色度子采样本身有损，高频彩块本来就会偏，
        // 拿它当基准没意义。真实画面是缓变的，这里就按缓变测。
        let (w, h) = (16, 16);
        let mut bgra = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 4;
                bgra[i] = (64 + x * 2) as u8;
                bgra[i + 1] = (64 + y * 2) as u8;
                bgra[i + 2] = 96;
                bgra[i + 3] = 255;
            }
        }
        let mut nv12 = vec![0u8; w * h * 3 / 2];
        bgra_to_nv12(&bgra, w, h, &mut nv12);

        let (y, uv) = nv12.split_at(w * h);
        let mut back = vec![0u8; w * h * 4];
        nv12_to_bgra(y, uv, w, h, &mut back);

        for i in 0..w * h {
            for c in 0..3 {
                let a = bgra[i * 4 + c] as i32;
                let b = back[i * 4 + c] as i32;
                assert!((a - b).abs() < 6, "像素 {i} 通道 {c}: {a} vs {b}");
            }
        }
    }

    #[test]
    fn 带行距的源转换正确() {
        let (w, h) = (4, 4);
        let stride = w * 4 + 16;
        let mut src = vec![0u8; stride * h];
        for y in 0..h {
            for x in 0..w {
                let o = y * stride + x * 4;
                src[o] = 255;
                src[o + 1] = 255;
                src[o + 2] = 255;
                src[o + 3] = 255;
            }
        }
        let mut out = vec![0u8; w * h * 3 / 2];
        bgra_to_nv12_strided(src.as_ptr(), stride, w, h, &mut out);
        assert!(out[0] >= 234);
    }
}
