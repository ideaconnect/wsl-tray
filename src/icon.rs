//! Tray icon: a Tux silhouette coloured by state, rendered from an embedded
//! coverage mask (Font Awesome Free "linux" glyph, CC BY 4.0, pre-rasterized
//! by tools/gentux-rs into assets/tux.bin) and scaled to the taskbar's icon size at runtime.

use std::ffi::c_void;
use std::ptr::null_mut;

use windows_sys::Win32::Graphics::Gdi::{
    CreateBitmap, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC,
    BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, HICON, ICONINFO};

use crate::monitor::Status;

/// Load levels (the higher of CPU and memory share of the host):
/// green < 50 %, orange 50-75 %, red > 75 %; gray = WSL2 is off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Off,
    Ok,
    Warn,
    High,
}

impl Level {
    pub fn for_status(st: &Status) -> Level {
        if !st.running {
            return Level::Off;
        }
        let load = st.cpu.unwrap_or(0.0).max(st.mem_pct);
        if load > 75.0 {
            Level::High
        } else if load >= 50.0 {
            Level::Warn
        } else {
            Level::Ok
        }
    }

    fn rgb(self) -> [u8; 3] {
        match self {
            Level::Off => [150, 150, 150],
            Level::Ok => [52, 199, 89],
            Level::Warn => [255, 149, 0],
            Level::High => [255, 59, 48],
        }
    }
}

/// Alpha (0-255) of Tux's belly/face areas, so the silhouette reads as a solid
/// shape instead of a thin outline on dark taskbars.
const HOLLOW_ALPHA: u32 = 110;

/// `assets/tux.bin`: u16 width, u16 height (little endian), then `w*h` bytes
/// of glyph coverage followed by `w*h` bytes marking the enclosed holes.
static TUX: &[u8] = include_bytes!("../assets/tux.bin");

struct Mask {
    w: usize,
    h: usize,
    cov: &'static [u8],
    holes: &'static [u8],
}

fn tux() -> Mask {
    let w = u16::from_le_bytes([TUX[0], TUX[1]]) as usize;
    let h = u16::from_le_bytes([TUX[2], TUX[3]]) as usize;
    let n = w * h;
    assert_eq!(TUX.len(), 4 + 2 * n, "assets/tux.bin is malformed");
    Mask {
        w,
        h,
        cov: &TUX[4..4 + n],
        holes: &TUX[4 + n..4 + 2 * n],
    }
}

/// Paints the icon into `pix` (BGRA premultiplied, `sz*sz` pixels).
pub fn draw_icon(pix: &mut [u8], sz: usize, lv: Level) {
    pix.fill(0);
    let c = lv.rgb();
    let put = |pix: &mut [u8], x: usize, y: usize, a: u32| {
        let i = (y * sz + x) * 4;
        pix[i] = (c[2] as u32 * a / 255) as u8;
        pix[i + 1] = (c[1] as u32 * a / 255) as u8;
        pix[i + 2] = (c[0] as u32 * a / 255) as u8;
        pix[i + 3] = a as u8;
    };

    // Fit the glyph into the square, keeping its aspect ratio, centred.
    let m = tux();
    let mut gh = sz;
    let mut gw = (gh as f64 * m.w as f64 / m.h as f64).round() as usize;
    if gw > sz {
        gw = sz;
        gh = (gw as f64 * m.h as f64 / m.w as f64).round() as usize;
    }
    let cov = resample(m.cov, m.w, m.h, gw, gh);
    let holes = resample(m.holes, m.w, m.h, gw, gh);
    let (x0, y0) = ((sz - gw) / 2, (sz - gh) / 2);
    for y in 0..gh {
        for x in 0..gw {
            let i = y * gw + x;
            let a = (cov[i] as u32).max(holes[i] as u32 * HOLLOW_ALPHA / 255);
            if a > 0 {
                put(pix, x0 + x, y0 + y, a);
            }
        }
    }
}

