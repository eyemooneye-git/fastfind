//! The application icon, drawn at runtime so the title bar, taskbar and the
//! embedded exe resource (`assets/fastfind.ico`) all show the same artwork.

use eframe::egui;

const BLUE_TOP: (f32, f32, f32) = (74.0, 144.0, 250.0);
const BLUE_BOTTOM: (f32, f32, f32) = (29.0, 78.0, 216.0);

fn seg_distance(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let vx = bx - ax;
    let vy = by - ay;
    let wx = px - ax;
    let wy = py - ay;
    let len2 = vx * vx + vy * vy;
    let t = if len2 > 0.0 {
        ((wx * vx + wy * vy) / len2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let dx = wx - vx * t;
    let dy = wy - vy * t;
    (dx * dx + dy * dy).sqrt()
}

/// One sample in unit coordinates: straight RGBA, alpha 0 outside the badge.
fn sample(px: f32, py: f32) -> (f32, f32, f32, f32) {
    // Rounded square occupying 0.03..0.97 with a 0.22 corner radius.
    let half = 0.47;
    let corner = 0.22;
    let qx = (px - 0.5).abs() - (half - corner);
    let qy = (py - 0.5).abs() - (half - corner);
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    let d = outside + qx.max(qy).min(0.0) - corner;
    if d > 0.0 {
        return (0.0, 0.0, 0.0, 0.0);
    }

    // Magnifier ring plus the diagonal handle (round caps, like the .ico).
    let (cx, cy) = (0.44, 0.41);
    let rad = 0.225;
    let half_stroke = 0.095 * 0.5;
    let dx = px - cx;
    let dy = py - cy;
    let dr = (dx * dx + dy * dy).sqrt();
    let ring = (dr - rad).abs() <= half_stroke;
    let handle = seg_distance(
        px,
        py,
        cx + rad * std::f32::consts::FRAC_1_SQRT_2,
        cy + rad * std::f32::consts::FRAC_1_SQRT_2,
        0.775,
        0.755,
    ) <= half_stroke;
    if ring || handle {
        return (1.0, 1.0, 1.0, 1.0);
    }

    let t = ((py - 0.03) / 0.94).clamp(0.0, 1.0);
    (
        (BLUE_TOP.0 + (BLUE_BOTTOM.0 - BLUE_TOP.0) * t) / 255.0,
        (BLUE_TOP.1 + (BLUE_BOTTOM.1 - BLUE_TOP.1) * t) / 255.0,
        (BLUE_TOP.2 + (BLUE_BOTTOM.2 - BLUE_TOP.2) * t) / 255.0,
        1.0,
    )
}

pub fn app_icon() -> egui::IconData {
    let size = 64usize;
    let ss = 8usize; // 8x8 supersampling for smooth curves
    let mut rgba = vec![0u8; size * size * 4];
    for y in 0..size {
        for x in 0..size {
            let (mut cr, mut cg, mut cb, mut ca) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
            for sy in 0..ss {
                for sx in 0..ss {
                    let px = (x as f32 + (sx as f32 + 0.5) / ss as f32) / size as f32;
                    let py = (y as f32 + (sy as f32 + 0.5) / ss as f32) / size as f32;
                    let (r, g, b, a) = sample(px, py);
                    cr += r * a;
                    cg += g * a;
                    cb += b * a;
                    ca += a;
                }
            }
            let (r, g, b) = if ca > 0.0 {
                (cr / ca, cg / ca, cb / ca)
            } else {
                (0.0, 0.0, 0.0)
            };
            let i = (y * size + x) * 4;
            // `sample` returns straight (non-premultiplied) 0..=1 components.
            rgba[i] = (r * 255.0).round().clamp(0.0, 255.0) as u8;
            rgba[i + 1] = (g * 255.0).round().clamp(0.0, 255.0) as u8;
            rgba[i + 2] = (b * 255.0).round().clamp(0.0, 255.0) as u8;
            rgba[i + 3] = (ca / (ss * ss) as f32 * 255.0).round() as u8;
        }
    }
    egui::IconData {
        rgba,
        width: size as u32,
        height: size as u32,
    }
}

/// Attach the exe's icon resource to the window via `WM_SETICON`.
///
/// eframe only sets the icon through `GetActiveWindow`, which can silently do
/// nothing depending on focus. Going through the real HWND and the embedded
/// `fastfind.ico` guarantees the title bar, taskbar and Explorer all show the
/// exact same artwork.
pub fn app_hwnd(frame: &eframe::Frame) -> Option<*mut core::ffi::c_void> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let handle = frame.window_handle().ok()?;
    let RawWindowHandle::Win32(win32) = handle.as_raw() else {
        return None;
    };
    Some(win32.hwnd.get() as *mut core::ffi::c_void)
}

pub fn install_window_icon(frame: &eframe::Frame) -> bool {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, LoadImageW, SendMessageW, ICON_BIG, ICON_SMALL, IMAGE_ICON,
        LR_DEFAULTCOLOR, SM_CXICON, SM_CXSMICON, WM_GETICON, WM_SETICON,
    };

    let Ok(handle) = frame.window_handle() else {
        return false;
    };
    let RawWindowHandle::Win32(win32) = handle.as_raw() else {
        return false;
    };
    let hwnd = win32.hwnd.get() as *mut core::ffi::c_void;

    unsafe {
        let module = GetModuleHandleW(std::ptr::null());
        if module.is_null() {
            return false;
        }
        // Icon group ID from app.rc; LoadImage picks the best matching size.
        let name = 1 as *const u16;
        let mut ok = true;
        for (which, metric, fallback) in [
            (ICON_BIG, SM_CXICON, 32),
            (ICON_SMALL, SM_CXSMICON, 16),
        ] {
            let mut size = GetSystemMetrics(metric);
            if size <= 0 {
                size = fallback;
            }
            let icon = LoadImageW(module, name, IMAGE_ICON, size, size, LR_DEFAULTCOLOR);
            if icon.is_null() {
                ok = false;
                continue;
            }
            SendMessageW(hwnd, WM_SETICON, which as usize, icon as isize);
        }
        if ok {
            let applied = SendMessageW(hwnd, WM_GETICON, ICON_BIG as usize, 0);
            crate::flog!("窗口图标已设置 (WM_GETICON=0x{:x})", applied);
        }
        ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The runtime icon used to be all white because `sample` mixed 0..1 and
    /// 0..255 colour components. Keep a check on the actual pixels.
    #[test]
    fn runtime_icon_has_colour() {
        let icon = app_icon();
        let w = icon.width as usize;
        let px = |x: usize, y: usize| {
            let i = (y * w + x) * 4;
            [
                icon.rgba[i],
                icon.rgba[i + 1],
                icon.rgba[i + 2],
                icon.rgba[i + 3],
            ]
        };

        // Middle of the lens: blue background, fully opaque.
        let center = px(32, 32);
        assert_eq!(center[3], 255);
        assert!(center[2] > 150, "background should be blue, got {center:?}");
        assert!(center[0] < 120, "background should not be white, got {center:?}");

        // Top of the magnifier ring: white stroke.
        let ring = px(32, 10);
        assert_eq!(&ring[..3], &[255, 255, 255], "ring should be white: {ring:?}");

        // Rounded corner: transparent.
        assert_eq!(px(1, 1)[3], 0);
    }
}
