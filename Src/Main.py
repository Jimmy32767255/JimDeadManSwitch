import sys
import os
import json
import time
import signal
import socket
import subprocess
import threading
import argparse
import textwrap
from pathlib import Path

# ------- 常量与路径 -------
VERSION = "1.0.0"
CONFIG_DIR = Path.home() / ".config" / "dmsw"
CONFIG_FILE = CONFIG_DIR / "config.json"
SOCKET_PATH = f"/tmp/dmsw-{os.getuid()}.sock"
PID_FILE = f"/tmp/dmsw-{os.getuid()}.pid"

# ------- 预设计命令映射 -------
PRESET_COMMANDS = {
    "d": "rm -rf /home/$USER/.safebox",
    "s": "bleachbit -s /home/$USER/.safebox",
    "o": "systemctl poweroff",
    "O": "sysrq -o",
    "r": "systemctl reboot",
    "R": "sysrq -b",
    "c": "sysrq -c",
}

def resolve_command(cmd: str) -> str:
    """若是预设缩写则替换为完整命令，否则原样返回。"""
    if cmd in PRESET_COMMANDS:
        return PRESET_COMMANDS[cmd]
    return cmd

# ------- 配置文件管理 -------
def load_config():
    """读取配置文件，没有则返回默认结构。"""
    if CONFIG_FILE.exists():
        with open(CONFIG_FILE, 'r') as f:
            return json.load(f)
    return {"armed": False, "rules": {}, "bindings": []}

def save_config(config):
    """保存配置到文件。"""
    CONFIG_DIR.mkdir(parents=True, exist_ok=True)
    with open(CONFIG_FILE, 'w') as f:
        json.dump(config, f, indent=2)

def notify_daemon(msg: dict):
    """向守护进程发送 JSON 消息。"""
    try:
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        sock.settimeout(2)
        sock.connect(SOCKET_PATH)
        sock.sendall(json.dumps(msg).encode())
        # 接收回应（可选）
        response = sock.recv(4096)
        sock.close()
        return json.loads(response) if response else {}
    except (socket.timeout, ConnectionRefusedError, FileNotFoundError):
        print("无法连接到守护进程。请先执行 dmsw daemon 启动守护进程。", file=sys.stderr)
        sys.exit(1)

# ------- 守护进程核心 -------
# 注意：需要 input 组权限或 root 才能读取 /dev/input/event*
# 安装 evdev 库: pip install evdev
try:
    import evdev
    from evdev import ecodes, InputDevice
except ImportError:
    evdev = None
    InputDevice = None
    ecodes = None

try:
    import psutil
except ImportError:
    psutil = None

