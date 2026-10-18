//! 守护进程：键盘监听、规则监控、超时触发与 socket 控制。

use crate::config::{self, Config, Rule};
use crate::input;
use crate::keycodes;
use crate::protocol::{Request, Response};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::io::RawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// 收到 SIGINT/SIGTERM 后置位，主循环据此退出。
pub static STOP: AtomicBool = AtomicBool::new(false);

/// 干运行模式：所有命令只写入日志，不真正执行。
static DRY_RUN: AtomicBool = AtomicBool::new(false);

/// 设置干运行模式，需在守护进程开始处理事件前调用。
pub fn set_dry_run(enabled: bool) {
    DRY_RUN.store(enabled, Ordering::SeqCst);
}

pub fn is_dry_run() -> bool {
    DRY_RUN.load(Ordering::SeqCst)
}

const MONITOR_INTERVAL: Duration = Duration::from_millis(500);
const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// 一次按键捕获最长等待时间。
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(20);

extern "C" fn handle_signal(_signum: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

fn install_signal_handlers() {
    unsafe {
        libc::signal(
            libc::SIGINT,
            handle_signal as extern "C" fn(libc::c_int) as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGTERM,
            handle_signal as extern "C" fn(libc::c_int) as libc::sighandler_t,
        );
        // 向已关闭的 socket/管道写入时不要让进程崩溃。
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
}

/// 写入守护进程日志（`~/.config/dmsw/dmsw.log`），失败时静默忽略。
pub fn log_line(message: &str) {
    // 单元测试不写入用户真实配置目录。
    if cfg!(test) {
        return;
    }
    let dir = config::config_dir();
    let _ = fs::create_dir_all(&dir);
    let path = dir.join("dmsw.log");
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&path)
    {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        let _ = writeln!(file, "[{timestamp}] {message}");
    }
}

#[derive(Debug, Clone, Copy)]
enum BindAction {
    Arm,
    Unarm,
    Trigger,
}

struct CompiledBinding {
    keys: BTreeSet<u16>,
    action: BindAction,
    rule: Option<String>,
    fired: bool,
}

enum RuleKind {
    KeyTimeout { key: u16, interval: Duration },
    KeyHold,
    Process { process: String },
}

struct CompiledRule {
    name: String,
    kind: RuleKind,
    command: String,
}

struct State {
    config: Config,
    rules: Vec<CompiledRule>,
    bindings: Vec<CompiledBinding>,
    held: HashSet<u16>,
    /// 规则名 -> 最近一次按键时间，用于 key_timeout。
    last_keypress: HashMap<String, Instant>,
    /// 规则名 -> 上一次观察时进程是否缺失，用于边沿触发。
    process_missing: HashMap<String, bool>,
    /// 按键码 -> key_hold 规则名。
    rule_hold: HashMap<u16, String>,
    /// 正在捕获组合键；捕获期间暂停规则监控。
    capturing: bool,
}

pub struct Daemon {
    state: Mutex<State>,
    capture: Mutex<CaptureSession>,
    capture_done: Condvar,
    /// 同一时间只允许一个客户端捕获按键。
    capture_serial: Mutex<()>,
    /// 守护进程启动时打开的键盘数量；0 表示无可用键盘。
    keyboards: AtomicUsize,
}

struct CaptureSession {
    active: bool,
    state: input::CaptureState,
}

impl Daemon {
    fn new(config: Config) -> Arc<Self> {
        let state = State {
            config,
            rules: Vec::new(),
            bindings: Vec::new(),
            held: HashSet::new(),
            last_keypress: HashMap::new(),
            process_missing: HashMap::new(),
            rule_hold: HashMap::new(),
            capturing: false,
        };
        let daemon = Arc::new(Daemon {
            state: Mutex::new(state),
            capture: Mutex::new(CaptureSession {
                active: false,
                state: input::CaptureState::new(),
            }),
            capture_done: Condvar::new(),
            capture_serial: Mutex::new(()),
            keyboards: AtomicUsize::new(0),
        });
        daemon.rebuild();
        daemon
    }

    fn rebuild(&self) {
        let mut state = self.state.lock().unwrap();
        let config = state.config.clone();
        let (rules, bindings, rule_hold) = compile(&config);

        for rule in &rules {
            if let RuleKind::KeyTimeout { key, interval } = &rule.kind {
                log_line(&format!(
                    "规则 '{}'：{} 秒内未按下键码 {} 则执行 '{}'",
                    rule.name,
                    interval.as_secs_f64(),
                    key,
                    rule.command
                ));
            }
        }
        for bind in &bindings {
            log_line(&format!(
                "绑定：{:?} -> {:?}",
                bind.keys.iter().collect::<Vec<_>>(),
                bind.action
            ));
        }

        state.rules = rules;
        state.bindings = bindings;
        state.rule_hold = rule_hold;
        state.last_keypress.clear();
        state.process_missing.clear();
    }

    fn save(&self) -> Result<(), String> {
        let config = self.state.lock().unwrap().config.clone();
        config::save_config(&config)
    }

    fn set_armed(&self, armed: bool) -> Result<(), String> {
        {
            let mut state = self.state.lock().unwrap();
            if state.config.armed == armed {
                return Ok(());
            }
            state.config.armed = armed;
            state.last_keypress.clear();
            state.process_missing.clear();
        }
        self.save()
    }

    fn command_for_rule(&self, name: &str) -> Option<String> {
        self.state
            .lock()
            .unwrap()
            .config
            .rules
            .get(name)
            .map(|rule| rule.command().to_string())
    }

    fn trigger(&self, name: &str, dry_run: bool) -> Result<String, String> {
        match self.command_for_rule(name) {
            Some(command) => {
                execute_command(&command, dry_run);
                Ok(command)
            }
            None => Err(format!("规则 '{name}' 不存在")),
        }
    }

    fn handle_key_down(&self, code: u16) {
        let mut commands: Vec<String> = Vec::new();
        let mut needs_save = false;

        {
            let mut state = self.state.lock().unwrap();
            if !state.held.insert(code) {
                return; // 自动重复或重复事件
            }

            let timeout_hits: Vec<String> = state
                .rules
                .iter()
                .filter_map(|rule| match &rule.kind {
                    RuleKind::KeyTimeout { key, .. } if *key == code => Some(rule.name.clone()),
                    _ => None,
                })
                .collect();
            for name in timeout_hits {
                state.last_keypress.insert(name, Instant::now());
            }

            let mut fire: Vec<usize> = Vec::new();
            for (index, binding) in state.bindings.iter().enumerate() {
                if !binding.fired
                    && !binding.keys.is_empty()
                    && binding.keys.iter().all(|key| state.held.contains(key))
                {
                    fire.push(index);
                }
            }

            for index in fire {
                if state.bindings[index].fired {
                    continue;
                }
                state.bindings[index].fired = true;
                match state.bindings[index].action {
                    BindAction::Arm => {
                        if !state.config.armed {
                            state.config.armed = true;
                            state.last_keypress.clear();
                            state.process_missing.clear();
                            needs_save = true;
                            log_line("绑定按键触发：系统已预位");
                        }
                    }
                    BindAction::Unarm => {
                        if state.config.armed {
                            state.config.armed = false;
                            state.last_keypress.clear();
                            state.process_missing.clear();
                            needs_save = true;
                            log_line("绑定按键触发：系统已解除预位");
                        }
                    }
                    BindAction::Trigger => {
                        if let Some(rule) = state.bindings[index].rule.clone() {
                            if let Some(command) = state
                                .config
                                .rules
                                .get(&rule)
                                .map(|rule| rule.command().to_string())
                            {
                                commands.push(command);
                            } else {
                                log_line(&format!("绑定引用的规则 '{rule}' 不存在"));
                            }
                        }
                    }
                }
            }
        }

        for command in commands {
            execute_command(&command, false);
        }
        if needs_save {
            if let Err(err) = self.save() {
                log_line(&format!("保存预位状态失败: {err}"));
            }
        }
    }

    fn handle_key_up(&self, code: u16) {
        let mut commands: Vec<String> = Vec::new();

        {
            let mut state = self.state.lock().unwrap();
            state.held.remove(&code);

            if state.config.armed {
                if let Some(name) = state.rule_hold.get(&code) {
                    if let Some(command) = state
                        .config
                        .rules
                        .get(name)
                        .map(|rule| rule.command().to_string())
                    {
                        log_line(&format!("按键保持规则 '{name}' 触发"));
                        commands.push(command);
                    }
                }
            }

            // 组合键被打破时允许下次重新触发。
            let held = state.held.clone();
            for binding in state.bindings.iter_mut() {
                if binding.fired && !binding.keys.iter().all(|key| held.contains(key)) {
                    binding.fired = false;
                }
            }
        }

        for command in commands {
            execute_command(&command, false);
        }
    }

    fn monitor_tick(&self) {
        let now = Instant::now();
        let mut commands: Vec<String> = Vec::new();

        {
            let mut state = self.state.lock().unwrap();
            // 捕获按键期间暂停规则监控，避免测试绑定组合时误触发规则。
            if !state.config.armed || state.capturing {
                return;
            }

            let timeouts: Vec<(String, Duration, String)> = state
                .rules
                .iter()
                .filter_map(|rule| match &rule.kind {
                    RuleKind::KeyTimeout { interval, .. } => {
                        Some((rule.name.clone(), *interval, rule.command.clone()))
                    }
                    _ => None,
                })
                .collect();

            for (name, interval, command) in timeouts {
                match state.last_keypress.get(&name).copied() {
                    None => {
                        state.last_keypress.insert(name, now);
                    }
                    Some(last) => {
                        if now.duration_since(last) > interval {
                            log_line(&format!("规则 '{name}' 超时，执行命令"));
                            commands.push(command);
                            state.last_keypress.insert(name, now);
                        }
                    }
                }
            }

            let processes: Vec<(String, String, String)> = state
                .rules
                .iter()
                .filter_map(|rule| match &rule.kind {
                    RuleKind::Process { process } => {
                        Some((rule.name.clone(), process.clone(), rule.command.clone()))
                    }
                    _ => None,
                })
                .collect();

            for (name, process, command) in processes {
                let running = is_process_running(&process);
                let was_missing = state.process_missing.get(&name).copied().unwrap_or(false);
                if !running && !was_missing {
                    log_line(&format!("关键进程 '{process}' 不存在，触发规则 '{name}'"));
                    commands.push(command);
                }
                state.process_missing.insert(name, !running);
            }
        }

        for command in commands {
            execute_command(&command, false);
        }
    }

    /// 把按键事件送入捕获会话；返回 true 表示事件已被捕获逻辑消费。
    fn capture_event(&self, code: u16, pressed: bool) -> bool {
        let mut capture = self.capture.lock().unwrap();
        if !capture.active {
            return false;
        }

        if pressed {
            capture.state.press(code);
        } else {
            capture.state.release(code);
        }

        if capture.state.is_finished() {
            capture.active = false;
            self.capture_done.notify_all();
        }
        true
    }

    /// 等待用户按下一组组合键并返回规范化按键名。
    fn handle_capture(&self) -> Response {
        self.capture_with_timeout(CAPTURE_TIMEOUT)
    }

    fn capture_with_timeout(&self, timeout: Duration) -> Response {
        if self.keyboards.load(Ordering::SeqCst) == 0 {
            return Response::error(
                "没有可用的键盘设备，请检查权限（通常需要加入 input 组）后重启守护进程。",
            );
        }

        let _serial = match self.capture_serial.try_lock() {
            Ok(guard) => guard,
            Err(_) => return Response::error("已有按键捕获正在进行，请稍后再试。"),
        };

        // 清空按键状态，确保捕获从干净状态开始，并避免旧组合触发绑定。
        {
            let mut state = self.state.lock().unwrap();
            state.capturing = true;
            state.held.clear();
            for binding in &mut state.bindings {
                binding.fired = false;
            }
        }
        {
            let mut capture = self.capture.lock().unwrap();
            capture.active = true;
            capture.state = input::CaptureState::new();
        }
        log_line("开始捕获按键组合");

        let deadline = Instant::now() + timeout;
        let mut capture = self.capture.lock().unwrap();
        while capture.active {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let (guard, _) = self
                .capture_done
                .wait_timeout(capture, deadline.saturating_duration_since(now))
                .unwrap();
            capture = guard;
        }
        capture.active = false;
        let codes = capture.state.peak_codes();
        drop(capture);

        // 捕获期间规则监控暂停，恢复后重新计时，避免超时立刻误触发。
        {
            let mut state = self.state.lock().unwrap();
            state.capturing = false;
            state.last_keypress.clear();
        }

        if codes.is_empty() {
            log_line("按键捕获超时，未捕获到按键");
            return Response::error(format!(
                "等待按键超时（{} 秒），未捕获到任何按键。",
                timeout.as_secs()
            ));
        }

        let keys: Vec<String> = codes
            .iter()
            .map(|code| {
                keycodes::canonical_name(*code)
                    .map(str::to_owned)
                    .unwrap_or_else(|| code.to_string())
            })
            .collect();
        log_line(&format!("捕获到组合键: {}", keys.join("+")));
        Response::ok_with_keys(keys)
    }

    fn handle_request(&self, request: &Request) -> Response {
        match request.command.as_str() {
            "reload" => match config::load_config() {
                Ok(config) => {
                    {
                        let mut state = self.state.lock().unwrap();
                        state.config = config;
                    }
                    self.rebuild();
                    log_line("配置已重新加载");
                    Response::ok()
                }
                Err(err) => Response::error(err),
            },
            "arm" => match self.set_armed(true) {
                Ok(()) => {
                    log_line("系统已预位");
                    Response::ok()
                }
                Err(err) => Response::error(err),
            },
            "unarm" => match self.set_armed(false) {
                Ok(()) => {
                    log_line("系统已解除预位");
                    Response::ok()
                }
                Err(err) => Response::error(err),
            },
            "capture" => self.handle_capture(),
            "trigger" => match &request.rule {
                Some(rule) => match self.trigger(rule, request.dry_run) {
                    Ok(command) => {
                        if request.dry_run || is_dry_run() {
                            Response::ok_with_message(format!(
                                "[干运行] 规则 '{rule}' 将执行命令: {command}"
                            ))
                        } else {
                            Response::ok()
                        }
                    }
                    Err(err) => Response::error(err),
                },
                None => Response::error("缺少规则名"),
            },
            "stop" => {
                STOP.store(true, Ordering::SeqCst);
                Response::ok()
            }
            other => Response::error(format!("未知命令: {other}")),
        }
    }

    fn serve_client(&self, mut stream: UnixStream) {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));

        let mut buffer = [0u8; 4096];
        let size = match stream.read(&mut buffer) {
            Ok(0) => return,
            Ok(size) => size,
            Err(err) => {
                log_line(&format!("读取客户端请求失败: {err}"));
                return;
            }
        };

        let response = match serde_json::from_slice::<Request>(&buffer[..size]) {
            Ok(request) => self.handle_request(&request),
            Err(err) => Response::error(format!("无效的请求: {err}")),
        };

        let payload = serde_json::to_vec(&response)
            .unwrap_or_else(|_| br#"{"status":"error","msg":"serialization failed"}"#.to_vec());
        let _ = stream.write_all(&payload);
        let _ = stream.flush();
    }
}

