# dmsw - 死手系统 (Dead Man's Switch)

GNU/Linux 平台的死手系统，用于监控特定条件（按键超时、按键释放、进程存活），
并在条件被打破时执行指定命令。

## 功能特性

- **按键超时规则 (`-k`)**：在指定时间内未按下特定按键则触发命令
- **按键保持规则 (`-K`)**：释放特定按键时触发命令
- **进程监控规则 (`-p`)**：指定进程不存在时触发命令
- **规则列表 (`list`)**：按名称列出全部规则、类型、条件与命令
- **按键绑定**：用组合键预位、解除预位或触发某条规则
- **组合键捕获**：`bind -c/--capture` 直接监听键盘，把实际按下的组合键保存为绑定
- **守护进程模式**：后台运行并监控所有规则
- **干运行模式**：安全测试规则触发，命令只记录不执行
- **启动自动解除预位**：系统启动后首次运行守护进程时自动解除预位，避免关机/重启循环

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
3. `git tag -a v2.1.0 -m "dmsw 2.1.0" && git push origin v2.1.0`；
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
dmsw new rule1 -k 30s space "systemctl poweroff"

# 2. 查看已创建的规则
dmsw list

# 3. 启动守护进程
dmsw daemon --start

# 4. 预位系统（规则开始生效）
dmsw arm

# 5. 查看状态/日志
dmsw -v
tail -f ~/.config/dmsw/dmsw.log

# 6. 解除预位并停止守护进程
dmsw unarm
dmsw daemon --stop
```

## systemd 集成

仓库提供两个单元文件，二者只能启用一个（它们共用同一份配置、socket 与 PID 文件）：

| 单元 | 安装位置 | 适用场景 |
|------|----------|----------|
| [`dmsw.service`](systemd/dmsw.service) | `/usr/lib/systemd/user/` | 当前登录用户 |
| [`dmsw@.service`](systemd/dmsw@.service) | `/usr/lib/systemd/system/` | 开机即运行，以指定用户身份运行（无需登录） |

AUR 包会自动安装这两个文件；从源码安装时可手动复制到对应目录并运行
`systemctl daemon-reload`（用户级还需 `systemctl --user daemon-reload`）。单元中的
二进制路径按 AUR 包的 `/usr/bin/dmsw` 编写；若按前面的源码方式安装到
`/usr/local/bin`，请先把单元里的 `ExecStart` / `ExecStop` 改为对应路径。

### 用户级（推荐桌面用户）

```bash
systemctl --user enable --now dmsw.service
systemctl --user status dmsw.service
```

要求当前用户属于 `input` 组（见[权限设置](#权限设置)）。若要在未登录时也保持运行：

```bash
loginctl enable-linger "$USER"
```

### 系统级（开机运行，以指定用户身份）

```bash
sudo systemctl enable --now dmsw@$(id -u).service
```

实例名是目标用户的**数字 UID**（`$(id -u)`），例如 `dmsw@1000.service`。守护进程
按 UID 命名 `/tmp/dmsw-<uid>.pid`，而系统级单元里的 `%U` 始终是 0，因此模板
使用 `%i` 作为 UID。

该单元通过 `SupplementaryGroups=input` 授予键盘访问权限，因此不要求用户加入
`input` 组；规则命令仍以该用户身份执行。

### 停止与日志

```bash
systemctl --user stop dmsw.service          # 用户级
sudo systemctl stop dmsw@$(id -u).service      # 系统级

journalctl --user -u dmsw.service -f        # 用户级启动/停止信息
sudo journalctl -u dmsw@$(id -u).service -f    # 系统级启动/停止信息
tail -f ~/.config/dmsw/dmsw.log             # 规则触发与运行日志
```

单元只负责启动守护进程。守护进程检测到新的系统启动时会自动解除预位（见下文），
因此开机后不会沿用上次关机前的预位状态，需要重新执行 `dmsw arm`；同一 boot
内 systemd 的自动重启不会解除预位。`Type=forking` 通过 `/tmp/dmsw-<uid>.pid`
跟踪守护进程。不要同时启用两个单元，也不要在 systemd 管理时再手动启动第二个实例。

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

日志中会出现 `[干运行] 将执行命令: ...`。
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
dmsw list
dmsw arm
dmsw unarm
dmsw bind <动作|规则名> <按键组合>
dmsw bind [--capture|-c] <动作|规则名>
dmsw trigger <规则名> [--dry-run]
dmsw daemon --start [--dry-run]
dmsw daemon --stop
dmsw -v|--version
dmsw -h|--help
```

### 列出规则

```bash
dmsw list
```

输出示例：

```
系统未预位，共 2 条规则：
  rule1  [按键超时] 30s 内未按 space -> systemctl poweroff
  rule3  [进程监控] 进程 firefox 不存在 -> systemctl reboot
```

规则按名称排序。首行显示当前预位状态和规则数量；没有规则时会提示用 `dmsw new`
创建。使用 `--key-code` 创建的规则会把数字键码显示为 `#57`。

### 规则类型

#### 按键超时规则 (`-k`)

