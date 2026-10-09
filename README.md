# hyprpet

**Hyprland 像素桌面宠物** —— Rust + Wayland layer-shell，一只住在桌面上的像素小猫/史莱姆。

自主漫游、踩窗口行走、抓起抛掷、通知气泡探头偷看，双击聊天调用 AI 工具，还能成为其他 agent 的 MCP 工具被远程指挥。

```
~17MB 内存 · 零 GPU 依赖 · 数据驱动角色 · 全热重载
```

---

## 目录

- [功能一览](#功能一览)
- [构建与运行](#构建与运行)
- [交互速查](#交互速查)
- [角色系统（换肤 / 自定义宠物）](#角色系统)
- [行为配置](#行为配置)
- [AI 配置（三种方式）](#ai-配置)
- [MCP 接入（两个方向）](#mcp-接入)
- [Agent 状态联动](#agent-状态联动)
- [外部控制 socket](#外部控制-socket)
- [Hyprland 集成](#hyprland-集成)
- [配置文件总览](#配置文件总览)
- [架构](#架构)
- [测试](#测试)

---

## 功能一览

| 领域 | 功能 |
|---|---|
| **渲染** | wlr-layer-shell Overlay 层 · 每显示器一个全屏 surface · wl_shm 软件像素管道 · nearest 采样无模糊 |
| **多屏** | 全局布局坐标自由跨屏拖动 · 热插拔 · 共享边界自动避让（内缘不收边） |
| **行为** | `actions.toml` 数据驱动 FSM · idle/walk/sleep 自主漫游 · **空闲门控**（无输入 N 秒才走动，活动即停） |
| **物理** | 抓起抛掷 · 重力 + 地板穿越检测 · 天花板 · 窗口顶缘可站立 · 走出边缘坠落 · 侧壁转向 |
| **感知** | Hyprland IPC（窗口几何缓存）· 全屏避让（淡出）· 系统负载感知（擦汗/陪读） |
| **通知** | DBus monitor 窃听系统通知 → 头顶像素气泡 + alert 动作 + 音效 |
| **边缘** | mini 模式 + 收边探头偷看（滑入/探出/张望/缩回动画，顶部倒挂探头） |
| **交互** | 悬停睁大眼 · 单击抚摸（^^ 开心 + 音效）· 双击聊天 · 右键配置菜单 · 系统托盘（SNI） |
| **AI** | 像素聊天框（fcitx5 中文输入 · Ctrl+V 粘贴 · 长按重复）· LLM 工具调用 · MCP 客户端/服务端 |
| **Agent 联动** | 语义状态点（思考/工作/完成/警告/异常/确认）· 专属音效 · 控制 socket |
| **配置** | 右键菜单/托盘/弹窗改 LLM · 全部配置热重载 · 改前自动备份 |

---

## 构建与运行

```bash
git clone <repo> && cd HyprPets
cargo build --release
./target/release/hyprpet            # 启动（右下角出现宠物）
./target/release/hyprpet -s 6       # 默认缩放（角色 meta.toml 的 scale 优先）
./target/release/hyprpet --pos 100,200
```

依赖：Wayland + Hyprland、系统字体（Noto Sans CJK 等，气泡/菜单文字用）、`wl-paste`（剪贴板工具）。

开机自启（hyprland.conf）：

```ini
exec-once = ~/Projects/ToolsProjects/HyprPets/target/release/hyprpet
```

## 交互速查

| 操作 | 效果 |
|---|---|
| 左键拖拽 / 快速甩动 | 移动 / 重力抛掷（可落在窗口顶缘） |
| **悬停** | 宠物睁大眼看向你（alert） |
| **单击**（不拖动） | 抚摸 → ^^ 开心动作 + 音效 |
| 左键**双击**（或 `kill -USR2`） | 打开 AI 聊天对话框 |
| 右键 | 配置菜单（换肤 / 阈值 / mini / 穿透 / 退出） |
| `kill -USR1` | 切换鼠标穿透（宠物变暗提示） |
| `kill -USR3` | 切换 mini 模式 |
| mini 下单击 | 收进最近屏幕边缘（6px 把手） |
| mini 下双击 / 点把手 | 弹出 / 探头偷看循环 |

**聊天框**：Enter 发送 · Esc 关闭 · Ctrl+V 粘贴 · 支持 fcitx5 中文输入（候选框跟随光标）· 长按删除/方向键自动重复。点击其他窗口焦点自动让出、弹窗收起。

**收边探头**：藏入后随机 2.5~7s 探出半张脸张望（alert 睁大眼），1.2~2.6s 缩回，循环。顶部边缘倒挂探头（头朝下看你）。

## 角色系统

右键菜单「换肤」即时切换。内置**像素小猫**和**像素史莱姆**（程序生成 32 帧动画）。

自定义角色 = 往目录丢三个文件，**保存即热生效**（无需重启）：

```text
~/.config/hyprpet/characters/<角色名>/
├── meta.toml      # 名称/作者/缩放/帧尺寸/预览帧
├── sprites.png    # 贴图（网格表或 Aseprite 整图）
├── sprites.json   # 可选：Aseprite 导出（支持 frameTags）
└── actions.toml   # 动作状态机
```

`meta.toml`：

```toml
name = "像素小猫"
scale = 4            # 默认像素放大倍率
frame_width = 32     # 网格格式必填
frame_height = 32
preview = [0, 0]     # 菜单预览帧
```

`actions.toml` 关键字段：

```toml
default = "idle"

[actions.walk_left]
fps = 8              # 帧率
loop = true          # 循环播放
duration = 4         # loop 动作持续秒数（超时转移）
movement = [-70, 0]  # 移动速度 px/s（缺省不动）
auto_turn = true     # 撞边界自动切镜像动作（_left ↔ _right）
frames = [[0,1],[1,1],[2,1],[3,1]]   # 网格 [col,row]；Aseprite 用 tag = "walk"
next_random = ["idle", "sleep"]      # 随机转移（可加权重复）
sound = "step.wav"   # 进入动作时播放（相对角色目录，可选）
```

**保留动作名**（缺失自动回落 idle）：`idle` `walk_left/right` `sleep` `grabbed`（抓起）`fall`（下落）`land`（落地）`petted`（抚摸）`alert`（警觉/通知）`tired`（高负载擦汗）`study`（陪读）`celebrate`（庆祝）。

## 行为配置

`~/.config/hyprpet/config.toml`（保存热生效）：

```toml
[behavior]
# 无电脑活动多少秒后宠物才开始自主漫游（默认 120，单位秒）
# 基于 Wayland ext-idle-notify-v1 合成器级输入空闲检测，有输入立即停止
move_after_idle_seconds = 120
```

右键菜单「漫游阈值」可点击循环切换（30/60/120/300s）。

## AI 配置

### 方式一：菜单弹窗（推荐）

右键菜单 / 托盘 →「LLM 设置…」→ 像素表单输入 → Enter 保存（api_key 掩码显示，MCP 配置自动保留），保存后新对话即生效。

### 方式二：编辑 `~/.config/hyprpet/agent.toml`

```toml
[llm]
backend = "openai"        # openai 兼容(DeepSeek/Moonshot/通义/OpenAI) | anthropic
base_url = "https://api.deepseek.com/v1"
api_key = "env:DEEPSEEK_API_KEY"   # env:NAME / cmd:命令 / 字面量
model = "deepseek-chat"

[security]
shell_allow = ["ls", "date", "uptime", "df", "free", "cat /proc"]  # run_command 白名单前缀
```

配置后聊天即可对话，内置工具自动可用：`get_system_status` / `set_timer`（到点气泡+音效）/ `get_clipboard` / `run_command`（白名单）。无 LLM 时聊天里可用本地命令：`/timer 5 喝水`、`/status`、`/shell ls`、`/mini`、`/hide`、`/show`、`/help`。

## MCP 接入

### 方向一：把宠物变成其他 agent 的 MCP 工具

`hyprpet mcp serve` 让宠物成为 MCP server（stdio），其他 agent 获得 9 个控制工具：`say` `act` `move_to` `skin` `mini` `hide` `show` `menu` `skins` + `status`（见下节）。

```bash
hyprpet mcp setup all          # 一键注册到：claude-code / claude-desktop / codex /
                               # pi / opencode / chatgpt-desktop / gemini / cursor / vscode
hyprpet mcp setup claude-code  # 或单个
```

注册用当前二进制绝对路径，JSON/TOML 合并保留已有配置，**改前自动备份**（`<file>.hyprpet-bak-<时间戳>`）。重启对应 agent 后，Claude Code 里一句"让宠物说任务完成了"即可调用。

### 方向二：hyprpet 使用其他来源的 MCP 工具

```bash
hyprpet mcp import all   # 从 claude-code(~/.claude.json) / claude-desktop / codex /
                         # pi(~/.pi/agent/mcp.json) / opencode / chatgpt-desktop /
                         # gemini / cursor / vscode 导入，格式自动识别、按名去重
hyprpet mcp add <name> <command> [args...]
hyprpet mcp list / remove <name>
hyprpet mcp export all   # 反向：把 hyprpet 的 [[mcp]] 列表推送到其他 agent 配置
```

运行中的宠物**热加载**：MCP 列表变化自动重建连接，新对话即可调用。仅支持 stdio 类型（远程 sse/http 自动跳过并提示）。

## Agent 状态联动

`status` 工具（或 socket `state` 命令）同步 agent 工作状态，宠物以**头顶彩色状态点 + 动作 + 专属音效**反馈：

| 状态 | 效果 | 音效 |
|---|---|---|
| `thinking` | 🟡 黄点 + study 陪读 | pop |
| `working` | 🔵 蓝点 + study 循环 | pop |
| `done` | 🟢 绿点 + celebrate 蹦跳（6s 自动消退） | 成功上行琶音 |
| `warning` | 🟠 橙点 + alert 警觉 | 警告双音 |
| `error` | 🔴 红点 + 疲惫擦汗 | 错误低音 |
| `confirm` | 🟣 紫点（保持直到 idle） | 清脆确认音 |

```bash
# 脚本 / CI / hook
echo "state working 编译中…"  | socat - UNIX-CONNECT:$XDG_RUNTIME_DIR/hyprpet.sock
echo "state done 构建成功！"  | socat - UNIX-CONNECT:$XDG_RUNTIME_DIR/hyprpet.sock
```

典型工作流：任务开始 `status working` → 完成 `status done`（蹦跳庆祝）→ 异常 `status error` → 等你确认 `status confirm`。

## 外部控制 socket

任何脚本 / agent / CI 可通过 `$XDG_RUNTIME_DIR/hyprpet.sock` 一行命令指挥宠物：

```bash
SOCK=$XDG_RUNTIME_DIR/hyprpet.sock
echo "say 构建完成！"        | socat - UNIX-CONNECT:$SOCK   # 气泡说话 + alert
echo "act celebrate"         | socat - UNIX-CONNECT:$SOCK   # 做动作
echo "pos 100 200"           | socat - UNIX-CONNECT:$SOCK   # 移动
echo "skin builtin_slime"    | socat - UNIX-CONNECT:$SOCK   # 换肤
echo "skins"                 | socat - UNIX-CONNECT:$SOCK   # 列皮肤
echo "mini"                  | socat - UNIX-CONNECT:$SOCK   # mini 开关
echo "hide" / "show" / "menu" / "settings" | socat - UNIX-CONNECT:$SOCK
echo "state done 成功"       | socat - UNIX-CONNECT:$SOCK   # 状态联动
```

## Hyprland 集成

```ini
layerrule = noanim, hyprpet
bind = SUPER CTRL, P, exec, killall -s USR1 hyprpet   # 穿透
bind = SUPER CTRL, C, exec, killall -s USR2 hyprpet   # 聊天
bind = SUPER CTRL, M, exec, killall -s USR3 hyprpet   # mini
```

通知守护进程（quickshell/swaync 等不广播 Notify 信号）通过 DBus BecomeMonitor 模式监听，无需改动通知配置。

## 配置文件总览

| 文件 | 内容 | 热重载 |
|---|---|---|
| `~/.config/hyprpet/config.toml` | `[behavior]` 漫游阈值 | ✓ |
| `~/.config/hyprpet/agent.toml` | `[llm]` `[security]` `[[mcp]]` | ✓（新对话） |
| `~/.config/hyprpet/characters/*/` | 角色（meta/sprites/actions） | ✓ |
| `~/.config/hyprpet/state.toml` | 当前角色 / mini / 位置（自动记忆） | — |
| `~/.config/hyprpet/sounds/*.wav` | 内置音效（自动生成，可替换） | — |

## 架构

```
src/
├── main.rs       # 主循环：多屏 surface、鼠标/键盘/IME 交互、地板物理、
│                 #   探头状态机、空闲门控、感知、事件总线、控制 socket
├── agent.rs      # AI Agent：OpenAI/Anthropic 工具调用循环、内置工具、MCP 客户端
├── mcp_serve.rs  # 宠物即 MCP server（rmcp stdio，9+1 个工具）
├── cli.rs        # hyprpet mcp 子命令（add/remove/list/import/serve/setup/export）
├── chat.rs       # 像素聊天对话框（气泡历史 + 输入行）
├── settings.rs   # LLM 设置弹窗（表单 + 掩码 + 保存）
├── hypr.rs       # Hyprland IPC：socket2 事件源 + j/clients 窗口缓存
├── assets.rs     # 角色加载（TOML + 网格/Aseprite 双格式）
├── fsm.rs        # 数据驱动动作状态机
├── physics.rs    # 抛掷物理（重力/终端速度）
├── renderer.rs   # wl_shm SlotPool 渲染（buffer 复用 + damage）
├── sprite.rs     # 图集 + nearest blit（双向裁剪/垂直翻转）
├── builtin_cat.rs / builtin_slime.rs  # 程序生成内置角色（8×4 帧动画表）
├── ui.rs         # 字体渲染、配置菜单（勾选项/分隔线）、通知气泡
├── tray.rs       # SNI 系统托盘（ksni）
├── audio.rs      # rodio 音效 + 多音合成器（5 种内置音效）
└── pet.rs        # 宠物状态
```

关键设计：

- **每显示器一个全屏 surface + 全局布局坐标**：移动零协议开销；可见原点按 `logical_size - surface_size` 补偿（顶栏 exclusive zone）
- **input region 四态**：常态 = 宠物 bbox ∩ 各屏；拖拽/菜单 = 全屏；弹窗 = 宠物 ∪ 对话框；穿透 = 空
- **弹窗焦点**：`OnDemand` 键盘交互（点击获得焦点、点其他窗口自动让出），键盘 leave 自动关弹窗
- **线程模型**：主线程 calloop（Wayland 事件 + 60Hz tick + fd 源）；后台线程：信号 / 文件监视 / DBus monitor / Agent / 托盘，经 channel 汇入
- **安全**：shell 仅白名单前缀可执行；MCP 工具由用户显式配置启用；配置写入前强制备份

## 测试

```bash
cargo test   # 图集布局 / PNG 往返颜色 / FSM 转移 / 内置角色生成
```

## 路线图

- [ ] Phase 5 候选：Agent hook 模板仓库（pi/Claude Code 任务生命周期自动 status 联动）
- [ ] 更多内置角色（像素小章鱼/团子…）
- [ ] 配置文件 GUI（行为阈值/音效音量进设置弹窗）
- [ ] 多宠物同屏