/// Shrinks a grayscale coverage mask with area averaging, which gives clean
/// antialiased edges at tray sizes.
pub fn resample(src: &[u8], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<u8> {
    let mut dst = vec![0u8; dw * dh];
    let fx = sw as f64 / dw as f64;
    let fy = sh as f64 / dh as f64;
    for y in 0..dh {
        let (sy0, sy1) = (y as f64 * fy, (y + 1) as f64 * fy);
        for x in 0..dw {
            let (sx0, sx1) = (x as f64 * fx, (x + 1) as f64 * fx);
            let (mut sum, mut area) = (0.0, 0.0);
            let mut sy = sy0 as usize;
            while (sy as f64) < sy1 && sy < sh {
                let wy = sy1.min((sy + 1) as f64) - sy0.max(sy as f64);
                let mut sx = sx0 as usize;
                while (sx as f64) < sx1 && sx < sw {
                    let wx = sx1.min((sx + 1) as f64) - sx0.max(sx as f64);
                    sum += src[sy * sw + sx] as f64 * wx * wy;
                    area += wx * wy;
                    sx += 1;
                }
                sy += 1;
            }
            if area > 0.0 {
                dst[y * dw + x] = (sum / area).round() as u8;
            }
        }
    }
    dst
}

/// Builds a premultiplied 32-bpp ARGB image and turns it into an HICON.
/// Returns the icon and, if `keep_pixels`, a copy of the BGRA pixels.
pub fn render_icon(
    sz: usize,
    lv: Level,
    keep_pixels: bool,
) -> Result<(HICON, Option<Vec<u8>>), String> {
    unsafe {
        let hdc_screen = GetDC(null_mut());
        let hdc = CreateCompatibleDC(hdc_screen);
        ReleaseDC(null_mut(), hdc_screen);
        if hdc.is_null() {
            return Err("CreateCompatibleDC failed".into());
        }
        // Everything below releases hdc/hbm/mask on every path.
        let result = (|| {
            let mut bmi: BITMAPINFO = std::mem::zeroed();
            bmi.bmiHeader = BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: sz as i32,
                biHeight: -(sz as i32), // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..std::mem::zeroed()
            };
            let mut bits: *mut c_void = null_mut();
            let hbm = CreateDIBSection(hdc, &bmi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
            if hbm.is_null() || bits.is_null() {
                return Err("CreateDIBSection failed".to_string());
            }
            // SAFETY: the DIB section is sz*sz 32-bpp pixels owned by hbm, which
            // outlives this slice (deleted below after use).
            let pix = std::slice::from_raw_parts_mut(bits as *mut u8, sz * sz * 4);
            draw_icon(pix, sz, lv);
            let copy = keep_pixels.then(|| pix.to_vec());

            let mask = CreateBitmap(sz as i32, sz as i32, 1, 1, null_mut());
            let hicon = if mask.is_null() {
                null_mut()
            } else {
                let ii = ICONINFO {
                    fIcon: 1,
                    xHotspot: 0,
                    yHotspot: 0,
                    hbmMask: mask,
                    hbmColor: hbm,
                };
                let h = CreateIconIndirect(&ii);
                DeleteObject(mask);
                h
            };
            DeleteObject(hbm);
            if hicon.is_null() {
                return Err("CreateIconIndirect failed".to_string());
            }
            Ok((hicon, copy))
        })();
        DeleteDC(hdc);
        result
    }
}

/// Converts premultiplied BGRA to straight-alpha RGBA rows.
pub fn to_rgba(sz: usize, pix: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; sz * sz * 4];
    for (src, dst) in pix
        .as_chunks::<4>()
        .0
        .iter()
        .zip(out.as_chunks_mut::<4>().0)
    {
        let a = src[3] as u32;
        let unpremul = |c: u8| (c as u32 * 255).checked_div(a).unwrap_or(0) as u8;
        dst[0] = unpremul(src[2]);
        dst[1] = unpremul(src[1]);
        dst[2] = unpremul(src[0]);
        dst[3] = src[3];
    }
    out
}

/// Minimal PNG encoder (RGBA, stored/uncompressed deflate) for `--render-test`.
pub fn encode_png(w: usize, h: usize, rgba: &[u8]) -> Vec<u8> {
    fn chunk(out: &mut Vec<u8>, tag: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let start = out.len();
        out.extend_from_slice(tag);
        out.extend_from_slice(data);
        let crc = crc32(&out[start..]);
        out.extend_from_slice(&crc.to_be_bytes());
    }
    // Raw scanlines with filter byte 0.
    let mut raw = Vec::with_capacity(h * (w * 4 + 1));
    for y in 0..h {
        raw.push(0);
        raw.extend_from_slice(&rgba[y * w * 4..(y + 1) * w * 4]);
    }
    // zlib stream with stored blocks.
    let mut z = vec![0x78, 0x01];
    let mut rest = &raw[..];
    loop {
        let n = rest.len().min(65535);
        let last = n == rest.len();
        z.push(last as u8);
        z.extend_from_slice(&(n as u16).to_le_bytes());
        z.extend_from_slice(&(!(n as u16)).to_le_bytes());
        z.extend_from_slice(&rest[..n]);
        rest = &rest[n..];
        if last {
            break;
        }
    }
    z.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut out = vec![0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(w as u32).to_be_bytes());
    ihdr.extend_from_slice(&(h as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit RGBA
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
    }
    !c
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &d in data {
        a = (a + d as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_loads() {
        let m = tux();
        assert!(m.w > 0 && m.h > 0);
        assert!(m.cov.contains(&255));
        assert!(m.holes.contains(&255));
    }

    #[test]
    fn level_thresholds() {
        let st = |cpu: f64, mem: f64| Status {
            running: true,
            cpu: Some(cpu),
            mem_pct: mem,
            ..Default::default()
        };
        assert_eq!(Level::for_status(&Status::default()), Level::Off);
        assert_eq!(Level::for_status(&st(10.0, 10.0)), Level::Ok);
        assert_eq!(Level::for_status(&st(49.9, 0.0)), Level::Ok);
        assert_eq!(Level::for_status(&st(50.0, 0.0)), Level::Warn);
        assert_eq!(Level::for_status(&st(0.0, 75.0)), Level::Warn);
        assert_eq!(Level::for_status(&st(0.0, 75.1)), Level::High);
        assert_eq!(
            Level::for_status(&Status {
                running: true,
                cpu: None,
                mem_pct: 80.0,
                ..Default::default()
            }),
            Level::High
        );
    }

    #[test]
    fn resample_preserves_flat_regions() {
        let src = vec![200u8; 16 * 16];
        let dst = resample(&src, 16, 16, 5, 7);
        assert!(dst.iter().all(|&v| v == 200));
    }

    #[test]
    fn draw_has_pixels_in_every_state() {
        for lv in [Level::Off, Level::Ok, Level::Warn, Level::High] {
            let mut pix = vec![0u8; 24 * 24 * 4];
            draw_icon(&mut pix, 24, lv);
            assert!(
                pix.chunks(4).any(|p| p[3] == 255),
                "{lv:?} has no opaque pixel"
            );
        }
    }

    #[test]
    fn png_roundtrip_header() {
        let png = encode_png(2, 2, &[255; 16]);
        assert_eq!(
            &png[..8],
            &[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n']
        );
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
    }
}
