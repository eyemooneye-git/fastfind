//! Small Win32 helpers.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use windows_sys::Win32::Foundation::GetLastError;

/// NUL terminated UTF-16 buffer for the `*W` Win32 APIs.
pub fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

pub fn last_error() -> u32 {
    unsafe { GetLastError() }
}

/// Human readable text for a Win32 error code.
pub fn error_text(code: u32) -> String {
    std::io::Error::from_raw_os_error(code as i32).to_string()
}

pub fn win_fail(what: &str) -> anyhow::Error {
    anyhow::anyhow!("{} 失败: {}", what, error_text(last_error()))
}

pub fn utf16_lossy(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// Append the UTF-8 encoding of little-endian UTF-16 bytes (as stored in MFT
/// FILE_NAME attributes). Never allocates. NUL characters are dropped.
pub fn push_utf16le(out: &mut Vec<u8>, bytes: &[u8]) {
    let mut tmp = [0u8; 4];
    let mut i = 0usize;
    while i + 2 <= bytes.len() {
        let u = u16::from_le_bytes([bytes[i], bytes[i + 1]]);
        i += 2;
        let c = match u {
            0xD800..=0xDBFF => {
                let mut ch = '\u{FFFD}';
                if i + 2 <= bytes.len() {
                    let u2 = u16::from_le_bytes([bytes[i], bytes[i + 1]]);
                    if (0xDC00..=0xDFFF).contains(&u2) {
                        i += 2;
                        let cp = 0x1_0000
                            + (((u as u32 - 0xD800) << 10) | (u2 as u32 - 0xDC00));
                        ch = char::from_u32(cp).unwrap_or('\u{FFFD}');
                    }
                }
                ch
            }
            0xDC00..=0xDFFF => '\u{FFFD}',
            _ => char::from_u32(u as u32).unwrap_or('\u{FFFD}'),
        };
        if c != '\0' {
            out.extend_from_slice(c.encode_utf8(&mut tmp).as_bytes());
        }
    }
}

#[inline]
pub fn ascii_lower(b: u8) -> u8 {
    if b.is_ascii_uppercase() {
        b + 32
    } else {
        b
    }
}

/// Lowercase `s` (ASCII only - enough for Windows case insensitive path lookups).
pub fn ascii_lower_str(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_uppercase() {
                c.to_ascii_lowercase()
            } else {
                c
            }
        })
        .collect()
}

/// Open a directory in Explorer (`C:\Windows` -> that folder).
pub fn open_folder(path: &str) {
    let _ = std::process::Command::new("explorer.exe").arg(path).spawn();
}

/// Put real files on the clipboard as `CF_HDROP`, so Explorer (and other apps)
/// can paste them. `cut` also sets `Preferred DropEffect` to move, which makes
/// Explorer dim the files until they are pasted.
pub fn clipboard_files(paths: &[String], cut: bool) -> anyhow::Result<()> {
    use windows_sys::Win32::Foundation::{GlobalFree, HWND};
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE,
    };
    use windows_sys::Win32::System::Ole::{CF_HDROP, DROPEFFECT_COPY, DROPEFFECT_MOVE};
    use windows_sys::Win32::UI::Shell::DROPFILES;

    if paths.is_empty() {
        return Ok(());
    }

    unsafe {
        // Another app can hold the clipboard briefly; retry a little.
        let mut opened = 0;
        for _ in 0..20 {
            opened = OpenClipboard(std::ptr::null_mut::<core::ffi::c_void>() as HWND);
            if opened != 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        if opened == 0 {
            anyhow::bail!("{}", crate::i18n::clipboard_failed());
        }

        let result = (|| -> anyhow::Result<()> {
            if EmptyClipboard() == 0 {
                anyhow::bail!("{}", crate::i18n::clipboard_failed());
            }

            // CF_HDROP = DROPFILES header + double-NUL terminated UTF-16 list.
            let mut list: Vec<u16> = Vec::new();
            for p in paths {
                list.extend(OsStr::new(p).encode_wide());
                list.push(0);
            }
            list.push(0);

            let header = std::mem::size_of::<DROPFILES>();
            let bytes = header + list.len() * 2;
            let hdrop = GlobalAlloc(GMEM_MOVEABLE, bytes);
            if hdrop.is_null() {
                anyhow::bail!("{}", crate::i18n::clipboard_failed());
            }
            let base = GlobalLock(hdrop);
            if base.is_null() {
                GlobalFree(hdrop);
                anyhow::bail!("{}", crate::i18n::clipboard_failed());
            }
            let df = base as *mut DROPFILES;
            (*df).pFiles = header as u32;
            (*df).pt.x = 0;
            (*df).pt.y = 0;
            (*df).fNC = 0;
            (*df).fWide = 1;
            std::ptr::copy_nonoverlapping(
                list.as_ptr() as *const u8,
                (base as *mut u8).add(header),
                list.len() * 2,
            );
            GlobalUnlock(hdrop);
            if SetClipboardData(u32::from(CF_HDROP), hdrop).is_null() {
                GlobalFree(hdrop);
                anyhow::bail!("{}", crate::i18n::clipboard_failed());
            }

            // Preferred DropEffect tells Explorer whether to copy or move.
            let fmt_name = wide("Preferred DropEffect");
            let fmt = RegisterClipboardFormatW(fmt_name.as_ptr());
            if fmt != 0 {
                let effect = GlobalAlloc(GMEM_MOVEABLE, std::mem::size_of::<u32>());
                if !effect.is_null() {
                    let ptr = GlobalLock(effect) as *mut u32;
                    if ptr.is_null() {
                        GlobalFree(effect);
                    } else {
                        *ptr = if cut { DROPEFFECT_MOVE } else { DROPEFFECT_COPY };
                        GlobalUnlock(effect);
                        if SetClipboardData(fmt, effect).is_null() {
                            GlobalFree(effect);
                        }
                    }
                }
            }
            Ok(())
        })();

        CloseClipboard();
        result
    }
}