fn compile(
    config: &Config,
) -> (
    Vec<CompiledRule>,
    Vec<CompiledBinding>,
    HashMap<u16, String>,
) {
    let mut rules = Vec::new();
    let mut bindings = Vec::new();
    let mut rule_hold = HashMap::new();

    for (name, rule) in &config.rules {
        match rule {
            Rule::KeyTimeout {
                interval,
                key,
                key_id,
                command,
            } => {
                if !interval.is_finite() || *interval <= 0.0 || *interval > 100_000_000.0 {
                    log_line(&format!("忽略规则 '{name}'：无效的时间间隔 {interval}"));
                    continue;
                }
                let code = match config::resolve_key(key, *key_id) {
                    Ok(code) => code,
                    Err(err) => {
                        log_line(&format!("忽略规则 '{name}'：{err}"));
                        continue;
                    }
                };
                rules.push(CompiledRule {
                    name: name.clone(),
                    kind: RuleKind::KeyTimeout {
                        key: code,
                        interval: Duration::from_secs_f64(*interval),
                    },
                    command: command.clone(),
                });
            }
            Rule::KeyHold {
                key,
                key_id,
                command,
            } => {
                let code = match config::resolve_key(key, *key_id) {
                    Ok(code) => code,
                    Err(err) => {
                        log_line(&format!("忽略规则 '{name}'：{err}"));
                        continue;
                    }
                };
                rule_hold.insert(code, name.clone());
                rules.push(CompiledRule {
                    name: name.clone(),
                    kind: RuleKind::KeyHold,
                    command: command.clone(),
                });
            }
            Rule::Process {
                process_name,
                command,
            } => {
                if process_name.trim().is_empty() {
                    log_line(&format!("忽略规则 '{name}'：进程名为空"));
                    continue;
                }
                rules.push(CompiledRule {
                    name: name.clone(),
                    kind: RuleKind::Process {
                        process: process_name.clone(),
                    },
                    command: command.clone(),
                });
            }
        }
    }

    for binding in &config.bindings {
        let mut keys = BTreeSet::new();
        let mut valid = true;
        for key in &binding.keys {
            match config::resolve_key(key, false) {
                Ok(code) => {
                    keys.insert(code);
                }
                Err(err) => {
                    log_line(&format!("忽略绑定 {:?}：{err}", binding.keys));
                    valid = false;
                    break;
                }
            }
        }
        if !valid || keys.is_empty() {
            continue;
        }

        let (action, rule) = match binding.action.as_str() {
            "arm" => (BindAction::Arm, None),
            "unarm" => (BindAction::Unarm, None),
            "trigger" => match &binding.rule {
                Some(rule) => (BindAction::Trigger, Some(rule.clone())),
                None => {
                    log_line(&format!(
                        "忽略绑定 {:?}：action 为 trigger 但缺少 rule",
                        binding.keys
                    ));
                    continue;
                }
            },
            other => {
                log_line(&format!("忽略绑定 {:?}：未知动作 '{other}'", binding.keys));
                continue;
            }
        };

        bindings.push(CompiledBinding {
            keys,
            action,
            rule,
            fired: false,
        });
    }

    (rules, bindings, rule_hold)
}

