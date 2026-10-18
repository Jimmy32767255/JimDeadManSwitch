//! 配置模型、路径、按键/时间解析以及守护进程通信。

use crate::keycodes;
use crate::protocol::{Request, Response};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::env;
use std::ffi::{CStr, OsStr};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

pub const VERSION: &str = "2.0.0";

/// 一条监控规则。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Rule {
    /// 在 `interval` 秒内没有按下 `key` 时触发。
    KeyTimeout {
        #[serde(default)]
        interval: f64,
        key: String,
        #[serde(default)]
        key_id: bool,
        command: String,
    },
    /// 释放 `key` 时触发。
    KeyHold {
        key: String,
        #[serde(default)]
        key_id: bool,
        command: String,
    },
    /// `process_name` 不存在时触发。
    Process {
        process_name: String,
        command: String,
    },
}

impl Rule {
    pub fn command(&self) -> &str {
        match self {
            Rule::KeyTimeout { command, .. }
            | Rule::KeyHold { command, .. }
            | Rule::Process { command, .. } => command,
        }
    }
}

/// 组合键绑定。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Binding {
    pub keys: Vec<String>,
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
}

/// 配置文件结构。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub armed: bool,
    #[serde(default)]
    pub rules: BTreeMap<String, Rule>,
    #[serde(default)]
    pub bindings: Vec<Binding>,
}

/// 当前用户 ID。
pub fn uid() -> u32 {
    unsafe { libc::getuid() }
}

/// 用户主目录（优先 `$HOME`，回退到 passwd 数据库）。
pub fn home_dir() -> PathBuf {
    if let Some(home) = env::var_os("HOME") {
        if !home.is_empty() {
            return PathBuf::from(home);
        }
    }

    unsafe {
        let passwd = libc::getpwuid(libc::getuid());
        if !passwd.is_null() && !(*passwd).pw_dir.is_null() {
            let dir = CStr::from_ptr((*passwd).pw_dir);
            return PathBuf::from(OsStr::from_bytes(dir.to_bytes()));
        }
    }

    PathBuf::from("/tmp")
}

pub fn config_dir() -> PathBuf {
    home_dir().join(".config").join("dmsw")
}

pub fn config_file() -> PathBuf {
    config_dir().join("config.json")
}

pub fn socket_path() -> String {
    format!("/tmp/dmsw-{}.sock", uid())
}

pub fn pid_file() -> String {
    format!("/tmp/dmsw-{}.pid", uid())
}

/// 上一次运行守护进程时记录的内核 boot ID。
pub fn boot_id_file() -> PathBuf {
    config_dir().join("boot-id")
}

/// 当前内核的 boot ID；无法读取时返回 `None`。
pub fn current_boot_id() -> Option<String> {
    fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
}

/// 读取上次记录的 boot ID。
pub fn saved_boot_id() -> Option<String> {
    fs::read_to_string(boot_id_file())
        .ok()
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
}

/// 记录当前 boot ID（权限 0600，原子写入）。
pub fn save_boot_id(id: &str) -> Result<(), String> {
    let dir = config_dir();
    fs::create_dir_all(&dir).map_err(|err| format!("无法创建配置目录 {}: {err}", dir.display()))?;
    let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));

    let path = boot_id_file();
    let tmp = dir.join(format!("boot-id.{}.tmp", std::process::id()));
    let result = (|| -> Result<(), String> {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|err| format!("无法写入临时 boot ID 文件 {}: {err}", tmp.display()))?;
        file.write_all(id.as_bytes())
            .and_then(|()| file.write_all(b"\n"))
            .and_then(|()| file.sync_all())
            .map_err(|err| format!("写入 boot ID 文件 {} 失败: {err}", tmp.display()))?;
        fs::rename(&tmp, &path)
            .map_err(|err| format!("无法更新 boot ID 文件 {}: {err}", path.display()))?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// 是否应因“系统重新启动”而解除预位。
///
/// 只有配置处于预位、能读到当前 boot ID、且与上次记录的 boot ID 不同（包括
/// 没有记录）时才返回真。同一 boot 内的守护进程重启不会解除预位。
pub fn should_disarm_on_boot(armed: bool, current: Option<&str>, previous: Option<&str>) -> bool {
    armed && current.is_some() && previous != current
}

/// 读取配置；文件不存在时返回默认配置。
pub fn load_config() -> Result<Config, String> {
    let path = config_file();
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(err) => {
            return Err(format!("无法读取配置文件 {}: {err}", path.display()));
        }
    };

    serde_json::from_str(&text)
        .map_err(|err| format!("配置文件 {} 解析失败: {err}", path.display()))
}