/// Put UTF-16 text on the clipboard (`CF_UNICODETEXT`), so any other app can
/// paste it. Uses the Win32 clipboard directly so it cannot silently fall back
/// to an in-process buffer.
pub fn clipboard_text(text: &str) -> anyhow::Result<()> {
    use windows_sys::Win32::Foundation::{GlobalFree, HWND};
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};

    const CF_UNICODETEXT: u32 = 13;

    unsafe {
        let mut opened = 0;
        for _ in 0..20 {
            opened = OpenClipboard(std::ptr::null_mut::<core::ffi::c_void>() as HWND);
            if opened != 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        if opened == 0 {
            anyhow::bail!("{}", crate::i18n::clipboard_failed());
        }

        let result = (|| -> anyhow::Result<()> {
            if EmptyClipboard() == 0 {
                anyhow::bail!("{}", crate::i18n::clipboard_failed());
            }
            let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
            let hmem = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2);
            if hmem.is_null() {
                anyhow::bail!("{}", crate::i18n::clipboard_failed());
            }
            let ptr = GlobalLock(hmem);
            if ptr.is_null() {
                GlobalFree(hmem);
                anyhow::bail!("{}", crate::i18n::clipboard_failed());
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr as *mut u16, wide.len());
            GlobalUnlock(hmem);
            if SetClipboardData(CF_UNICODETEXT, hmem).is_null() {
                GlobalFree(hmem);
                anyhow::bail!("{}", crate::i18n::clipboard_failed());
            }
            Ok(())
        })();

        CloseClipboard();
        result
    }
}