fn is_process_running(name: &str) -> bool {
    let entries = match fs::read_dir("/proc") {
        Ok(entries) => entries,
        Err(_) => return false,
    };

    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        if !file_name.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        if let Ok(comm) = fs::read_to_string(format!("/proc/{file_name}/comm")) {
            if comm.trim() == name {
                return true;
            }
        }
    }
    false
}

fn execute_command(command: &str, force_dry_run: bool) {
    if force_dry_run || is_dry_run() {
        log_line(&format!("[干运行] 将执行命令: {command}"));
        return;
    }
    log_line(&format!("执行命令: {command}"));

    match Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            // 单独线程回收子进程，避免僵尸进程。
            thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(err) => log_line(&format!("命令执行失败: {err}")),
    }
}

fn remove_file_if_exists(path: &str) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => log_line(&format!("无法删除 {path}: {err}")),
    }
}

/// 启动守护进程。成功进入主循环前会向 `ready_fd` 写入 `ok\n`。
pub fn run(ready_fd: RawFd) -> Result<(), String> {
    install_signal_handlers();
    let _ = fs::create_dir_all(config::config_dir());
    let _ = fs::set_permissions(config::config_dir(), fs::Permissions::from_mode(0o700));

    let mut config = config::load_config()?;

    // 操作系统重新启动后自动解除预位，避免“触发关机 -> 重启 -> 再次触发”
    // 的循环。同一 boot 内 systemd 重启守护进程不会解除预位。
    let current_boot = config::current_boot_id();
    let saved_boot = config::saved_boot_id();
    let mut boot_disarmed = false;
    let mut disarm_saved = false;
    if config::should_disarm_on_boot(config.armed, current_boot.as_deref(), saved_boot.as_deref()) {
        config.armed = false;
        boot_disarmed = true;
        match config::save_config(&config) {
            Ok(()) => {
                disarm_saved = true;
                log_line("检测到系统启动，已自动解除预位。");
            }
            Err(err) => log_line(&format!(
                "检测到系统启动，但保存解除预位状态失败: {err}；本次运行保持未预位。"
            )),
        }
    }
    if let Some(boot_id) = current_boot.as_deref() {
        // 配置保存失败时不记录 boot ID，让下次启动继续尝试解除预位。
        if !boot_disarmed || disarm_saved {
            if let Err(err) = config::save_boot_id(boot_id) {
                log_line(&format!("无法记录当前 boot ID: {err}"));
            }
        }
    } else {
        log_line("无法读取内核 boot ID，跳过启动自动解除预位检查。");
    }

    let daemon = Daemon::new(config);

    let socket_path = config::socket_path();
    if Path::new(&socket_path).exists() {
        remove_file_if_exists(&socket_path);
    }

    let listener = UnixListener::bind(&socket_path)
        .map_err(|err| format!("无法绑定 socket {socket_path}: {err}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|err| format!("无法设置 socket 非阻塞: {err}"))?;
    let _ = fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600));

    let pid_path = config::pid_file();
    fs::write(&pid_path, std::process::id().to_string())
        .map_err(|err| format!("无法写入 PID 文件 {pid_path}: {err}"))?;
    let _ = fs::set_permissions(&pid_path, fs::Permissions::from_mode(0o600));

    let devices = input::enumerate_devices();
    daemon.keyboards.store(devices.len(), Ordering::SeqCst);
    if devices.is_empty() {
        log_line("未找到任何键盘设备，请检查权限（可能需要 input 组）。");
    }
    for device in devices {
        log_line(&format!(
            "监听键盘: {} ({})",
            device.name,
            device.path.display()
        ));
        let daemon = daemon.clone();
        let mut file = device.file;
        thread::spawn(move || loop {
            if STOP.load(Ordering::SeqCst) {
                break;
            }
            match input::read_event(&mut file) {
                Ok(Some(event)) => {
                    if event.type_ != input::EV_KEY {
                        continue;
                    }
                    match event.value {
                        1 if !daemon.capture_event(event.code, true) => {
                            daemon.handle_key_down(event.code);
                        }
                        0 if !daemon.capture_event(event.code, false) => {
                            daemon.handle_key_up(event.code);
                        }
                        _ => {} // 2 = 自动重复，或已被捕获逻辑消费
                    }
                }
                Ok(None) => break,
                Err(err) => {
                    log_line(&format!("读取输入事件失败: {err}"));
                    break;
                }
            }
        });
    }

    if is_dry_run() {
        log_line(&format!("守护进程已启动（干运行模式），监听 {socket_path}"));
        log_line("干运行：所有规则照常评估，但命令只会记录，不会执行。");
    } else {
        log_line(&format!("守护进程已启动，监听 {socket_path}"));
    }
    let _ = write_ready(ready_fd);

    let mut last_monitor = Instant::now();
    while !STOP.load(Ordering::SeqCst) {
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    // 每个请求单独处理：capture 可能等待用户按键数十秒，
                    // 不能阻塞 accept 循环（否则 stop/monitor 都会停摆）。
                    let daemon = daemon.clone();
                    thread::spawn(move || daemon.serve_client(stream));
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(err) => {
                    log_line(&format!("socket 接受连接失败: {err}"));
                    break;
                }
            }
        }

        if last_monitor.elapsed() >= MONITOR_INTERVAL {
            daemon.monitor_tick();
            last_monitor = Instant::now();
        }

        thread::sleep(POLL_INTERVAL);
    }

    remove_file_if_exists(&socket_path);
    remove_file_if_exists(&pid_path);
    log_line("守护进程已退出");
    Ok(())
}

