# dmsw - 死手系统 (Dead Man's Switch)

GNU/Linux 平台的死手系统，用于监控特定条件并在条件触发时执行预设命令。

## 功能特性

- **按键超时规则 (-k)**: 在指定时间内未按下特定按键则触发
- **按键保持规则 (-K)**: 释放特定按键时触发
- **进程监控规则 (-p)**: 当指定进程不存在时触发
- **按键绑定**: 绑定组合键来预位/解除预位系统或触发规则
- **守护进程模式**: 后台运行监控所有规则

## 安装

### 依赖

- Python 3.7+
- evdev (键盘事件监控)
- psutil (进程监控，可选但推荐)

### 从源码安装

```bash
# 克隆仓库
git clone <repository-url>
cd JimDeadManSwitch

# 创建虚拟环境
python3 -m venv venv
source venv/bin/activate

# 安装依赖
pip install -r requirements.txt

# 使用 PyInstaller 构建
pyinstaller build.spec

# 或直接使用
python Src/Main.py
```

### 权限设置

访问键盘设备需要 `input` 组权限或 root 权限：

```bash
# 将用户添加到 input 组
sudo usermod -a -G input $USER

# 或运行时使用 sudo
sudo ./dist/dmsw daemon --start
```

## 使用方法

### 基本命令

```bash
# 显示版本
dmsw -v

# 创建规则
dmsw new <规则名> <-k|-K|-p> [参数...] <命令>

# 删除规则
dmsw del <规则名>

# 预位系统
dmsw arm

# 解除预位
dmsw unarm

# 绑定按键
dmsw bind <动作> [规则名] <按键组合>

# 立即触发规则
dmsw trigger <规则名>

# 控制守护进程
dmsw daemon --start   # 启动守护进程
dmsw daemon --stop    # 停止守护进程
```

### 规则类型

#### 按键超时规则 (-k)

在指定时间内未按下特定按键则触发命令：

```bash
# 30秒内未按空格键则执行命令
dmsw new rule1 -k 30s space "rm -rf /home/$USER/.safebox"

# 使用按键ID
dmsw new rule2 -k 5m --id 57 "systemctl poweroff"
```

时间单位：`s` (秒), `m` (分), `h` (小时), `d` (天)

#### 按键保持规则 (-K)

释放特定按键时触发命令：

```bash
# 释放左Ctrl键时执行命令
dmsw new rule3 -K leftctrl "rm -rf /home/$USER/.safebox"

# 使用按键ID
dmsw new rule4 -K --id 29 "systemctl poweroff"
```

#### 进程监控规则 (-p)

当指定进程不存在时触发命令：

```bash
# 当 firefox 进程不存在时执行命令
dmsw new rule5 -p firefox "systemctl poweroff"
```

### 按键绑定

绑定组合键来控制系统：

```bash
# 绑定 Ctrl+Alt+A 预位系统
dmsw bind arm ctrl+alt+a

# 绑定 Ctrl+Alt+U 解除预位
dmsw bind unarm ctrl+alt+u

# 绑定 Ctrl+Alt+T 触发指定规则
dmsw bind rule1 ctrl+alt+t
```

### 预设命令

支持以下预设命令缩写：

| 缩写 | 命令 |
|------|------|
| `d` | `rm -rf /home/$USER/.safebox` |
| `s` | `bleachbit -s /home/$USER/.safebox` |
| `o` | `systemctl poweroff` |
| `O` | `sysrq -reisub` |
| `r` | `systemctl reboot` |
| `R` | `sysrq -reisuo` |
| `c` | `sysrq -c` |

使用预设命令：

```bash
dmsw new rule1 -k 30s space o  # 30秒未按空格则关机
```

## 配置

配置文件位于 `~/.config/dmsw/config.json`：

```json
{
  "armed": false,
  "rules": {
    "rule1": {
      "type": "key_timeout",
      "interval": 30,
      "key": "space",
      "key_id": false,
      "command": "systemctl poweroff"
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

## 工作流程

1. **创建规则**：使用 `dmsw new` 创建监控规则
2. **启动守护进程**：使用 `dmsw daemon --start` 启动后台监控
3. **预位系统**：使用 `dmsw arm` 激活所有规则
4. **监控运行**：守护进程持续监控所有规则条件
5. **触发执行**：当条件满足时自动执行预设命令

## 注意事项

- 需要 root 或 `input` 组权限才能监控键盘事件
- 预位后规则才会生效
- 守护进程通过 Unix Socket 通信 (`/tmp/dmsw-<uid>.sock`)
- PID 文件存储在 `/tmp/dmsw-<uid>.pid`

## 常见问题

- Q：此项目是否支持 Windows 操作系统？
- A：目前并不支持 Windows 操作系统，但是我们正计划增加对 Windows 操作系统平台的支持。

## 许可证

MIT License