/// 原子地保存配置（先写临时文件再 rename），权限设为 0600。
pub fn save_config(config: &Config) -> Result<(), String> {
    let dir = config_dir();
    fs::create_dir_all(&dir).map_err(|err| format!("无法创建配置目录 {}: {err}", dir.display()))?;
    let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));

    let path = config_file();
    let tmp = dir.join(format!("config.json.{}.tmp", std::process::id()));
    let json =
        serde_json::to_string_pretty(config).map_err(|err| format!("配置序列化失败: {err}"))?;

    let write_result = (|| -> Result<(), String> {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|err| format!("无法写入临时配置文件 {}: {err}", tmp.display()))?;
        file.write_all(json.as_bytes())
            .and_then(|()| file.write_all(b"\n"))
            .and_then(|()| file.sync_all())
            .map_err(|err| format!("写入配置文件 {} 失败: {err}", tmp.display()))?;
        fs::rename(&tmp, &path)
            .map_err(|err| format!("无法更新配置文件 {}: {err}", path.display()))?;
        Ok(())
    })();

    if write_result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    write_result
}

/// 向守护进程发送请求并等待响应。
pub fn notify_daemon(request: &Request) -> Result<Response, String> {
    notify_daemon_timeout(request, Duration::from_secs(2))
}

/// 与 [`notify_daemon`] 相同，但可自定义等待响应的超时（按键捕获需要较长时间）。
pub fn notify_daemon_timeout(request: &Request, timeout: Duration) -> Result<Response, String> {
    let path = socket_path();
    let mut stream = UnixStream::connect(&path).map_err(|err| {
        format!("无法连接到守护进程（{path}）：{err}。请先执行 dmsw daemon --start 启动守护进程。")
    })?;

    stream
        .set_read_timeout(Some(timeout))
        .and_then(|()| stream.set_write_timeout(Some(Duration::from_secs(2))))
        .map_err(|err| format!("无法设置 socket 超时: {err}"))?;

    let payload = serde_json::to_vec(request).map_err(|err| format!("请求序列化失败: {err}"))?;
    stream
        .write_all(&payload)
        .and_then(|()| stream.flush())
        .map_err(|err| format!("发送请求失败: {err}"))?;

    let mut buf = [0u8; 4096];
    let size = stream
        .read(&mut buf)
        .map_err(|err| format!("读取守护进程响应失败: {err}"))?;
    if size == 0 {
        return Ok(Response::ok());
    }

    serde_json::from_slice(&buf[..size]).map_err(|err| format!("守护进程响应无效: {err}"))
}

/// 探测守护进程是否在运行（socket 是否可连接）。
pub fn daemon_available() -> bool {
    UnixStream::connect(socket_path()).is_ok()
}

