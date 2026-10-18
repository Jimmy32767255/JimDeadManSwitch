# dmsw - 死手系统 (Dead Man's Switch)

GNU/Linux 平台的死手系统，用于监控特定条件（按键超时、按键释放、进程存活），
并在条件被打破时执行预设命令。本项目是原 Python 版本的完整 Rust 重写。

## 功能特性

- **按键超时规则 (`-k`)**：在指定时间内未按下特定按键则触发命令
- **按键保持规则 (`-K`)**：释放特定按键时触发命令
- **进程监控规则 (`-p`)**：指定进程不存在时触发命令
- **按键绑定**：用组合键预位、解除预位或触发某条规则
- **守护进程模式**：后台运行并监控所有规则
- **干运行模式**：安全测试规则触发，命令只记录不执行
- **纯 Rust 实现**：只依赖 `libc`、`serde`、`serde_json`，无需 Python 运行时

## 系统要求

- GNU/Linux（使用 evdev 键盘接口与 `/proc`）
- Rust 1.70 或更高版本（仅构建时需要）
- 访问 `/dev/input/event*` 的权限：通常需要 `input` 组或 root

## 构建与安装

```bash
# 构建 release 版本
cargo build --release

# 安装到 /usr/local/bin
sudo install -m 755 target/release/dmsw /usr/local/bin/dmsw
```

开发检查：

```bash
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

### Arch Linux（AUR）

[AUR 打包文件](aur/PKGBUILD) 位于 `aur/` 目录，对应的 [.SRCINFO](aur/.SRCINFO) 已生成。
PKGBUILD 从 GitHub 的 `v$pkgver` 标签构建，因此构建前需要先推送对应标签。
由于 `makepkg` 默认使用当前目录下的 `src/`，请在 `aur/` 子目录中执行，避免与
Rust 源码目录冲突：

```bash
cd aur
makepkg -si
```

发布新版本时：

1. 更新 [Cargo.toml](Cargo.toml) 与 [aur/PKGBUILD](aur/PKGBUILD) 中的版本号；
2. 提交并推送所有改动（tag 必须包含 `LICENSE`，否则打包会失败）；
3. `git tag -a v1.1.0 -m "dmsw 1.1.0" && git push origin v1.1.0`；
4. `cd aur && makepkg --printsrcinfo > .SRCINFO`。

提交 AUR 时，将 `aur/` 下的 `PKGBUILD` 与 `.SRCINFO` 推送至 AUR 的 `dmsw` 仓库即可。

### 权限设置

监控键盘需要读取输入设备：

```bash
# 将当前用户加入 input 组（重新登录后生效）
sudo usermod -a -G input "$USER"

# 或者以 root 运行守护进程
sudo dmsw daemon --start
```

如果权限不足，守护进程仍会启动，但日志中会提示未找到键盘设备。

## 快速开始

```bash
# 1. 创建规则：30 秒内未按空格键则关机
dmsw new rule1 -k 30s space o

# 2. 启动守护进程
dmsw daemon --start

# 3. 预位系统（规则开始生效）
dmsw arm

# 4. 查看状态/日志
dmsw -v
tail -f ~/.config/dmsw/dmsw.log

# 5. 解除预位并停止守护进程
dmsw unarm
dmsw daemon --stop
```

## 干运行模式

干运行用于安全地验证规则和绑定是否会按预期触发：规则照常评估，但命令只写入
日志或打印出来，不会真正执行。

```bash
# 若普通守护进程正在运行，先停止它
dmsw daemon --stop

# 以干运行模式启动：触发情况与真实运行一致，命令只记录
dmsw daemon --start --dry-run
dmsw arm
tail -f ~/.config/dmsw/dmsw.log

# 单次预览某条规则会执行什么命令（无需启动守护进程）
dmsw trigger rule1 --dry-run
```

日志中会出现 `[干运行] 将执行命令: ...`，预设缩写会展开为完整命令。
`--dry-run` 可以写在子命令前后，例如 `dmsw --dry-run trigger rule1`。

注意：

- 干运行只阻止“执行命令”；配置修改、预位状态、绑定等仍然照常生效。
- 守护进程级别的干运行需要不带 `--dry-run` 重新启动才会恢复真实执行。
- 只想预览单条规则时，可直接用 `dmsw trigger <规则名> --dry-run`，对正在运行的
  普通守护进程同样安全；若要整体切换为干运行，请先 `dmsw daemon --stop`。

## 命令

```
dmsw new <规则名> <-k|-K|-p> [参数...] <命令>
dmsw del <规则名>
dmsw arm
dmsw unarm
dmsw bind <动作|规则名> <按键组合>
dmsw trigger <规则名> [--dry-run]
dmsw daemon --start [--dry-run]
dmsw daemon --stop
dmsw -v|--version
dmsw -h|--help
```

### 规则类型

#### 按键超时规则 (`-k`)

```bash
# 30 秒内未按空格键则执行命令
dmsw new rule1 -k 30s space "rm -rf /home/$USER/.safebox"

# 使用数字键码（--id）
dmsw new rule2 -k 5m --id 57 "systemctl poweroff"
```

时间单位支持 `s`（秒）、`m`（分）、`h`（小时）、`d`（天），也可以直接写秒数。
超时触发后计时器会重置，因此只要持续未按键，命令会按间隔重复执行。

#### 按键保持规则 (`-K`)

```bash
# 释放左 Ctrl 键时执行命令
dmsw new rule3 -K leftctrl "rm -rf /home/$USER/.safebox"