/// Capture the window (or just its client area) into a PNG.
///
/// Used by the hidden `--window-shot` / `--screenshot` debug flags. The pixels
/// are copied off the screen (the caller makes the window topmost first), since
/// `PrintWindow` renders text unreliably for this UI.
pub fn capture_window(
    hwnd: *mut core::ffi::c_void,
    path: &std::path::Path,
    client_only: bool,
) -> anyhow::Result<()> {
    use windows_sys::Win32::Foundation::{HWND, POINT, RECT};
    use windows_sys::Win32::Graphics::Gdi::{
        BitBlt, ClientToScreen, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject,
        GetDC, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CAPTUREBLT,
        DIB_RGB_COLORS, SRCCOPY,
    };
    use windows_sys::Win32::Storage::Xps::{PrintWindow, PW_CLIENTONLY};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetClientRect, GetWindowRect, PW_RENDERFULLCONTENT,
    };

    unsafe {
        let mut rect = RECT::default();
        let mut screen_origin = POINT { x: 0, y: 0 };
        if client_only {
            if GetClientRect(hwnd as HWND, &mut rect) == 0 {
                anyhow::bail!("GetClientRect 失败: {}", error_text(last_error()));
            }
            ClientToScreen(hwnd as HWND, &mut screen_origin);
        } else {
            if GetWindowRect(hwnd as HWND, &mut rect) == 0 {
                anyhow::bail!("GetWindowRect 失败: {}", error_text(last_error()));
            }
            screen_origin = POINT {
                x: rect.left,
                y: rect.top,
            };
            rect.right -= rect.left;
            rect.bottom -= rect.top;
            rect.left = 0;
            rect.top = 0;
        }
        let w = (rect.right - rect.left).max(1) as u32;
        let h = (rect.bottom - rect.top).max(1) as u32;

        let screen = GetDC(std::ptr::null_mut());
        if screen.is_null() {
            anyhow::bail!("GetDC 失败");
        }
        let mem = CreateCompatibleDC(screen);
        let mut bmi = BITMAPINFO::default();
        bmi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w as i32,
            biHeight: -(h as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let dib = CreateDIBSection(
            mem,
            &bmi,
            DIB_RGB_COLORS,
            &mut bits,
            std::ptr::null_mut(),
            0,
        );
        if dib.is_null() || bits.is_null() {
            DeleteDC(mem);
            ReleaseDC(std::ptr::null_mut(), screen);
            anyhow::bail!("CreateDIBSection 失败");
        }
        let old = SelectObject(mem, dib);
        let flags = if client_only {
            PW_CLIENTONLY | PW_RENDERFULLCONTENT
        } else {
            PW_RENDERFULLCONTENT
        };
        let mut ok = BitBlt(
            mem,
            0,
            0,
            w as i32,
            h as i32,
            screen,
            screen_origin.x,
            screen_origin.y,
            SRCCOPY | CAPTUREBLT,
        );
        if ok == 0 {
            ok = PrintWindow(hwnd as HWND, mem, flags);
        }
        SelectObject(mem, old);

        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        if ok != 0 {
            let src = std::slice::from_raw_parts(bits as *const u8, (w * h * 4) as usize);
            for px in src.chunks_exact(4) {
                // 32bpp BI_RGB is BGRA and often has alpha 0; the frame is opaque.
                rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
            }
        }
        DeleteObject(dib);
        DeleteDC(mem);
        ReleaseDC(std::ptr::null_mut(), screen);

        if ok == 0 {
            anyhow::bail!("BitBlt 失败: {}", error_text(last_error()));
        }
        crate::png::write_rgba(path, w as usize, h as usize, &rgba)?;
        Ok(())
    }
}

/// Show the system folder picker. Returns the chosen absolute path.
pub fn pick_folder(title: &str) -> Option<String> {
    use windows_sys::Win32::System::Com::{
        CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_APARTMENTTHREADED,
    };
    use windows_sys::Win32::UI::Shell::{
        SHBrowseForFolderW, SHGetPathFromIDListW, BIF_EDITBOX, BIF_NEWDIALOGSTYLE,
        BIF_RETURNONLYFSDIRS, BROWSEINFOW,
    };
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetActiveWindow;

    unsafe {
        // The shell picker needs COM on this thread; initialising twice is
        // harmless and covers a caller that has not done it yet.
        let hr = CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32);
        let com_ready = hr >= 0;

        let title_w = wide(title);
        let mut display = [0u16; 512];
        let bi = BROWSEINFOW {
            hwndOwner: GetActiveWindow(),
            pidlRoot: std::ptr::null_mut(),
            pszDisplayName: display.as_mut_ptr(),
            lpszTitle: title_w.as_ptr(),
            ulFlags: BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE | BIF_EDITBOX,
            lpfn: None,
            lParam: 0,
            iImage: 0,
        };
        let pidl = SHBrowseForFolderW(&bi);
        let mut picked = None;
        if !pidl.is_null() {
            let mut path = [0u16; 1024];
            if SHGetPathFromIDListW(pidl, path.as_mut_ptr()) != 0 {
                let s = utf16_lossy(&path);
                if !s.is_empty() {
                    picked = Some(s);
                }
            }
            CoTaskMemFree(pidl as *const core::ffi::c_void);
        }
        if com_ready {
            CoUninitialize();
        }
        picked
    }
}