```bash
# 30 秒内未按空格键则执行命令
dmsw new rule1 -k 30s space "rm -rf /home/$USER/.safebox"

# 使用数字键码（--key-code）
dmsw new rule2 -k 5m --key-code 57 "systemctl poweroff"
```

时间单位支持 `s`（秒）、`m`（分）、`h`（小时）、`d`（天），也可以直接写秒数。
超时触发后计时器会重置，因此只要持续未按键，命令会按间隔重复执行。

#### 按键保持规则 (`-K`)

```bash
# 释放左 Ctrl 键时执行命令
dmsw new rule3 -K leftctrl "rm -rf /home/$USER/.safebox"

# 使用数字键码
dmsw new rule4 -K --key-code 29 "systemctl poweroff"
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

#### 捕获组合键 (`-c` / `--capture`)

不想手动查按键名时，可以让 dmsw 监听键盘，把实际按下的组合键保存为绑定：

```bash
# 监听键盘，按下的组合键绑定到规则 rule1
dmsw bind --capture rule1
dmsw bind -c unarm

# 输出示例：
# 请按下要绑定的组合键（按住修饰键，全部松开后结束）…
# 绑定已添加: KEY_LEFTCTRL+KEY_LEFTALT+KEY_T -> trigger rule1
```

使用说明：

- 按住所有修饰键（Ctrl/Alt/Shift/Super），再按主键；**全部松开**后捕获结束。
- 捕获会记录“同时按下数量最多”的那一组键，短暂误触不会拼接出多余组合。
- 守护进程运行时，捕获请求交给守护进程处理，复用其已打开的键盘设备；捕获期间
  规则监控暂停，结束后重新计时，不会因捕获过程误触发。
- 守护进程未运行时，`bind -c` 会直接打开键盘监听（需要读取 `/dev/input/event*`
  的权限）。
- 守护进程侧等待上限为 20 秒，直接监听上限为 15 秒；超时则不会保存绑定。
- 捕获只负责得到组合键，绑定动作（`arm`/`unarm`/规则名）与普通 `bind` 相同。

命令通过 `sh -c` 执行，工作目录为守护进程启动时的目录，输出被丢弃。

### 按键名称

- 内核键名：`KEY_A`、`KEY_SPACE`、`KEY_LEFTCTRL`……
- 去掉 `KEY_` 前缀的名称：`a`、`space`、`leftctrl`、`f1`、`5`
- 常用别名：`ctrl`、`alt`、`shift`、`super`/`win`、`enter`、`esc`、`del`、
  `ins`、`pgup`、`pgdn` 等（不区分大小写）
- `--key-code` 模式下直接使用 0–65535 的 Linux 键码

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
      "command": "systemctl poweroff"
    },
    "rule3": {
      "type": "process",
      "process_name": "firefox",
      "command": "systemctl reboot"
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
| `~/.config/dmsw/boot-id` | 上次运行守护进程时的内核 boot ID（权限 0600） |
| `~/.config/dmsw/dmsw.log` | 守护进程日志 |
| `/tmp/dmsw-<uid>.sock` | Unix socket（权限 0600） |
| `/tmp/dmsw-<uid>.pid` | PID 文件（权限 0600） |

`dmsw daemon --start` 会 fork 出后台进程并等待其完成初始化（绑定 socket、
写入 PID）后才返回，因此启动失败会直接报错。日志写入 `dmsw.log`，stdout/stderr
重定向到 `/dev/null`。

## 启动时自动解除预位

为避免“触发关机/重启 → 系统启动 → 规则再次触发”的循环，守护进程每次启动都会
比较当前内核 boot ID 与 `~/.config/dmsw/boot-id` 中记录的值：

- boot ID 不同（或首次记录）且配置处于预位状态时，自动把 `armed` 设为 `false`
  并保存，然后记录新的 boot ID；
- boot ID 相同（同一 boot 内 systemd 重启或手动重启守护进程）时，保持原预位状态；
- 无法读取 `/proc/sys/kernel/random/boot_id` 时跳过检查并写入日志。

`dmsw arm` 也会记录当前 boot ID，因此在同一个 boot 内“先 arm、再启动守护进程”
不会被误解除；只有跨系统启动的预位状态会被清除。首次升级到带此功能的版本时，
由于还没有 `boot-id` 文件，已预位的配置会在下一次守护进程启动时被解除一次。

## 工作流程

1. 使用 `dmsw new` 创建规则，使用 `dmsw bind` 创建组合键绑定
2. `dmsw daemon --start` 启动后台监控（新系统启动时会自动解除预位）
3. `dmsw arm` 预位，规则开始生效
4. 守护进程每 0.5 秒检查超时与进程规则，并实时处理键盘事件
5. 条件满足时执行规则命令
6. `dmsw unarm` 解除预位，`dmsw daemon --stop` 停止守护进程

## 注意事项与免责声明

- 规则命令以运行守护进程的用户权限执行，**请务必先测试命令**，避免误删数据。
- 键盘监控以只读方式共享输入设备，不会独占或拦截按键，其他程序仍能正常接收输入。
- `-k` 规则在预位后生效；解除预位后不再触发。
- 示例中的 `sysrq` 需要 root 权限和内核 SysRq 支持，请谨慎使用关机/重启类命令。
- 本项目按 MIT License 发布。