# 使用数字键码
dmsw new rule4 -K --id 29 "systemctl poweroff"
```

注意：该规则在**释放**按键时触发，用于“松开按键 = 出事”的场景。
只有在系统已预位时才会执行。

#### 进程监控规则 (`-p`)

```bash
# 当 firefox 进程不存在时执行命令
dmsw new rule5 -p firefox "systemctl poweroff"
```

进程名与 `/proc/<pid>/comm` 精确匹配。进程消失时只触发一次；进程恢复后再次消失
会重新触发。该规则同样只在预位后生效。

### 按键绑定

```bash
# Ctrl+Alt+A 预位
dmsw bind arm ctrl+alt+a

# Ctrl+Alt+U 解除预位
dmsw bind unarm ctrl+alt+u

# Ctrl+Alt+T 触发规则 rule1
dmsw bind rule1 ctrl+alt+t

# 也支持把组合键写在前面
dmsw bind ctrl+alt+t rule1
```

组合键使用 `+` 分隔。绑定按“当前按下的键集合包含组合键”匹配：例如绑定
`ctrl+alt+x` 后，同时按住 Ctrl、Alt、X 即可触发；组合键未释放前不会重复触发。

### 预设命令

| 缩写 | 实际命令 |
|------|----------|
| `d` | `rm -rf /home/$USER/.safebox` |
| `s` | `bleachbit -s /home/$USER/.safebox` |
| `o` | `systemctl poweroff` |
| `O` | `sysrq -o` |
| `r` | `systemctl reboot` |
| `R` | `sysrq -b` |
| `c` | `sysrq -c` |

```bash
# 30 秒未按空格则关机
dmsw new rule1 -k 30s space o
```

命令通过 `sh -c` 执行，工作目录为守护进程启动时的目录，输出被丢弃。

### 按键名称

- 内核键名：`KEY_A`、`KEY_SPACE`、`KEY_LEFTCTRL`……
- 去掉 `KEY_` 前缀的名称：`a`、`space`、`leftctrl`、`f1`、`5`
- 常用别名：`ctrl`、`alt`、`shift`、`super`/`win`、`enter`、`esc`、`del`、
  `ins`、`pgup`、`pgdn` 等（不区分大小写）
- `--id` 模式下直接使用 0–65535 的 Linux 键码

## 配置文件

配置文件位于 `~/.config/dmsw/config.json`，由命令自动维护：
目录权限 `0700`，文件权限 `0600`，写入采用临时文件 + `rename` 的原子方式。

```json
{
  "armed": false,
  "rules": {
    "rule1": {
      "type": "key_timeout",
      "interval": 30.0,
      "key": "space",
      "key_id": false,
      "command": "systemctl poweroff"
    },
    "rule2": {
      "type": "key_hold",
      "key": "leftctrl",
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
    {
      "keys": ["KEY_LEFTCTRL", "KEY_LEFTALT", "KEY_A"],
      "action": "arm"
    }
  ]
}
```

修改配置后，正在运行的守护进程会通过 `dmsw new`/`del`/`bind` 自动收到
`reload` 命令；也可以手动发送 socket 请求重新加载。

## 守护进程与文件位置

| 文件 | 说明 |
|------|------|
| `~/.config/dmsw/config.json` | 配置（权限 0600） |
| `~/.config/dmsw/dmsw.log` | 守护进程日志 |
| `/tmp/dmsw-<uid>.sock` | Unix socket（权限 0600） |
| `/tmp/dmsw-<uid>.pid` | PID 文件（权限 0600） |

`dmsw daemon --start` 会 fork 出后台进程并等待其完成初始化（绑定 socket、
写入 PID）后才返回，因此启动失败会直接报错。日志写入 `dmsw.log`，stdout/stderr
重定向到 `/dev/null`。

## 工作流程

1. 使用 `dmsw new` 创建规则，使用 `dmsw bind` 创建组合键绑定
2. `dmsw daemon --start` 启动后台监控
3. `dmsw arm` 预位，规则开始生效
4. 守护进程每 0.5 秒检查超时与进程规则，并实时处理键盘事件
5. 条件满足时执行预设命令
6. `dmsw unarm` 解除预位，`dmsw daemon --stop` 停止守护进程

## 与 Python 原版的差异

Rust 版保持了原有 CLI、配置结构、socket 协议和预设命令，同时修复了原版的
若干问题：

- **按键绑定现在能真正触发**：原版把字符串键名与整数键码直接比较，绑定永远
  不匹配；现在会把绑定解析为键码集合并做子集匹配。
- **组合键不会重复触发**：按住不放或自动重复只触发一次，组合键被打破后重置。
- **进程规则改为边沿触发**：原版在进程缺失期间每 0.5 秒重复执行命令；现在只在
  “存在 → 缺失”时触发一次。
- **守护进程未运行时不再报错退出**：`new`/`del`/`bind`/`arm`/`unarm` 会先保存
  配置，仅提示守护进程未运行。
- **更严格的权限**：配置目录 0700、配置文件 0600、socket 与 PID 文件 0600。
- **更安全的启动**：子进程回收避免僵尸进程；启动握手让失败可见。
- **无 Python 依赖**：用 `/proc` 扫描代替 psutil，用 libc ioctl 代替 evdev。
- **新增干运行模式**：`--dry-run` 可让守护进程或单次 `trigger` 只报告将要执行的
  命令，用于安全测试规则。
- **参数兼容**：`bind` 同时接受“动作在前”和“按键组合在前”两种写法。

原 Python 实现保留在 git 历史中。

## 注意事项与免责声明

- 规则命令以运行守护进程的用户权限执行，**请务必先测试命令**，避免误删数据。
- 键盘监控使用 `EVIOCGRAB` 独占设备，被监控的键盘事件不会传递给其他程序。
- `-k` 规则在预位后生效；解除预位后不再触发。
- 示例中的 `sysrq` 需要 root 权限和内核 SysRq 支持，请谨慎使用关机/重启类命令。
- 本项目按 MIT License 发布。