pub fn format_size(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if n < 1024 {        return format!("{} B", n);
    }
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    format!("{:.2} {}", v, UNITS[u])
}

/// `1234567` -> `1,234,567`.
pub fn format_count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// FILETIME (100ns since 1601) -> local time string.
pub fn format_filetime(ft: u64) -> String {
    use windows_sys::Win32::Storage::FileSystem::FileTimeToLocalFileTime;
    use windows_sys::Win32::System::Time::FileTimeToSystemTime;

    let src = windows_sys::Win32::Foundation::FILETIME {
        dwLowDateTime: (ft & 0xFFFF_FFFF) as u32,
        dwHighDateTime: (ft >> 32) as u32,
    };
    let mut local = windows_sys::Win32::Foundation::FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut st = windows_sys::Win32::Foundation::SYSTEMTIME::default();
    unsafe {
        if FileTimeToLocalFileTime(&src, &mut local) == 0
            || FileTimeToSystemTime(&local, &mut st) == 0
        {
            return "-".to_owned();
        }
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, OpenClipboard, RegisterClipboardFormatW,
    };
    use windows_sys::Win32::System::Memory::{GlobalLock, GlobalUnlock};
    use windows_sys::Win32::System::Ole::{CF_HDROP, DROPEFFECT_COPY, DROPEFFECT_MOVE};
    use windows_sys::Win32::UI::Shell::DragQueryFileW;

    /// Tests run in parallel, but the clipboard is a single global resource.
    static CLIPBOARD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Reads back the `CF_HDROP` / `Preferred DropEffect` pair that Explorer uses.
    fn read_back(expected: &str) -> u32 {
        unsafe {
            assert_ne!(
                OpenClipboard(std::ptr::null_mut::<core::ffi::c_void>()),
                0,
                "OpenClipboard"
            );
            let hdrop = GetClipboardData(u32::from(CF_HDROP));
            assert!(!hdrop.is_null(), "CF_HDROP is missing");
            let count = DragQueryFileW(hdrop, u32::MAX, std::ptr::null_mut(), 0);
            assert_eq!(count, 1, "expected exactly one file");
            let mut buf = [0u16; 512];
            let len = DragQueryFileW(hdrop, 0, buf.as_mut_ptr(), buf.len() as u32);
            let got = String::from_utf16_lossy(&buf[..len as usize]);
            assert_eq!(got, expected);

            let name = wide("Preferred DropEffect");
            let fmt = RegisterClipboardFormatW(name.as_ptr());
            assert_ne!(fmt, 0);
            let effect = GetClipboardData(fmt);
            assert!(!effect.is_null(), "Preferred DropEffect is missing");
            let value = *(GlobalLock(effect) as *const u32);
            GlobalUnlock(effect);
            CloseClipboard();
            value
        }
    }

    #[test]
    fn clipboard_text_roundtrip() {
        let _guard = CLIPBOARD_LOCK.lock().unwrap();
        let text = "FastFind 复制测试 ✓";
        clipboard_text(text).unwrap();

        unsafe {
            assert_ne!(
                OpenClipboard(std::ptr::null_mut::<core::ffi::c_void>()),
                0,
                "OpenClipboard"
            );
            let hmem = GetClipboardData(13); // CF_UNICODETEXT
            assert!(!hmem.is_null(), "CF_UNICODETEXT is missing");
            let ptr = GlobalLock(hmem) as *const u16;
            assert!(!ptr.is_null());
            let mut len = 0usize;
            while *ptr.add(len) != 0 {
                len += 1;
            }
            let got = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
            GlobalUnlock(hmem);
            CloseClipboard();
            assert_eq!(got, text);
        }
    }

    #[test]
    fn clipboard_hdrop_roundtrip() {
        let _guard = CLIPBOARD_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join("fastfind-clipboard-test");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("clip test 中文.txt");
        std::fs::write(&file, b"x").unwrap();
        let path = file.to_string_lossy().to_string();

        clipboard_files(std::slice::from_ref(&path), false).unwrap();
        assert_eq!(read_back(&path), DROPEFFECT_COPY);

        clipboard_files(std::slice::from_ref(&path), true).unwrap();
        assert_eq!(read_back(&path), DROPEFFECT_MOVE);

        let _ = std::fs::remove_file(&file);
    }
}
