//! dmsw —— GNU/Linux 死手系统（Dead Man's Switch）命令行入口。

mod config;
mod daemon;
mod input;
mod keycodes;
mod protocol;

use protocol::Request;
use std::env;
use std::fs;
use std::process;
use std::thread;
use std::time::Duration;

const HELP: &str = "\
死手系统 (Dead Man's Switch) for GNU/Linux

用法: dmsw <子命令> [参数]

子命令:
    new     创建一条规则
    del     删除一条规则
    list    列出所有规则
    arm     预位系统（激活所有规则监控）
    unarm   解除预位
    bind    绑定一组按键到规则或预位切换
    trigger 立即触发一条规则
    daemon  控制守护进程 (--start 启动, --stop 停止)

规则类型:
    -k <间隔> [--id] <按键> <命令>   按键超时：间隔内未按该键则执行命令
    -K [--id] <按键> <命令>          按键保持：释放该键时执行命令
    -p <进程名> <命令>               进程监控：进程不存在时执行命令

选项:
    --dry-run  干运行：规则照常评估，但命令只记录/显示，不真正执行
               可用于 daemon --start 或 trigger
    -c, --capture  bind：监听键盘，捕获要绑定的组合键

按键名: space、leftctrl、KEY_A、a 等；加 --id 后使用数字键码。
bind 的 -c/--capture 会监听键盘，把实际按下的组合键保存为绑定。";

fn main() {
    let raw_args: Vec<String> = env::args().skip(1).collect();
    // --dry-run 是全局开关，可写在子命令前后；只对 daemon/trigger 有意义。
    let dry_run = raw_args.iter().any(|arg| arg == "--dry-run");
    let args: Vec<String> = raw_args
        .into_iter()
        .filter(|arg| arg != "--dry-run")
        .collect();

    if args.is_empty() {
        println!("{HELP}");
        process::exit(1);
    }

    if dry_run
        && !matches!(
            args[0].as_str(),
            "daemon" | "trigger" | "-v" | "--version" | "-h" | "--help"
        )
    {
        eprintln!("--dry-run 只能用于 dmsw daemon --start 或 dmsw trigger <规则名>。");
        process::exit(1);
    }

    let result = match args[0].as_str() {
        "-v" | "--version" => {
            println!("dmsw version {}", config::VERSION);
            return;
        }
        "-h" | "--help" => {
            println!("{HELP}");
            return;
        }
        "new" => cmd_new(&args[1..]),
        "del" => cmd_del(&args[1..]),
        "list" => cmd_list(&args[1..]),
        "arm" => cmd_arm(),
        "unarm" => cmd_unarm(),
        "bind" => cmd_bind(&args[1..]),
        "trigger" => cmd_trigger(&args[1..], dry_run),
        "daemon" => cmd_daemon(&args[1..], dry_run),
        other => Err(format!("未知子命令: {other}\n\n{HELP}")),
    };

    if let Err(message) = result {
        eprintln!("{message}");
        process::exit(1);
    }
}

fn cmd_new(args: &[String]) -> Result<(), String> {
    if args.len() < 3 {
        return Err("用法: dmsw new <规则名> <-k|-K|-p> [参数...] <命令>".to_string());
    }

    let name = &args[0];
    let rule_type = &args[1];
    let rule = match rule_type.as_str() {
        "-k" => {
            let mut index = 2;
            let interval_text = args
                .get(index)
                .ok_or("语法: dmsw new <name> -k <时间间隔> [--id] <按键> <命令>")?;
            let interval = config::parse_interval(interval_text)?;
            index += 1;

            let use_id = args.get(index).map(String::as_str) == Some("--id");
            if use_id {
                index += 1;
            }

            let key = args
                .get(index)
                .ok_or("语法: dmsw new <name> -k <时间间隔> [--id] <按键> <命令>")?;
            config::resolve_key(key, use_id)?;
            index += 1;

            let command = join_command(&args[index..])?;
            config::Rule::KeyTimeout {
                interval,
                key: key.clone(),
                key_id: use_id,
                command,
            }
        }
        "-K" => {
            let mut index = 2;
            let use_id = args.get(index).map(String::as_str) == Some("--id");
            if use_id {
                index += 1;
            }

            let key = args
                .get(index)
                .ok_or("语法: dmsw new <name> -K [--id] <按键> <命令>")?;
            config::resolve_key(key, use_id)?;
            index += 1;

            let command = join_command(&args[index..])?;
            config::Rule::KeyHold {
                key: key.clone(),
                key_id: use_id,
                command,
            }
        }
        "-p" => {
            let process_name = args
                .get(2)
                .ok_or("语法: dmsw new <name> -p <进程名> <命令>")?;
            if process_name.trim().is_empty() {
                return Err("进程名不能为空".to_string());
            }
            let command = join_command(&args[3..])?;
            config::Rule::Process {
                process_name: process_name.clone(),
                command,
            }
        }
        other => return Err(format!("未知规则类型: {other}")),
    };

    let mut config = config::load_config()?;
    config.rules.insert(name.clone(), rule);
    config::save_config(&config)?;
    println!("规则 '{name}' 已创建。");
    notify_reload();
    Ok(())
}

