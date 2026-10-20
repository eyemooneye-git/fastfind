//! Minimal two-language UI: Chinese on Chinese Windows, English otherwise.
//!
//! `FASTFIND_LANG=zh|en` overrides the detection, which is handy for
//! screenshots and tests.

use std::sync::OnceLock;

use windows_sys::Win32::Globalization::GetUserDefaultUILanguage;

static ZH: OnceLock<bool> = OnceLock::new();

/// Is the UI in Chinese?
pub fn zh() -> bool {
    *ZH.get_or_init(|| {
        if let Ok(v) = std::env::var("FASTFIND_LANG") {
            match v.trim().to_ascii_lowercase().as_str() {
                "zh" | "cn" | "zh-cn" => return true,
                "en" | "en-us" => return false,
                _ => {}
            }
        }
        // LANGID: primary language in the low 10 bits, 0x04 = Chinese.
        let id = unsafe { GetUserDefaultUILanguage() };
        (id & 0x03ff) == 0x04
    })
}

/// Pick one of two static strings.
pub fn tr(cn: &'static str, en: &'static str) -> &'static str {
    if zh() {
        cn
    } else {
        en
    }
}

pub fn app_title() -> &'static str {
    tr("FastFind - NTFS 快速搜索", "FastFind - Fast NTFS Search")
}

pub fn all_drives() -> &'static str {
    tr("全部磁盘", "All drives")
}

pub fn hits_label(hits: usize, truncated: bool, ms: u128) -> String {
    if zh() {
        let mut s = format!("命中 {} 条", hits);
        if truncated {
            s.push_str("（已截断）");
        }
        s.push_str(&format!(" · {} ms", ms));
        s
    } else {
        let mut s = format!("{hits} matches");
        if truncated {
            s.push_str(" (truncated)");
        }
        s.push_str(&format!(" · {ms} ms"));
        s
    }
}

pub fn scope_label(label: &str) -> String {
    if zh() {
        format!("范围: {label}")
    } else {
        format!("Scope: {label}")
    }
}

pub fn indexing_label(drive: char) -> String {
    if zh() {
        format!("正在索引 {drive}:")
    } else {
        format!("Indexing {drive}:")
    }
}

pub fn volume_line(
    drive: char,
    files: &str,
    dirs: &str,
    size: &str,
    ms: u64,
    skipped: Option<&str>,
) -> String {
    if zh() {
        let mut s = format!(
            "{drive}:  {files} 个文件 / {dirs} 个目录 / {size}  ·  扫描 {ms} ms"
        );
        if let Some(k) = skipped {
            s.push_str(&format!("  ·  跳过 {k} 条"));
        }
        s
    } else {
        let mut s = format!("{drive}:  {files} files / {dirs} folders / {size}  ·  scan {ms} ms");
        if let Some(k) = skipped {
            s.push_str(&format!("  ·  {k} skipped"));
        }
        s
    }
}

pub fn total_line(files: &str, dirs: &str, size: &str) -> String {
    if zh() {
        format!("总计 {files} 个文件 / {dirs} 个目录 / {size}")
    } else {
        format!("Total {files} files / {dirs} folders / {size}")
    }
}

pub fn drive_not_indexed(drive: char) -> String {
    if zh() {
        format!("盘符 {drive}: 未被索引，仅支持本机 NTFS 固定磁盘")
    } else {
        format!("Drive {drive}: is not indexed; only local fixed NTFS drives are supported")
    }
}

pub fn dir_not_in_index(path: &str) -> String {
    if zh() {
        format!("目录不在索引中: {path}")
    } else {
        format!("Folder is not in the index: {path}")
    }
}

pub fn no_fixed_drives() -> &'static str {
    tr("没有找到固定磁盘", "No fixed drives found")
}

pub fn fs_unsupported(fs: &str) -> String {
    if zh() {
        format!("文件系统是 {fs}，暂不支持（只有 NTFS 走 MFT 索引）")
    } else {
        format!("{fs} is not supported (only NTFS volumes are indexed via the MFT)")
    }
}

pub fn pick_folder_title() -> &'static str {
    tr("选择搜索目录", "Choose search folder")
}

pub fn clipboard_failed() -> &'static str {
    tr("无法写入剪贴板", "Could not write to the clipboard")
}

pub fn copy_status(name: &str) -> String {
    if zh() {
        format!("已复制: {name}")
    } else {
        format!("Copied: {name}")
    }
}

pub fn copied_name_status() -> &'static str {
    tr("已复制文件名", "File name copied")
}

pub fn copied_path_status() -> &'static str {
    tr("已复制路径", "Path copied")
}