fn write_ready(fd: RawFd) -> std::io::Result<()> {
    let message = b"ok\n";
    unsafe {
        let written = libc::write(fd, message.as_ptr().cast::<libc::c_void>(), message.len());
        libc::close(fd);
        if written == message.len() as isize {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
}

/// 在 fork 之后重定向标准输入/输出/错误到 `/dev/null`。
pub fn detach_stdio() {
    unsafe {
        let fd = libc::open(b"/dev/null\0".as_ptr().cast::<libc::c_char>(), libc::O_RDWR);
        if fd >= 0 {
            libc::dup2(fd, libc::STDIN_FILENO);
            libc::dup2(fd, libc::STDOUT_FILENO);
            libc::dup2(fd, libc::STDERR_FILENO);
            if fd > libc::STDERR_FILENO {
                libc::close(fd);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Binding;

    #[test]
    fn compile_resolves_rules_and_bindings() {
        let mut config = Config::default();
        config.rules.insert(
            "timeout".to_string(),
            Rule::KeyTimeout {
                interval: 30.0,
                key: "space".to_string(),
                key_id: false,
                command: "systemctl poweroff".to_string(),
            },
        );
        config.rules.insert(
            "hold".to_string(),
            Rule::KeyHold {
                key: "leftctrl".to_string(),
                key_id: false,
                command: "sysrq -c".to_string(),
            },
        );
        config.rules.insert(
            "missing".to_string(),
            Rule::Process {
                process_name: "firefox".to_string(),
                command: "systemctl reboot".to_string(),
            },
        );
        config.bindings.push(Binding {
            keys: vec!["ctrl".to_string(), "a".to_string()],
            action: "arm".to_string(),
            rule: None,
        });

        let (rules, bindings, holds) = compile(&config);
        assert_eq!(rules.len(), 3);
        assert_eq!(holds.get(&29).map(String::as_str), Some("hold"));
        assert_eq!(bindings.len(), 1);
        assert!(bindings[0].keys.contains(&29));
        assert!(bindings[0].keys.contains(&30));

        let timeout = rules.iter().find(|rule| rule.name == "timeout").unwrap();
        assert_eq!(timeout.command, "systemctl poweroff");
        assert!(matches!(timeout.kind, RuleKind::KeyTimeout { key: 57, .. }));
    }

    #[test]
    fn dry_run_does_not_execute_commands() {
        let path = std::env::temp_dir().join(format!("dmsw-dry-run-{}", std::process::id()));
        let _ = fs::remove_file(&path);
        execute_command(&format!("touch {}", path.display()), true);
        assert!(!path.exists());
    }

    #[test]
    fn dry_run_trigger_reports_command_without_executing() {
        let mut config = Config::default();
        config.rules.insert(
            "poweroff".to_string(),
            Rule::Process {
                process_name: "definitely-not-running".to_string(),
                command: "systemctl poweroff".to_string(),
            },
        );
        let daemon = Daemon::new(config);

        let response =
            daemon.handle_request(&Request::with_rule("trigger", "poweroff").dry_run(true));
        assert!(response.is_ok());
        assert!(response
            .msg
            .as_deref()
            .unwrap_or_default()
            .contains("systemctl poweroff"));
    }

    #[test]
    fn binding_fires_once_and_resets_when_combo_breaks() {
        let config = Config {
            armed: false,
            rules: Default::default(),
            bindings: vec![Binding {
                keys: vec!["ctrl".to_string(), "a".to_string()],
                action: "trigger".to_string(),
                rule: Some("missing".to_string()),
            }],
        };
        let daemon = Daemon::new(config);

        daemon.handle_key_down(29);
        assert!(!daemon.state.lock().unwrap().bindings[0].fired);
        daemon.handle_key_down(30);
        assert!(daemon.state.lock().unwrap().bindings[0].fired);

        // 自动重复不会重复触发。
        daemon.handle_key_down(30);
        assert!(daemon.state.lock().unwrap().bindings[0].fired);

        daemon.handle_key_up(30);
        assert!(!daemon.state.lock().unwrap().bindings[0].fired);
        daemon.handle_key_up(29);
    }

    #[test]
    fn capture_returns_pressed_combination() {
        let daemon = Daemon::new(Config::default());
        daemon.keyboards.store(1, Ordering::SeqCst);

        let worker = daemon.clone();
        let handle = thread::spawn(move || worker.capture_with_timeout(Duration::from_secs(2)));

        // 等待捕获会话建立后再送入按键事件。
        for _ in 0..100 {
            if daemon.capture.lock().unwrap().active {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(daemon.capture.lock().unwrap().active);

        assert!(daemon.capture_event(29, true));
        assert!(daemon.capture_event(56, true));
        assert!(daemon.capture_event(45, true));
        assert!(daemon.capture_event(45, false));
        assert!(daemon.capture_event(56, false));
        assert!(daemon.capture_event(29, false));

        let response = handle.join().unwrap();
        assert!(response.is_ok());
        assert_eq!(response.keys, vec!["KEY_LEFTCTRL", "KEY_LEFTALT", "KEY_X"]);
    }

    #[test]
    fn capture_ignores_events_when_not_active() {
        let daemon = Daemon::new(Config::default());
        assert!(!daemon.capture_event(29, true));
    }

    #[test]
    fn capture_timeout_resets_monitoring_state() {
        let daemon = Daemon::new(Config::default());
        daemon.keyboards.store(1, Ordering::SeqCst);

        let response = daemon.capture_with_timeout(Duration::from_millis(50));
        assert!(!response.is_ok());
        assert!(response
            .msg
            .as_deref()
            .unwrap_or_default()
            .contains("等待按键超时"));
        assert!(!daemon.state.lock().unwrap().capturing);
    }

    #[test]
    fn capture_without_keyboards_fails_fast() {
        let daemon = Daemon::new(Config::default());
        let response = daemon.capture_with_timeout(Duration::from_secs(5));
        assert!(!response.is_ok());
        assert!(response
            .msg
            .as_deref()
            .unwrap_or_default()
            .contains("没有可用的键盘设备"));
    }
}