class Daemon:
    def __init__(self):
        self.config = load_config()
        self.running = True
        self.keyboard_devices = []
        self.last_keypress = {}  # 规则名 -> 时间戳，用于 -k 超时监控
        self.held_keys = set()   # 当前按下的键（用于绑定/保持监控）
        self.bindings_active = {} # 绑定组合 -> 动作信息，用于触发
        self.rule_hold_map = {}   # 按键保持规则映射：按键码 -> 规则名
        self.lock = threading.Lock()

        signal.signal(signal.SIGINT, self.stop)
        signal.signal(signal.SIGTERM, self.stop)

    def stop(self, signum=None, frame=None):
        self.running = False

    def setup_keyboard(self):
        """扫描并打开所有键盘设备。"""
        if evdev is None:
            print("evdev 库未安装，无法监控键盘。", file=sys.stderr)
            return
        devices = [evdev.InputDevice(path) for path in evdev.list_devices()]
        for dev in devices:
            if ecodes.EV_KEY in dev.capabilities() and "keyboard" in dev.name.lower() or "key" in dev.name.lower():
                try:
                    dev.grab()  # 独占获取，防止输入事件泄露
                except IOError:
                    print(f"警告：无法独占设备 {dev.path}，可能有其他程序占用。", file=sys.stderr)
                self.keyboard_devices.append(dev)
                print(f"监听键盘: {dev.name} ({dev.path})")
        if not self.keyboard_devices:
            print("未找到任何键盘设备，请检查权限（可能需要 input 组）。", file=sys.stderr)

    def build_bindings(self):
        """预处理 bindings，构建内部查找表。"""
        self.bindings_active.clear()
        for bind in self.config.get("bindings", []):
            # 绑定的按键列表（按顺序排序的键码组合，作为触发条件）
            key_codes = frozenset(bind.get("keys", []))
            self.bindings_active[key_codes] = bind

    def build_rule_hold_map(self):
        """构建按键保持规则 map。"""
        self.rule_hold_map.clear()
        for name, rule in self.config.get("rules", {}).items():
            if rule["type"] == "key_hold":
                key = self.resolve_key_code(rule.get("key"), rule.get("key_id"))
                if key is not None:
                    self.rule_hold_map[key] = name

    def resolve_key_code(self, key_str, use_id=False):
        """将用户输入的按键字符串或 ID 转为整数键码。"""
        if use_id:
            return int(key_str)
        # 尝试通过 evdev.ecodes 查找
        if ecodes:
            code = ecodes.ecodes.get(key_str.upper(), None)
            if code:
                return code
            # 尝试作为单字符处理
            if len(key_str) == 1:
                # 使用 evdev 的 keys 查找
                for name, val in ecodes.keys.items():
                    if name.startswith("KEY_") and name[4:].lower() == key_str.lower():
                        return val
        # 未找到则当作原始键码尝试
        try:
            return int(key_str)
        except ValueError:
            print(f"错误: 无法解析按键 '{key_str}'，请安装 evdev 库或使用按键ID", file=sys.stderr)
            return None

    def execute_command(self, command_str):
        """在子进程中执行命令（非阻塞）。"""
        cmd = resolve_command(command_str)
        print(f"执行命令: {cmd}")
        subprocess.Popen(cmd, shell=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    def trigger_rule(self, rule_name):
        """立即触发一条规则。"""
        with self.lock:
            rules = self.config.get("rules", {})
            if rule_name in rules:
                self.execute_command(rules[rule_name]["command"])

    def process_events(self, device):
        """持续从单个设备读取事件。"""
        try:
            for event in device.read_loop():
                if not self.running:
                    break
                if event.type == ecodes.EV_KEY:
                    key_code = event.code
                    if event.value == 1:  # 按下
                        self.handle_key_down(key_code)
                    elif event.value == 0:  # 释放
                        self.handle_key_up(key_code)
        except OSError:
            pass  # 设备断开

    def handle_key_down(self, key_code):
        """按键按下处理。"""
        self.held_keys.add(key_code)

        # 更新所有 key_timeout 规则的最后按键时间
        for name, rule in self.config.get("rules", {}).items():
            if rule["type"] == "key_timeout":
                required_code = self.resolve_key_code(rule["key"], rule.get("key_id"))
                if key_code == required_code:
                    self.last_keypress[name] = time.time()

        # 检查绑定是否需要触发
        self.check_bindings_trigger()

        # key_hold 规则不在此处触发，而在释放时触发

    def handle_key_up(self, key_code):
        """按键释放处理。"""
        # 按键保持规则：如果释放的键所在规则处于武装状态，则执行
        if self.config.get("armed"):
            rule_name = self.rule_hold_map.get(key_code)
            if rule_name:
                self.trigger_rule(rule_name)

        if key_code in self.held_keys:
            self.held_keys.remove(key_code)

    def check_bindings_trigger(self):
        """检查当前按下的组合键是否匹配任何绑定。"""
        current_set = frozenset(self.held_keys)
        for combo, bind in self.bindings_active.items():
            if current_set == combo:
                self.execute_binding_action(bind)
                # 防止重复触发，可限制为只触发一次，直到组合释放
                # 简单起见这里可能多次触发，但正常组合只会同时按一次
                break

    def execute_binding_action(self, bind):
        """执行绑定的动作。"""
        action = bind["action"]
        if action == "arm":
            self.set_armed(True)
        elif action == "unarm":
            self.set_armed(False)
        elif action == "trigger":
            rule = bind.get("rule")
            if rule:
                self.trigger_rule(rule)

    def set_armed(self, state):
        with self.lock:
            self.config["armed"] = state
            save_config(self.config)
            print(f"系统预位状态改变为: {'已预位' if state else '未预位'}")

    def process_monitor(self):
        """守护进程主监控循环：键盘监听、超时检查、进程检查。"""
        # 键盘事件线程
        for dev in self.keyboard_devices:
            t = threading.Thread(target=self.process_events, args=(dev,), daemon=True)
            t.start()

        # 规则监控循环
        while self.running:
            time.sleep(0.5)
            with self.lock:
                config = self.config
                armed = config.get("armed", False)

            if not armed:
                continue

            # 检查 key_timeout 规则
            now = time.time()
            for name, rule in config.get("rules", {}).items():
                if rule["type"] == "key_timeout":
                    last = self.last_keypress.get(name, 0)
                    interval = rule.get("interval", 0)
                    if last == 0:
                        # 初次设置基准时间
                        self.last_keypress[name] = now
                    elif now - last > interval:
                        print(f"规则 {name} 超时，执行命令...")
                        self.trigger_rule(name)
                        # 避免重复触发，重置时间
                        self.last_keypress[name] = now

            # 检查 process 规则
            if psutil:
                for name, rule in config.get("rules", {}).items():
                    if rule["type"] == "process":
                        proc_name = rule.get("process_name")
                        if proc_name and not self.is_process_running(proc_name):
                            print(f"关键进程 {proc_name} 不存在，触发规则 {name}...")
                            self.trigger_rule(name)
            else:
                # 未安装 psutil 时，使用系统方法（会降低性能）
                for name, rule in config.get("rules", {}).items():
                    if rule["type"] == "process":
                        proc_name = rule.get("process_name")
                        if proc_name and not self.is_process_running_fallback(proc_name):
                            print(f"关键进程 {proc_name} 不存在，触发规则 {name}...")
                            self.trigger_rule(name)

    def is_process_running(self, proc_name):
        """使用 psutil 检查进程是否存活。"""
        for proc in psutil.process_iter(['name']):
            if proc.info['name'] == proc_name:
                return True
        return False

    def is_process_running_fallback(self, proc_name):
        """回退方案：通过 /proc 检查进程。"""
        for pid in os.listdir('/proc'):
            if pid.isdigit():
                try:
                    with open(f'/proc/{pid}/comm', 'r') as f:
                        if f.read().strip() == proc_name:
                            return True
                except:
                    continue
        return False

    def socket_listener(self):
        """监听 Unix socket 接受客户端指令。"""
        if os.path.exists(SOCKET_PATH):
            os.unlink(SOCKET_PATH)
        server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        server.bind(SOCKET_PATH)
        server.listen(5)
        server.settimeout(1)
        print(f"守护进程已启动，监听 {SOCKET_PATH}")

        while self.running:
            try:
                client, _ = server.accept()
                data = client.recv(4096)
                if not data:
                    client.close()
                    continue
                msg = json.loads(data.decode())
                response = self.handle_command(msg)
                client.sendall(json.dumps(response).encode())
                client.close()
            except socket.timeout:
                continue
            except Exception as e:
                print(f"socket 错误: {e}")

        server.close()
        if os.path.exists(SOCKET_PATH):
            os.unlink(SOCKET_PATH)

    def handle_command(self, msg):
        """处理来自客户端的命令。"""
        cmd = msg.get("command")
        resp = {"status": "ok"}
        if cmd == "reload":
            self.config = load_config()
            self.build_bindings()
            self.build_rule_hold_map()
            self.last_keypress.clear()
            print("配置已重新加载")
        elif cmd == "arm":
            self.set_armed(True)
        elif cmd == "unarm":
            self.set_armed(False)
        elif cmd == "trigger":
            rule = msg.get("rule")
            if rule:
                self.trigger_rule(rule)
            else:
                resp = {"status": "error", "msg": "缺少规则名"}
        elif cmd == "stop":
            self.running = False
        else:
            resp = {"status": "error", "msg": "未知命令"}
        return resp

    def run(self):
        """启动守护进程所有任务。"""
        self.setup_keyboard()
        self.build_bindings()
        self.build_rule_hold_map()

        # 写入 PID
        with open(PID_FILE, 'w') as f:
            f.write(str(os.getpid()))

        # 启动 socket 线程
        sock_thread = threading.Thread(target=self.socket_listener, daemon=True)
        sock_thread.start()

        # 主循环负责监控
        try:
            self.process_monitor()
        except KeyboardInterrupt:
            pass
        finally:
            if os.path.exists(PID_FILE):
                os.unlink(PID_FILE)
            print("守护进程已退出")

# ------- 命令行解析与客户端操作 -------
def cmd_new(args):
    """创建新规则。"""
    # 手动解析参数：dmsw new <name> <type> [type_args...] <command>
    # 简化解析：假设 args 是一个列表，不包含 'new'
    if len(args) < 3:
        print("用法: dmsw new <规则名> <-k|-K|-p> [参数...] <命令>")
        sys.exit(1)

    name = args[0]
    rule_type = args[1]
    rule = {"type": "", "command": ""}
    idx = 2

    if rule_type == "-k":
        if len(args) < 5:
            print("语法: dmsw new <name> -k <时间间隔> [--id] <按键> <命令>")
            sys.exit(1)
        rule["type"] = "key_timeout"
        interval_str = args[idx]
        rule["interval"] = parse_interval(interval_str)
        idx += 1
        use_id = False
        if args[idx] == "--id":
            use_id = True
            idx += 1
        rule["key"] = args[idx]
        rule["key_id"] = use_id
        idx += 1
        rule["command"] = " ".join(args[idx:])

    elif rule_type == "-K":
        if len(args) < 4:
            print("语法: dmsw new <name> -K [--id] <按键> <命令>")
            sys.exit(1)
        rule["type"] = "key_hold"
        use_id = False
        if args[idx] == "--id":
            use_id = True
            idx += 1
        rule["key"] = args[idx]
        rule["key_id"] = use_id
        idx += 1
        rule["command"] = " ".join(args[idx:])

    elif rule_type == "-p":
        if len(args) < 4:
            print("语法: dmsw new <name> -p <进程名> <命令>")
            sys.exit(1)
        rule["type"] = "process"
        rule["process_name"] = args[idx]
        idx += 1
        rule["command"] = " ".join(args[idx:])
    else:
        print(f"未知规则类型: {rule_type}")
        sys.exit(1)

    config = load_config()
    config["rules"][name] = rule
    save_config(config)
    print(f"规则 '{name}' 已创建。")
    notify_daemon({"command": "reload"})

def parse_interval(s):
    """解析 30s, 5m, 1h, 2d 为秒数。"""
    s = s.strip()
    if s[-1] in "smhd":
        num = float(s[:-1])
        if s[-1] == 's':
            return num
        elif s[-1] == 'm':
            return num * 60
        elif s[-1] == 'h':
            return num * 3600
        elif s[-1] == 'd':
            return num * 86400
    return float(s)

def cmd_del(args):
    if not args:
        print("用法: dmsw del <规则名>")
        sys.exit(1)
    name = args[0]
    config = load_config()
    if name in config["rules"]:
        del config["rules"][name]
        save_config(config)
        print(f"规则 '{name}' 已删除。")
        notify_daemon({"command": "reload"})
    else:
        print(f"规则 '{name}' 不存在。")

def cmd_arm(args):
    config = load_config()
    if not config.get("armed"):
        config["armed"] = True
        save_config(config)
        print("系统已预位。")
        notify_daemon({"command": "arm"})
    else:
        print("系统已经在预位状态。")

def cmd_unarm(args):
    config = load_config()
    if config.get("armed"):
        config["armed"] = False
        save_config(config)
        print("系统已解除预位。")
        notify_daemon({"command": "unarm"})
    else:
        print("系统未处于预位状态。")

def cmd_bind(args):
    if len(args) < 2:
        print("用法: dmsw bind <动作> [规则名] <按键组合>")
        print("动作: arm, unarm, 或规则名")
        print("按键组合示例: a+b 或 ctrl+alt+x (使用 '+' 分隔)")
        sys.exit(1)

    action_target = args[0]
    if action_target in ("arm", "unarm"):
        action = action_target
        rule_name = None
        keys_str = args[1]
    else:
        action = "trigger"
        rule_name = action_target
        keys_str = args[1]

    key_list = parse_key_combination(keys_str)
    config = load_config()
    binding = {"keys": key_list, "action": action}
    if rule_name:
        binding["rule"] = rule_name
    config["bindings"].append(binding)
    save_config(config)
    print(f"绑定已添加: {keys_str} -> {action} {rule_name or ''}")
    notify_daemon({"command": "reload"})

def parse_key_combination(keystr):
    """解析 'ctrl+alt+x' 或 'a+b' 为 key 名称列表。"""
    parts = keystr.split('+')
    key_names = []
    for p in parts:
        p = p.strip().upper()
        # 常见修饰键映射
        if p in ("CTRL", "CONTROL"):
            p = "KEY_LEFTCTRL"
        elif p == "ALT":
            p = "KEY_LEFTALT"
        elif p == "SHIFT":
            p = "KEY_LEFTSHIFT"
        elif p == "SUPER":
            p = "KEY_LEFTMETA"
        elif len(p) == 1:
            p = "KEY_" + p
        else:
            # 假定已为完整键名
            if not p.startswith("KEY_"):
                p = "KEY_" + p
        key_names.append(p)
    return key_names

def cmd_trigger(args):
    if not args:
        print("用法: dmsw trigger <规则名>")
        sys.exit(1)
    rule_name = args[0]
    notify_daemon({"command": "trigger", "rule": rule_name})

def cmd_daemon(args):
    """控制守护进程：--start 启动，--stop 停止。"""
    if not args or args[0] not in ("--start", "--stop"):
        print("用法: dmsw daemon <--start|--stop>")
        print("  --start  启动守护进程")
        print("  --stop   停止守护进程")
        sys.exit(1)

    action = args[0]

    if action == "--start":
        if os.path.exists(PID_FILE):
            with open(PID_FILE) as f:
                pid = int(f.read())
            try:
                os.kill(pid, 0)
                print("守护进程已在运行中。")
                return
            except OSError:
                os.unlink(PID_FILE)

        # 守护进程化
        pid = os.fork()
        if pid > 0:
            print(f"守护进程已启动，PID: {pid}")
            sys.exit(0)
        else:
            os.setsid()
            daemon = Daemon()
            daemon.run()
            sys.exit(0)

    elif action == "--stop":
        if not os.path.exists(PID_FILE):
            print("守护进程未运行（PID 文件不存在）。")
            sys.exit(1)

        with open(PID_FILE) as f:
            pid = int(f.read())

        try:
            os.kill(pid, 0)
        except OSError:
            print("守护进程未运行（进程不存在）。")
            os.unlink(PID_FILE)
            sys.exit(1)

        # 发送停止命令到守护进程
        try:
            notify_daemon({"command": "stop"})
            print("守护进程已停止。")
        except SystemExit:
            pass

def main():
    # 顶层参数解析
    parser = argparse.ArgumentParser(
        description="死手系统 (Dead Man's Switch) for GNU/Linux"
    )
    parser.add_argument('command', nargs='?', help='子命令')
    parser.add_argument('args', nargs=argparse.REMAINDER, help='子命令参数')

    # 提前处理 -v 和 --version
    if len(sys.argv) > 1 and sys.argv[1] in ('-v', '--version'):
        print(f"dmsw version {VERSION}")
        sys.exit(0)

    # 提前处理 -h 和 --help
    if len(sys.argv) > 1 and sys.argv[1] in ('-h', '--help'):
        parser.print_help()
        print("""
子命令:
    new     创建一条规则
    del     删除一条规则
    arm     预位系统（激活所有规则监控）
    unarm   解除预位
    bind    绑定一组按键到规则或预位切换
    trigger 立即触发一条规则
    daemon  控制守护进程 (--start 启动, --stop 停止)

示例:
    dmsw new rule1 -k 30s space o
    dmsw daemon --start
    dmsw arm
        """)
        sys.exit(0)

    args = parser.parse_args()
    command = args.command
    cmd_args = args.args

    if not command:
        parser.print_help()
        sys.exit(1)

    if command == "new":
        cmd_new(cmd_args)
    elif command == "del":
        cmd_del(cmd_args)
    elif command == "arm":
        cmd_arm(cmd_args)
    elif command == "unarm":
        cmd_unarm(cmd_args)
    elif command == "bind":
        cmd_bind(cmd_args)
    elif command == "trigger":
        cmd_trigger(cmd_args)
    elif command == "daemon":
        cmd_daemon(cmd_args)
    else:
        print(f"未知子命令: {command}")
        sys.exit(1)

if __name__ == "__main__":
    main()