fn join_command(parts: &[String]) -> Result<String, String> {
    if parts.is_empty() {
        return Err("缺少要执行的命令".to_string());
    }
    Ok(parts.join(" "))
}

fn cmd_del(args: &[String]) -> Result<(), String> {
    let name = args.first().ok_or("用法: dmsw del <规则名>")?;
    let mut config = config::load_config()?;

    if config.rules.remove(name).is_some() {
        config::save_config(&config)?;
        println!("规则 '{name}' 已删除。");
        notify_reload();
    } else {
        println!("规则 '{name}' 不存在。");
    }
    Ok(())
}

fn cmd_list(args: &[String]) -> Result<(), String> {
    if !args.is_empty() {
        return Err("用法: dmsw list".to_string());
    }

    let config = config::load_config()?;
    let status = if config.armed {
        "已预位"
    } else {
        "未预位"
    };
    if config.rules.is_empty() {
        println!("系统{status}，当前没有规则。使用 dmsw new 创建规则。");
        return Ok(());
    }

    println!("系统{status}，共 {} 条规则：", config.rules.len());
    for (name, rule) in &config.rules {
        println!("  {name}  {}", describe_rule(rule));
    }
    Ok(())
}

fn describe_rule(rule: &config::Rule) -> String {
    match rule {
        config::Rule::KeyTimeout {
            interval,
            key,
            key_id,
            command,
        } => format!(
            "[按键超时] {interval}s 内未按 {} -> {command}",
            key_label(key, *key_id)
        ),
        config::Rule::KeyHold {
            key,
            key_id,
            command,
        } => format!("[按键保持] 释放 {} -> {command}", key_label(key, *key_id)),
        config::Rule::Process {
            process_name,
            command,
        } => format!("[进程监控] 进程 {process_name} 不存在 -> {command}"),
    }
}

fn key_label(key: &str, key_id: bool) -> String {
    if key_id {
        format!("#{key}")
    } else {
        key.to_string()
    }
}

fn cmd_arm() -> Result<(), String> {
    let mut config = config::load_config()?;
    if config.armed {
        record_boot_id_best_effort();
        println!("系统已经在预位状态。");
        return Ok(());
    }

    config.armed = true;
    config::save_config(&config)?;
    record_boot_id_best_effort();
    println!("系统已预位。");
    notify_command("arm");
    Ok(())
}

/// 记录当前 boot ID，供守护进程启动时判断是否发生了系统重启。
fn record_boot_id_best_effort() {
    if let Some(boot_id) = config::current_boot_id() {
        if let Err(err) = config::save_boot_id(&boot_id) {
            eprintln!("提示：无法记录当前 boot ID：{err}");
        }
    }
}

fn cmd_unarm() -> Result<(), String> {
    let mut config = config::load_config()?;
    if !config.armed {
        println!("系统未处于预位状态。");
        return Ok(());
    }

    config.armed = false;
    config::save_config(&config)?;
    println!("系统已解除预位。");
    notify_command("unarm");
    Ok(())
}

