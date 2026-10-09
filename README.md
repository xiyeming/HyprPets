# hyprpet

Hyprland 像素桌面宠物（Rust + Wayland layer-shell）。

一只住在桌面上的像素小猫：自主漫游、踩窗口行走、抓起抛掷、通知气泡，双击还能聊天调用 AI 工具。

## 功能状态

**Phase 1 ✅** 底层：layer-shell overlay 多屏 surface、wl_shm 像素渲染、穿透模式
**Phase 2 ✅** 像素引擎：`actions.toml` 数据驱动 FSM、抛掷物理、热重载、右键换肤菜单
**Phase 3 ✅** 感知：窗口地板物理、全屏避让、DBus 通知气泡、rodio 音效
**Phase 4 ✅** Agent：AI 聊天对话框、LLM 工具调用、MCP 插件、系统状态感知

## 交互速查

| 操作 | 效果 |
|---|---|
| 左键拖拽 / 快速甩动 | 移动 / 重力抛掷 |
| 悬停 / 单击 | 悬停睁大眼看你了 / 单击抚摸（开心 + 音效） |
| 左键**双击**（或 `kill -USR2`） | 打开 AI 聊天对话框（支持 fcitx5 中文输入） |
| 右键 | 换肤菜单（像素小猫 / 像素史莱姆…） |
| `kill -USR1` | 切换鼠标穿透 |
| `kill -USR3` | 切换 mini 模式（缩小 · 停漫游 · 可固定任意位置） |
| mini 下单击 / 双击 | 收进屏幕边缘（滑入动画，周期性探头偷看）/ 弹出 |
| 右键菜单 | 换肤 + 配置：mini/穿透勾选、漫游阈值循环、收边、退出 |
| 系统托盘 | SNI 图标在 bar，左键切 mini，右键菜单同配置项 |

mini 模式：宠物缩小为 2x，自主行为全部暂停，拖到哪固定在哪；收边后只剩边缘小把手，点击把手或双击弹出。模式与位置自动记忆。

## Agent 联动（控制 socket）

任何脚本 / agent（pi、Claude Code、CI…）都可以通过 Unix socket 指挥宠物：

```bash
SOCK=$XDG_RUNTIME_DIR/hyprpet.sock
echo "say 构建完成！" | socat - UNIX-CONNECT:$SOCK     # 说话气泡
echo "act celebrate" | socat - UNIX-CONNECT:$SOCK      # 做动作
echo "skin builtin_slime" | socat - UNIX-CONNECT:$SOCK # 换皮肤
echo "pos 100 200" | socat - UNIX-CONNECT:$SOCK        # 移动
echo "mini" | socat - UNIX-CONNECT:$SOCK               # mini 开关
echo "hide" | socat - UNIX-CONNECT:$SOCK               # 收边
echo "skins" | socat - UNIX-CONNECT:$SOCK              # 列出角色
```

聊天里也可 `/mini` `/hide` `/show`。

聊天内命令（无需 LLM）：`/timer <分钟> [事项]`、`/status`、`/shell <命令>`、`/help`

## 构建与运行

```bash
cargo build --release
./target/release/hyprpet
./target/release/hyprpet -s 6 --pos 100,200
```

Hyprland 集成：

```ini
layerrule = noanim, hyprpet
bind = SUPER CTRL, P, exec, killall -s USR1 hyprpet   # 穿透
bind = SUPER CTRL, C, exec, killall -s USR2 hyprpet   # 聊天
```

## AI 配置（`~/.config/hyprpet/agent.toml`）

```toml
[llm]
backend = "openai"            # openai 兼容 / anthropic 原生
base_url = "https://api.openai.com/v1"
api_key = "env:OPENAI_API_KEY"   # 支持 env:NAME / cmd:命令 / 字面量
model = "gpt-4o-mini"

[security]
shell_allow = ["ls", "date", "uptime", "df", "free", "cat /proc"]

```

**方式三（CLI 快速接入）**：从主流 agent 工具一键导入已配置的 MCP server：

```bash
hyprpet mcp import all        # 扫描 claude-code / claude-desktop / codex / pi /
                              # opencode / chatgpt-desktop / gemini / cursor / vscode
hyprpet mcp import codex      # 或指定单个来源
hyprpet mcp add myserver npx -y some-mcp-server
hyprpet mcp list / remove <name>
```

导入去重、写回 agent.toml，运行中的宠物热加载（MCP 连接自动重建）。
hyprpet 仅支持 stdio 类型 MCP（远程 http/sse 自动跳过并提示）。

## 把宠物变成其他 agent 的 MCP 工具（反向接入）

`hyprpet mcp serve` 把宠物本身变成 MCP server（stdio），暴露 say/act/move_to/
skin/mini/hide/show/menu/skins 九个工具，其他 agent 就能原生指挥桌宠：

```bash
hyprpet mcp setup all        # 一键注册到全部 agent（claude-code/claude-desktop/
                             # codex/pi/opencode/chatgpt-desktop/gemini/cursor/vscode）
hyprpet mcp setup claude-code   # 或单个
hyprpet mcp export all          # 把 hyprpet 的 [[mcp]] 列表推送到其他 agent 配置
```

注册使用当前二进制绝对路径，JSON/TOML 合并且保留已有配置。重启对应 agent 后，
Claude Code 里就能调用 `say` 让宠物播报任务结果、Codex 里能 `act celebrate` 庆祝构建成功。

内置工具：`get_system_status`、`set_timer`、`get_clipboard`、`run_command`（白名单），以及所有已配置 MCP server 的工具。

## 行为配置（~/.config/hyprpet/config.toml）



修改保存后自动热生效。

## 角色格式

```text
~/.config/hyprpet/characters/<名字>/
├── meta.toml      # name/author/scale/frame_width/frame_height/preview
├── sprites.png    # 网格表或 Aseprite 整图
├── sprites.json   # 可选（Aseprite 导出，支持 frameTags）
└── actions.toml   # 动作状态机
```

`actions.toml` 关键字段：`fps` / `frames`（网格 `[col,row]` 或 aseprite `tag`）/ `loop` / `duration` / `movement`（px/s）/ `auto_turn` / `next_random` / `sound`。

保留动作名：`idle` `walk_left/right` `sleep` `grabbed` `fall` `land` `petted` `alert` `tired` `study` `celebrate`（缺失自动回落 idle）。

## 架构

```
src/
├── main.rs       # 主循环：多屏 surface、鼠标/键盘交互、地板物理、感知、事件总线
├── agent.rs      # AI Agent：OpenAI/Anthropic 工具调用循环、内置工具、MCP（rmcp+tokio 线程）
├── chat.rs       # 像素聊天对话框（历史气泡 + 输入行 + 光标）
├── hypr.rs       # Hyprland IPC：socket2 事件源 + j/clients 窗口缓存
├── assets.rs     # 角色加载（TOML + 网格/Aseprite）
├── fsm.rs        # 动作状态机        ├── physics.rs  # 抛掷物理
├── renderer.rs   # wl_shm SlotPool    ├── sprite.rs   # 图集 + nearest blit
├── builtin_cat.rs# 内置 32 帧小猫     ├── ui.rs       # 字体/菜单/气泡
├── audio.rs      # rodio 音效         └── pet.rs      # 宠物状态
```

线程模型：主线程 calloop（Wayland 事件 + 定时器 + fd 源）；后台线程仅信号监听、文件监视、DBus、zbus、Agent（各自 channel 汇入）。

## 测试

```bash
cargo test
```
