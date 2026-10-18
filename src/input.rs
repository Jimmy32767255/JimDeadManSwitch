//! Linux evdev 键盘设备访问。
//!
//! 这里直接使用 `libc` 的 `ioctl`/`read`，不依赖额外的 evdev crate。

use std::ffi::CStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

pub const EV_KEY: u16 = 0x01;

const IOC_READ: u32 = 2;
const IOC_WRITE: u32 = 1;

/// Linux `_IOC` 编码。
const fn ioc(dir: u32, ty: u8, nr: u8, size: usize) -> libc::c_ulong {
    (((dir as u64) << 30) | ((size as u64) << 16) | (((ty as u64) << 8) | (nr as u64)))
        as libc::c_ulong
}

const fn eviocgname(size: usize) -> libc::c_ulong {
    ioc(IOC_READ, b'E', 0x06, size)
}

const fn eviocgbit(ev: u8, size: usize) -> libc::c_ulong {
    ioc(IOC_READ, b'E', 0x20 + ev, size)
}

const fn eviocgrab() -> libc::c_ulong {
    ioc(IOC_WRITE, b'E', 0x90, std::mem::size_of::<libc::c_int>())
}

/// 已打开的键盘设备。
pub struct Device {
    pub file: File,
    pub path: PathBuf,
    pub name: String,
}

/// 枚举并打开所有键盘类输入设备。
pub fn enumerate_devices() -> Vec<Device> {
    let mut paths: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = fs::read_dir("/dev/input") {
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let file_name = file_name.to_string_lossy();
            if let Some(suffix) = file_name.strip_prefix("event") {
                if !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit()) {
                    paths.push(entry.path());
                }
            }
        }
    }
    paths.sort();

    let mut devices = Vec::new();
    for path in paths {
        match open_device(&path) {
            Ok(Some(device)) => devices.push(device),
            Ok(None) => {}
            Err(err) => eprintln!("跳过输入设备 {}: {err}", path.display()),
        }
    }
    devices
}

/// 打开一个输入设备；不是键盘时返回 `Ok(None)`。
fn open_device(path: &Path) -> Result<Option<Device>, String> {
    let file = OpenOptions::new()
        .read(true)
        .open(path)
        .map_err(|err| format!("{err}"))?;
    let fd = file.as_raw_fd();

    let name = device_name(fd)?;
    if !has_key_events(fd)? {
        return Ok(None);
    }

    let lower_name = name.to_ascii_lowercase();
    if !lower_name.contains("key") {
        return Ok(None);
    }

    let grab: libc::c_int = 1;
    let result = unsafe { libc::ioctl(fd, eviocgrab(), &grab as *const libc::c_int) };
    if result < 0 {
        eprintln!(
            "警告：无法独占设备 {}（{}），可能有其他程序占用。",
            path.display(),
            io::Error::last_os_error()
        );
    }

    Ok(Some(Device {
        file,
        path: path.to_path_buf(),
        name,
    }))
}

fn device_name(fd: libc::c_int) -> Result<String, String> {
    let mut buffer = [0u8; 256];
    let result = unsafe {
        libc::ioctl(
            fd,
            eviocgname(buffer.len()),
            buffer.as_mut_ptr() as *mut libc::c_char,
        )
    };
    if result < 0 {
        return Err(format!("读取设备名称失败: {}", io::Error::last_os_error()));
    }

    let name = CStr::from_bytes_until_nul(&buffer)
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "<unknown>".to_string());
    Ok(name)
}

fn has_key_events(fd: libc::c_int) -> Result<bool, String> {
    let mut bits = [0u8; 8];
    let result = unsafe {
        libc::ioctl(
            fd,
            eviocgbit(0, bits.len()),
            bits.as_mut_ptr() as *mut libc::c_void,
        )
    };
    if result < 0 {
        return Err(format!("读取设备能力失败: {}", io::Error::last_os_error()));
    }

    let event_type = EV_KEY as usize;
    let byte = event_type / 8;
    let bit = event_type % 8;
    Ok(byte < bits.len() && bits[byte] & (1 << bit) != 0)
}

/// 阻塞读取一个输入事件。设备断开时返回 `Ok(None)`。
pub fn read_event(file: &mut File) -> io::Result<Option<libc::input_event>> {
    let mut event: libc::input_event = unsafe { std::mem::zeroed() };
    let length = std::mem::size_of::<libc::input_event>();
    let buffer = unsafe {
        std::slice::from_raw_parts_mut((&mut event as *mut libc::input_event).cast::<u8>(), length)
    };

    match file.read_exact(buffer) {
        Ok(()) => Ok(Some(event)),
        Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => Ok(None),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_numbers_match_kernel_uapi() {
        assert_eq!(eviocgname(256), 0x8100_4506);
        assert_eq!(eviocgrab(), 0x4004_4590);
    }

    #[test]
    fn input_event_has_expected_layout() {
        // 64 位 Linux 上 input_event 为 16 字节 timeval + 8 字节负载。
        #[cfg(target_pointer_width = "64")]
        assert_eq!(std::mem::size_of::<libc::input_event>(), 24);
    }
}