fn cmd_bind(args: &[String]) -> Result<(), String> {
    const USAGE: &str = "用法: dmsw bind <动作|规则名> <按键组合>\n\
                         或:   dmsw bind [--capture|-c] <动作|规则名>\n\
                         动作: arm, unarm, 或规则名\n\
                         按键组合示例: a+b 或 ctrl+alt+x (使用 '+' 分隔)\n\
                         选项: -c, --capture  监听键盘并捕获要绑定的组合键";

    let mut capture = false;
    let mut positional: Vec<&String> = Vec::new();
    for arg in args {
        match arg.as_str() {
            "-c" | "--capture" => {
                if capture {
                    return Err(format!("--capture 只能指定一次。\n{USAGE}"));
                }
                capture = true;
            }
            _ => positional.push(arg),
        }
    }

    if capture {
        if positional.len() != 1 {
            return Err(format!("--capture 需要且只需要一个动作或规则名。\n{USAGE}"));
        }
        let action_target = positional[0];
        println!("请按下要绑定的组合键（按住修饰键，全部松开后结束）…");

        let keys = if config::daemon_available() {
            let request = Request::new("capture");
            let response = config::notify_daemon_timeout(&request, Duration::from_secs(30))?;
            if !response.is_ok() {
                return Err(response
                    .msg
                    .unwrap_or_else(|| "守护进程未能捕获按键".to_string()));
            }
            response.keys
        } else {
            eprintln!("提示：未检测到运行中的守护进程，将直接监听键盘。");
            let codes = crate::input::capture_keys(Duration::from_secs(15))?;
            codes
                .into_iter()
                .map(|code| {
                    crate::keycodes::canonical_name(code)
                        .map(str::to_owned)
                        .unwrap_or_else(|| code.to_string())
                })
                .collect()
        };

        if keys.is_empty() {
            return Err("未捕获到任何按键，绑定未添加。".to_string());
        }
        return add_binding(keys, action_target);
    }

    if positional.len() != 2 {
        return Err(USAGE.to_string());
    }

    // 兼容两种书写顺序：
    //   dmsw bind <动作|规则名> <按键组合>
    //   dmsw bind <按键组合> <动作|规则名>
    let first_is_combo = config::parse_key_combination(positional[0]).is_ok();
    let second_is_combo = config::parse_key_combination(positional[1]).is_ok();
    let (action_target, keys_str) = if first_is_combo && !second_is_combo {
        (positional[1].as_str(), positional[0].as_str())
    } else {
        (positional[0].as_str(), positional[1].as_str())
    };

    let key_list = config::parse_key_combination(keys_str)?;
    add_binding(key_list, action_target)
}

fn add_binding(key_list: Vec<String>, action_target: &str) -> Result<(), String> {
    let (action, rule) = match action_target {
        "arm" | "unarm" => (action_target.to_string(), None),
        rule_name => ("trigger".to_string(), Some(rule_name.to_string())),
    };

    let mut config = config::load_config()?;
    if let Some(rule_name) = &rule {
        if !config.rules.contains_key(rule_name) {
            eprintln!("提示：规则 '{rule_name}' 不存在，绑定仍会保存。");
        }
    }
    config.bindings.push(config::Binding {
        keys: key_list.clone(),
        action: action.clone(),
        rule: rule.clone(),
    });
    config::save_config(&config)?;

    let keys_str = key_list.join("+");
    match &rule {
        Some(rule) => println!("绑定已添加: {keys_str} -> {action} {rule}"),
        None => println!("绑定已添加: {keys_str} -> {action}"),
    }
    notify_reload();
    Ok(())
}

fn cmd_trigger(args: &[String], dry_run: bool) -> Result<(), String> {
    let name = args
        .first()
        .ok_or("用法: dmsw trigger <规则名> [--dry-run]")?;
    let request = Request::with_rule("trigger", name).dry_run(dry_run);

    match config::notify_daemon(&request) {
        Ok(response) if response.is_ok() => {
            match response.msg {
                Some(message) => println!("{message}"),
                None => println!("规则 '{name}' 已触发。"),
            }
            Ok(())
        }
        Ok(response) => Err(response.msg.unwrap_or_else(|| "触发失败".to_string())),
        Err(err) if dry_run && err.contains("无法连接到守护进程") => {
            // 守护进程未运行也能预览：直接读取配置并解析出最终命令。
            let config = config::load_config()?;
            let rule = config
                .rules
                .get(name)
                .ok_or_else(|| format!("规则 '{name}' 不存在"))?;
            let command = rule.command();
            eprintln!("提示：{err}");
            println!("[干运行] 规则 '{name}' 将执行命令: {command}");
            Ok(())
        }
        Err(err) => Err(err),
    }
}

fn cmd_daemon(args: &[String], dry_run: bool) -> Result<(), String> {
    match args {
        [flag] if flag == "--start" => start_daemon(dry_run),
        [flag] if flag == "--stop" => {
            if dry_run {
                return Err("--dry-run 不能用于 dmsw daemon --stop。".to_string());
            }
            stop_daemon()
        }
        _ => Err(
            "用法: dmsw daemon <--start|--stop> [--dry-run]\n  --start  启动守护进程\n  --stop   停止守护进程"
                .to_string(),
        ),
    }
}