/// 解析 `30s`、`5m`、`1h`、`2d` 或纯秒数为秒数。
pub fn parse_interval(text: &str) -> Result<f64, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("时间间隔不能为空".to_string());
    }

    let (number, multiplier) = match text.chars().last().unwrap_or(' ') {
        's' | 'S' => (&text[..text.len() - 1], 1.0),
        'm' | 'M' => (&text[..text.len() - 1], 60.0),
        'h' | 'H' => (&text[..text.len() - 1], 3600.0),
        'd' | 'D' => (&text[..text.len() - 1], 86400.0),
        _ => (text, 1.0),
    };

    let value: f64 = number
        .trim()
        .parse()
        .map_err(|_| format!("无效的时间间隔 '{text}'，示例：30s、5m、1h、2d"))?;
    let seconds = value * multiplier;
    if !seconds.is_finite() || seconds <= 0.0 {
        return Err(format!("时间间隔必须大于 0：'{text}'"));
    }

    Ok(seconds)
}

/// 把用户输入的按键名称或按键 ID 解析为 Linux 键码。
///
/// 支持 `space`、`leftctrl`、`KEY_A`、`ctrl` 等名称；当 `key_id` 为真时
/// 只接受 0-65535 的整数。
pub fn resolve_key(input: &str, key_id: bool) -> Result<u16, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("按键不能为空".to_string());
    }

    if key_id {
        return input
            .parse::<u16>()
            .map_err(|_| format!("无效的按键 ID '{input}'，必须是 0-65535 的整数"));
    }

    let upper = input.to_ascii_uppercase();
    if let Some(code) = keycodes::lookup(&upper) {
        return Ok(code);
    }

    let alias = match upper.as_str() {
        "CTRL" | "CONTROL" | "LCTRL" => Some("KEY_LEFTCTRL"),
        "RCTRL" => Some("KEY_RIGHTCTRL"),
        "ALT" | "LALT" | "OPTION" => Some("KEY_LEFTALT"),
        "RALT" => Some("KEY_RIGHTALT"),
        "SHIFT" | "LSHIFT" => Some("KEY_LEFTSHIFT"),
        "RSHIFT" => Some("KEY_RIGHTSHIFT"),
        "SUPER" | "WIN" | "META" | "CMD" | "LWIN" => Some("KEY_LEFTMETA"),
        "RWIN" => Some("KEY_RIGHTMETA"),
        "RETURN" => Some("KEY_ENTER"),
        "ESCAPE" => Some("KEY_ESC"),
        "DEL" => Some("KEY_DELETE"),
        "INS" => Some("KEY_INSERT"),
        "PGUP" => Some("KEY_PAGEUP"),
        "PGDN" => Some("KEY_PAGEDOWN"),
        _ => None,
    };
    if let Some(name) = alias {
        if let Some(code) = keycodes::lookup(name) {
            return Ok(code);
        }
    }

    if let Some(code) = keycodes::lookup(&format!("KEY_{upper}")) {
        return Ok(code);
    }

    // 兼容直接写键码的方式，例如 `dmsw new r -k 30 57 cmd`。
    if let Ok(code) = input.parse::<u16>() {
        return Ok(code);
    }

    Err(format!(
        "无法解析按键 '{input}'，请使用按键名称（如 space、leftctrl、KEY_A）或按键 ID"
    ))
}

