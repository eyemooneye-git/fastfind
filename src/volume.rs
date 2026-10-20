//! Raw volume access (`\\.\C:`) and the NTFS volume geometry we need for MFT scanning.

use std::ffi::c_void;

use anyhow::{bail, Context, Result};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW, ReadFile, SetFilePointerEx,
    FILE_ATTRIBUTE_NORMAL, FILE_BEGIN, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::FSCTL_GET_NTFS_VOLUME_DATA;

use crate::winutil::{last_error, utf16_lossy, wide, win_fail};

const GENERIC_READ: u32 = 0x8000_0000;
const DRIVE_FIXED: u32 = 3;

/// Geometry of an NTFS volume, straight from `FSCTL_GET_NTFS_VOLUME_DATA`.
#[derive(Clone, Copy, Debug, Default)]
pub struct NtfsInfo {
    pub bytes_per_cluster: u64,
    pub bytes_per_record: u64,
    /// `MftValidDataLength` - how many bytes of the MFT actually hold records.
    pub mft_valid_bytes: u64,
    pub mft_start_byte: u64,
    pub total_records: u64,
}

pub struct Volume {
    handle: HANDLE,
    pub letter: char,
    pub info: NtfsInfo,
}

// The raw handle is only used from one thread at a time.
unsafe impl Send for Volume {}

impl Drop for Volume {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

/// A fixed disk that could be indexed.
pub struct DriveCandidate {
    pub letter: char,
    pub fs: String,
    pub label: String,
}

/// All fixed local drives, in drive letter order.
pub fn fixed_drives() -> Vec<DriveCandidate> {
    let mask = unsafe { GetLogicalDrives() };
    let mut out = Vec::new();
    let mut label = [0u16; 128];
    let mut fs = [0u16; 32];
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        let root = format!("{}:\\", letter);
        let root_w = wide(&root);
        if unsafe { GetDriveTypeW(root_w.as_ptr()) } != DRIVE_FIXED {
            continue;
        }
        let mut serial = 0u32;
        let mut max_component = 0u32;
        let mut flags = 0u32;
        let ok = unsafe {
            GetVolumeInformationW(
                root_w.as_ptr(),
                label.as_mut_ptr(),
                label.len() as u32,
                &mut serial,
                &mut max_component,
                &mut flags,
                fs.as_mut_ptr(),
                fs.len() as u32,
            )
        };
        if ok == 0 {
            continue;
        }
        out.push(DriveCandidate {
            letter,
            fs: utf16_lossy(&fs),
            label: utf16_lossy(&label),
        });
    }
    out
}

impl Volume {
    pub fn open(letter: char) -> Result<Self> {
        let dev = format!("\\\\.\\{}:", letter);
        let dev_w = wide(&dev);
        let handle = unsafe {
            CreateFileW(
                dev_w.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            let code = last_error();
            if code == 5 {
                bail!("无法打开 {}: 需要管理员权限", dev);
            }
            bail!("无法打开 {}: {}", dev, crate::winutil::error_text(code));
        }
        let mut vol = Self {
            handle,
            letter,
            info: NtfsInfo::default(),
        };
        vol.info = vol.query_ntfs_info().context("读取 NTFS 卷信息失败")?;
        Ok(vol)
    }

    fn query_ntfs_info(&self) -> Result<NtfsInfo> {
        // Output buffer is deliberately generous: the real NTFS_VOLUME_DATA_BUFFER
        // is larger than the fields we care about.
        let mut buf = [0u8; 512];
        let mut returned = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                self.handle,
                FSCTL_GET_NTFS_VOLUME_DATA,
                std::ptr::null(),
                0,
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as u32,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(win_fail("FSCTL_GET_NTFS_VOLUME_DATA"));
        }
        let bytes_per_cluster = u32::from_le_bytes(buf[0x2C..0x30].try_into().unwrap()) as u64;
        let bytes_per_record_field = u32::from_le_bytes(buf[0x30..0x34].try_into().unwrap());
        let clusters_per_record = i32::from_le_bytes(buf[0x34..0x38].try_into().unwrap());
        let mft_valid_bytes = u64::from_le_bytes(buf[0x38..0x40].try_into().unwrap());
        let mft_start_lcn = u64::from_le_bytes(buf[0x40..0x48].try_into().unwrap());

        let bytes_per_record = if bytes_per_record_field != 0 {
            bytes_per_record_field as u64
        } else if clusters_per_record < 0 {
            1u64 << (-clusters_per_record)
        } else {
            bail!("无法确定 MFT 记录大小");
        };
        if bytes_per_cluster == 0 || bytes_per_record == 0 {
            bail!("无效的 NTFS 几何信息");
        }
        let total_records = mft_valid_bytes / bytes_per_record;
        Ok(NtfsInfo {
            bytes_per_cluster,
            bytes_per_record,
            mft_valid_bytes,
            mft_start_byte: mft_start_lcn * bytes_per_cluster,
            total_records,
        })
    }

    /// Positioned read. Offsets and lengths must stay 512 byte aligned, which the
    /// MFT scanner guarantees (records are >= 1024 bytes and chunk sizes are aligned).
    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let mut moved = 0i64;
        let ok = unsafe { SetFilePointerEx(self.handle, offset as i64, &mut moved, FILE_BEGIN) };
        if ok == 0 {
            return Err(win_fail("SetFilePointerEx"));
        }
        let mut done = 0usize;
        while done < buf.len() {
            let want = (buf.len() - done).min(u32::MAX as usize) as u32;
            let mut got = 0u32;
            let ok = unsafe {
                ReadFile(
                    self.handle,
                    buf[done..].as_mut_ptr(),
                    want,
                    &mut got,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(win_fail("ReadFile"));
            }
            if got == 0 {
                bail!("读取卷数据时提前结束 (offset {})", offset + done as u64);
            }
            done += got as usize;
        }
        Ok(())
    }
}