fn read_pid() -> Option<i32> {
    fs::read_to_string(config::pid_file())
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn process_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    let result = unsafe { libc::kill(pid, 0) };
    if result == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn start_daemon(dry_run: bool) -> Result<(), String> {
    if let Some(pid) = read_pid() {
        if process_alive(pid) {
            if dry_run {
                println!("守护进程已在运行中（--dry-run 只对本次新启动生效，请先停止）。");
            } else {
                println!("守护进程已在运行中。");
            }
            return Ok(());
        }
        let _ = fs::remove_file(config::pid_file());
    }

    let mut pipe_fds = [0i32; 2];
    if unsafe { libc::pipe(pipe_fds.as_mut_ptr()) } != 0 {
        return Err(format!("创建管道失败: {}", std::io::Error::last_os_error()));
    }

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        unsafe {
            libc::close(pipe_fds[0]);
            libc::close(pipe_fds[1]);
        }
        return Err(format!("fork 失败: {}", std::io::Error::last_os_error()));
    }

    if pid > 0 {
        // 父进程：等待子进程报告启动结果。
        unsafe {
            libc::close(pipe_fds[1]);
        }
        let mut buffer = [0u8; 4096];
        let size = unsafe {
            libc::read(
                pipe_fds[0],
                buffer.as_mut_ptr().cast::<libc::c_void>(),
                buffer.len(),
            )
        };
        unsafe {
            libc::close(pipe_fds[0]);
        }

        if size <= 0 {
            return Err("守护进程启动失败（子进程提前退出）。".to_string());
        }
        let message = String::from_utf8_lossy(&buffer[..size as usize]);
        let message = message.trim();
        if message == "ok" {
            if dry_run {
                println!("守护进程已启动（干运行模式），PID: {pid}");
            } else {
                println!("守护进程已启动，PID: {pid}");
            }
            Ok(())
        } else {
            Err(message.to_string())
        }
    } else {
        // 子进程：脱离终端并运行守护进程。
        unsafe {
            libc::close(pipe_fds[0]);
            libc::umask(0o077);
            if libc::setsid() < 0 {
                let message = format!("setsid 失败: {}", std::io::Error::last_os_error());
                write_fd(pipe_fds[1], &message);
                libc::close(pipe_fds[1]);
                libc::_exit(1);
            }
        }
        daemon::detach_stdio();
        daemon::set_dry_run(dry_run);

        let code = match daemon::run(pipe_fds[1]) {
            Ok(()) => 0,
            Err(err) => {
                write_fd(pipe_fds[1], &format!("守护进程启动失败: {err}"));
                1
            }
        };
        unsafe { libc::_exit(code) }
    }
}

fn write_fd(fd: i32, message: &str) {
    let bytes = message.as_bytes();
    unsafe {
        libc::write(fd, bytes.as_ptr().cast::<libc::c_void>(), bytes.len());
    }
}

fn stop_daemon() -> Result<(), String> {
    let pid = read_pid().ok_or("守护进程未运行（PID 文件不存在）。")?;

    if !process_alive(pid) {
        let _ = fs::remove_file(config::pid_file());
        return Err("守护进程未运行（进程不存在）。".to_string());
    }

    let response = config::notify_daemon(&Request::new("stop"))?;
    if !response.is_ok() {
        return Err(response.msg.unwrap_or_else(|| "停止失败".to_string()));
    }

    // 等待守护进程清理 PID 文件，最多 2 秒。
    for _ in 0..100 {
        if read_pid().is_none() && !process_alive(pid) {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }

    println!("守护进程已停止。");
    Ok(())
}

fn notify_reload() {
    notify_command("reload");
}

fn notify_command(command: &str) {
    match config::notify_daemon(&Request::new(command)) {
        Ok(response) => {
            if !response.is_ok() {
                eprintln!(
                    "提示：守护进程返回错误：{}",
                    response.msg.as_deref().unwrap_or("未知错误")
                );
            }
        }
        Err(err) => {
            // 配置已经保存，守护进程下次启动时会读取；这里只提示而不失败。
            eprintln!("提示：{err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Rule;

    #[test]
    fn describes_each_rule_kind() {
        let timeout = Rule::KeyTimeout {
            interval: 30.0,
            key: "space".to_string(),
            key_id: false,
            command: "systemctl poweroff".to_string(),
        };
        assert_eq!(
            describe_rule(&timeout),
            "[按键超时] 30s 内未按 space -> systemctl poweroff"
        );

        let hold = Rule::KeyHold {
            key: "29".to_string(),
            key_id: true,
            command: "systemctl reboot".to_string(),
        };
        assert_eq!(
            describe_rule(&hold),
            "[按键保持] 释放 #29 -> systemctl reboot"
        );

        let process = Rule::Process {
            process_name: "firefox".to_string(),
            command: "systemctl poweroff".to_string(),
        };
        assert_eq!(
            describe_rule(&process),
            "[进程监控] 进程 firefox 不存在 -> systemctl poweroff"
        );
    }
}