/// 解析 `ctrl+alt+x` 形式的组合键，返回规范化的 `KEY_*` 名称列表。
pub fn parse_key_combination(text: &str) -> Result<Vec<String>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("按键组合不能为空".to_string());
    }

    let mut codes: Vec<u16> = Vec::new();
    for part in text.split('+') {
        let part = part.trim();
        if part.is_empty() {
            return Err(format!("无效的按键组合 '{text}'"));
        }
        let code = resolve_key(part, false)?;
        if !codes.contains(&code) {
            codes.push(code);
        }
    }

    Ok(codes
        .into_iter()
        .map(|code| {
            keycodes::canonical_name(code)
                .map(str::to_owned)
                .unwrap_or_else(|| code.to_string())
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intervals_are_parsed() {
        assert_eq!(parse_interval("30s").unwrap(), 30.0);
        assert_eq!(parse_interval("5m").unwrap(), 300.0);
        assert_eq!(parse_interval("1.5h").unwrap(), 5400.0);
        assert_eq!(parse_interval("2d").unwrap(), 172800.0);
        assert_eq!(parse_interval("15").unwrap(), 15.0);
        assert!(parse_interval("0").is_err());
        assert!(parse_interval("-3s").is_err());
        assert!(parse_interval("abc").is_err());
        assert!(parse_interval("").is_err());
    }

    #[test]
    fn keys_are_resolved() {
        assert_eq!(resolve_key("space", false).unwrap(), 57);
        assert_eq!(resolve_key("SPACE", false).unwrap(), 57);
        assert_eq!(resolve_key("ctrl", false).unwrap(), 29);
        assert_eq!(resolve_key("leftctrl", false).unwrap(), 29);
        assert_eq!(resolve_key("KEY_A", false).unwrap(), 30);
        assert_eq!(resolve_key("a", false).unwrap(), 30);
        assert_eq!(resolve_key("5", false).unwrap(), 6); // KEY_5，而不是键码 5
        assert_eq!(resolve_key("57", false).unwrap(), 57); // 未知名称时回退为键码
        assert_eq!(resolve_key("57", true).unwrap(), 57);
        assert!(resolve_key("space", true).is_err());
        assert!(resolve_key("not-a-key", false).is_err());
    }

    #[test]
    fn key_combinations_are_canonicalized() {
        assert_eq!(
            parse_key_combination("ctrl+alt+x").unwrap(),
            vec!["KEY_LEFTCTRL", "KEY_LEFTALT", "KEY_X"]
        );
        assert_eq!(
            parse_key_combination("a+b+a").unwrap(),
            vec!["KEY_A", "KEY_B"]
        );
        assert!(parse_key_combination("ctrl++a").is_err());
        assert!(parse_key_combination("").is_err());
    }

    #[test]
    fn config_json_shape_matches_python_version() {
        let json = r#"{
            "armed": false,
            "rules": {
                "rule1": {
                    "type": "key_timeout",
                    "interval": 30,
                    "key": "space",
                    "key_id": false,
                    "command": "systemctl poweroff"
                },
                "rule2": {
                    "type": "key_hold",
                    "key": "KEY_LEFTCTRL",
                    "key_id": false,
                    "command": "o"
                },
                "rule3": {
                    "type": "process",
                    "process_name": "firefox",
                    "command": "r"
                }
            },
            "bindings": [
                { "keys": ["KEY_LEFTCTRL", "KEY_LEFTALT", "KEY_A"], "action": "arm" }
            ]
        }"#;

        let config: Config = serde_json::from_str(json).unwrap();
        assert!(!config.armed);
        assert_eq!(config.rules.len(), 3);
        assert_eq!(config.bindings.len(), 1);
        assert_eq!(config.rules["rule1"].command(), "systemctl poweroff");

        let round_trip = serde_json::to_value(&config).unwrap();
        assert_eq!(round_trip["rules"]["rule1"]["type"], "key_timeout");
        assert_eq!(round_trip["rules"]["rule1"]["interval"], 30.0);
        assert_eq!(round_trip["bindings"][0]["action"], "arm");
    }

    #[test]
    fn missing_config_is_default() {
        let config: Config = serde_json::from_str("{}").unwrap();
        assert!(!config.armed);
        assert!(config.rules.is_empty());
        assert!(config.bindings.is_empty());
    }

    #[test]
    fn boot_disarm_only_when_armed_and_boot_changed() {
        assert!(should_disarm_on_boot(true, Some("new"), Some("old")));
        assert!(should_disarm_on_boot(true, Some("new"), None));
        assert!(!should_disarm_on_boot(true, Some("same"), Some("same")));
        assert!(!should_disarm_on_boot(false, Some("new"), Some("old")));
        assert!(!should_disarm_on_boot(true, None, Some("old")));
    }
}
