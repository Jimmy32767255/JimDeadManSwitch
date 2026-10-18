//! Linux evdev 键盘设备访问。
//!
//! 这里直接使用 `libc` 的 `ioctl`/`read`，不依赖额外的 evdev crate。

use std::collections::BTreeSet;
use std::ffi::CStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const EV_KEY: u16 = 0x01;

const IOC_READ: u32 = 2;

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

/// 已打开的键盘设备。
pub struct Device {
    pub file: File,
    pub path: PathBuf,
    pub name: String,
}

/// 枚举并以只读方式打开所有键盘类输入设备。
///
/// 不调用 `EVIOCGRAB`：守护进程只旁路读取按键，不能独占设备，否则其他程序
/// 将收不到键盘输入。
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

/// 捕获组合键时的状态：记录当前按下的键，以及同时按下数量最多的一组键。
#[derive(Debug, Clone, Default)]
pub struct CaptureState {
    pressed: BTreeSet<u16>,
    peak: BTreeSet<u16>,
    finished: bool,
}

impl CaptureState {
    pub fn new() -> Self {
        Self::default()
    }

    /// 按下按键。自动重复不会改变状态。
    pub fn press(&mut self, code: u16) {
        if self.pressed.insert(code) && self.pressed.len() > self.peak.len() {
            self.peak = self.pressed.clone();
        }
    }

    /// 松开按键；所有键都松开后捕获完成。
    pub fn release(&mut self, code: u16) {
        self.pressed.remove(&code);
        if self.pressed.is_empty() && !self.peak.is_empty() {
            self.finished = true;
        }
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// 捕获到的组合，修饰键在前，其余按键码升序。
    pub fn peak_codes(&self) -> Vec<u16> {
        let mut codes: Vec<u16> = self.peak.iter().copied().collect();
        codes.sort_by_key(|code| (shortcut_rank(*code), *code));
        codes
    }
}

/// 组合键展示顺序：Ctrl、Alt、Shift、Super，其他键在后。
fn shortcut_rank(code: u16) -> u8 {
    match code {
        29 | 97 => 0,   // KEY_LEFTCTRL / KEY_RIGHTCTRL
        56 | 100 => 1,  // KEY_LEFTALT / KEY_RIGHTALT
        42 | 54 => 2,   // KEY_LEFTSHIFT / KEY_RIGHTSHIFT
        125 | 126 => 3, // KEY_LEFTMETA / KEY_RIGHTMETA
        _ => 4,
    }
}

/// 在没有守护进程运行时直接监听键盘，等待用户按下并松开一组组合键。
pub fn capture_keys(timeout: Duration) -> Result<Vec<u16>, String> {
    let mut devices = enumerate_devices();
    if devices.is_empty() {
        return Err(
            "未找到可用的键盘设备，请检查 /dev/input 读取权限（通常需要加入 input 组）。"
                .to_string(),
        );
    }

    let mut poll_fds: Vec<libc::pollfd> = devices
        .iter()
        .map(|device| libc::pollfd {
            fd: device.file.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();

    let mut capture = CaptureState::new();
    let deadline = Instant::now() + timeout;

    while !capture.is_finished() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let timeout_ms = remaining.as_millis().clamp(1, 100) as libc::c_int;

        let ready = unsafe {
            libc::poll(
                poll_fds.as_mut_ptr(),
                poll_fds.len() as libc::nfds_t,
                timeout_ms,
            )
        };
        if ready < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(format!("等待键盘输入失败: {err}"));
        }
        if ready == 0 {
            continue;
        }

        for index in 0..poll_fds.len() {
            if poll_fds[index].fd < 0 {
                continue;
            }
            if poll_fds[index].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                poll_fds[index].fd = -1;
                continue;
            }
            if poll_fds[index].revents & libc::POLLIN == 0 {
                continue;
            }
            match read_event(&mut devices[index].file) {
                Ok(Some(event)) => {
                    if event.type_ != EV_KEY {
                        continue;
                    }
                    match event.value {
                        1 => capture.press(event.code),
                        0 => capture.release(event.code),
                        _ => {}
                    }
                }
                Ok(None) => poll_fds[index].fd = -1,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => poll_fds[index].fd = -1,
            }
            if capture.is_finished() {
                break;
            }
        }
    }

    let codes = capture.peak_codes();
    if codes.is_empty() {
        Err(format!(
            "等待按键超时（{} 秒），未捕获到任何按键。",
            timeout.as_secs()
        ))
    } else {
        Ok(codes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_numbers_match_kernel_uapi() {
        assert_eq!(eviocgname(256), 0x8100_4506);
    }

    #[test]
    fn input_event_has_expected_layout() {
        // 64 位 Linux 上 input_event 为 16 字节 timeval + 8 字节负载。
        #[cfg(target_pointer_width = "64")]
        assert_eq!(std::mem::size_of::<libc::input_event>(), 24);
    }

    #[test]
    fn capture_state_records_largest_simultaneous_set() {
        let mut capture = CaptureState::new();
        capture.press(29); // left ctrl
        capture.press(56); // left alt
        capture.press(45); // x
        assert_eq!(capture.peak_codes(), vec![29, 56, 45]);
        assert!(!capture.is_finished());

        capture.release(45);
        capture.release(56);
        assert!(!capture.is_finished());
        capture.release(29);
        assert!(capture.is_finished());
        assert_eq!(capture.peak_codes(), vec![29, 56, 45]);
    }

    #[test]
    fn capture_state_finishes_on_single_key() {
        let mut capture = CaptureState::new();
        capture.press(30);
        capture.press(30); // 自动重复
        capture.release(30);
        assert!(capture.is_finished());
        assert_eq!(capture.peak_codes(), vec![30]);
    }

    #[test]
    fn capture_state_does_not_merge_sequential_keys() {
        let mut capture = CaptureState::new();
        capture.press(29);
        capture.release(29);
        assert!(capture.is_finished());
        assert_eq!(capture.peak_codes(), vec![29]);
    }
}
