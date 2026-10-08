# hyprpet

Hyprland 像素桌面宠物（Rust + Wayland layer-shell）。

在任意工作区/显示器角落养一只像素小猫：自主发呆、踱步、睡觉，可以抓起抛掷，右键一键换肤，配置热重载。

## 当前状态

**Phase 1 ✅**
- [x] `wlr-layer-shell` Overlay 层全屏 surface（跨 workspace 可见）
- [x] **多显示器**：每屏一个 surface + 全局布局坐标，可自由拖过屏幕边界；显示器热插拔自动增删
- [x] wl_shm 软件渲染管道，nearest 采样像素缩放，无模糊，跨屏自动裁剪
- [x] 穿透模式切换（SIGUSR1）

**Phase 2 ✅**
- [x] **数据驱动 FSM**：`actions.toml` 定义动作（fps / 帧序列 / loop / duration / movement / auto_turn / next_random）
- [x] **自主行为**：idle（眨眼）→ walk（左右踱步、边界自动转向）→ sleep（Zzz）随机漫游
- [x] **物理交互**：抓起（grabbed）、快速甩动松手触发重力抛掷（fall → land）、轻放落地
- [x] **角色系统**：`characters/<名字>/` 目录，网格表 + Aseprite JSON 双格式，内置 32 帧像素小猫
- [x] **热重载**：修改 `actions.toml` / 贴图自动生效，无需重启
- [x] **右键换肤菜单**：系统字体渲染角色名 + 贴图预览 + 悬停高亮，选择记忆

## 构建与运行

```bash
cargo build --release
./target/release/hyprpet
./target/release/hyprpet -s 6                # 默认缩放（角色 meta.toml 的 scale 优先）
./target/release/hyprpet --pos 100,200       # 初始位置（全局布局坐标）
./target/release/hyprpet --debug-fill FF0000 # 调试：纯色填充整帧
```

交互：左键拖拽 / 快速甩动抛掷；右键换肤菜单；`kill -USR1 <pid>` 切穿透。

## 角色格式

```text
~/.config/hyprpet/characters/
└── <角色名>/
    ├── meta.toml      # 名称/作者/默认缩放/帧尺寸
    ├── sprites.png    # 贴图（网格表或 Aseprite 整图）
    ├── sprites.json   # 可选，Aseprite 导出（存在则优先，支持 frameTags）
    └── actions.toml   # 动作状态机
```

`meta.toml`：

```toml
name = "像素小猫"
scale = 4
frame_width = 32      # 网格格式必填
frame_height = 32
preview = [0, 0]      # 菜单预览帧
```

`actions.toml`：

```toml
default = "idle"

[actions.idle]
fps = 2               # 帧率
loop = true           # 循环播放
duration = 6          # loop 动作持续秒数（超时转移）
frames = [[0,0],[0,0],[1,0],[0,0]]   # 网格 [col,row]；aseprite 格式用 tag = "walk"
next_random = ["walk_left", "walk_right", "sleep"]

[actions.walk_left]
fps = 8
movement = [-70, 0]   # 像素/秒
auto_turn = true      # 碰到布局边界自动切镜像动作（_left ↔ _right）
# ...
```

保留动作名有特殊语义：`grabbed`（抓起）、`fall`（下落）、`land`（落地）、`idle`（默认）。缺失时自动回落。

## Hyprland 集成

```ini
layerrule = noanim, hyprpet
bind = SUPER CTRL, P, exec, killall -s USR1 hyprpet
```

## 架构

```
src/
├── main.rs       # SCTK + calloop 主循环、多屏 surface、鼠标交互、信号/热重载
├── assets.rs     # 角色目录加载：meta/actions TOML、网格/Aseprite 解析
├── fsm.rs        # 数据驱动动作状态机（tick 推进 + 随机转移）
├── physics.rs    # 抛掷物理（重力/终端速度）
├── renderer.rs   # wl_shm SlotPool 渲染（buffer 复用 + damage）
├── sprite.rs     # 图集（多帧）+ nearest 放大 blit（双向裁剪）
├── builtin_cat.rs# 内置像素小猫 32 帧动画表（程序生成）
├── ui.rs         # 系统字体文字渲染（fontdb+fontdue）+ 像素菜单
└── pet.rs        # 宠物状态：位置/缩放/标志
```

关键设计：

- **每显示器一个全屏 surface + 全局布局坐标**：宠物移动只是改全局坐标重绘；可见原点按 `logical_size - surface_size` 补偿（顶部 bar 的 exclusive zone）
- **input region 四态**：常态 = 宠物 bbox ∩ 各屏；拖拽/菜单 = 全屏（跨屏不断流）；穿透 = 空
- **全局坐标 = 屏幕物理坐标**：指针事件经各 surface origin 换算，渲染按 viewport 裁剪
- **tick 循环**：calloop 60Hz 定时器，FSM 动画 + 物理积分只在脏时重绘

## 路线图

- [ ] Phase 3：Hyprland IPC 感知（窗口攀爬/全屏避让）、zbus 通知气泡、rodio 音效
- [ ] Phase 4：MCP/rmcp Agent 接入、像素对话框、LLM 工具调用

## 测试

```bash
cargo test   # 图集布局 / PNG 往返颜色 / FSM 转移 / 内置角色生成
```
