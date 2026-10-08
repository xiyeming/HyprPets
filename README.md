# hyprpet

Hyprland 像素桌面宠物（Rust + Wayland layer-shell）。

在任意工作区/显示器角落养一只像素小猫：无边框、透明背景、置顶 overlay 层，鼠标可拖拽，支持一键切穿透模式。

## 当前状态（Phase 1 ✅）

- [x] `wlr-layer-shell` Overlay 层全屏 surface（跨 workspace 可见，多显示器由 compositor 分配）
- [x] wl_shm 软件渲染管道，nearest 采样像素缩放（1x/2x/4x/8x），无模糊
- [x] 鼠标拖拽移动（拖拽时自动扩大 input region 防断拖）
- [x] 穿透模式切换（SIGUSR1），穿透时宠物降至 40% 不透明度作视觉反馈
- [x] 默认贴图自动生成（`~/.config/hyprpet/pet.png`，32x32 像素小猫）
- [x] 内存占用 ~17MB（含 3 帧全屏 shm buffer）

## 构建与运行

```bash
cargo build --release
./target/release/hyprpet            # 默认贴图 + 右下角
./target/release/hyprpet -s 6       # 6 倍缩放
./target/release/hyprpet -i ~/pets/slime.png --pos 100,200
./target/release/hyprpet --debug-fill FFFF0000   # 调试：纯色填充整帧
```

参数：

| 参数 | 说明 |
|---|---|
| `-i, --image <路径>` | 宠物贴图 PNG（默认 `~/.config/hyprpet/pet.png`，缺失时自动生成） |
| `-s, --scale <倍率>` | 像素放大倍率（默认 4） |
| `--pos <x,y>` | 初始位置（默认右下角，留边 48px） |

## Hyprland 集成

`hyprland.conf` 建议：

```ini
# 按 namespace 匹配 layer 规则
layerrule = noanim, hyprpet

# 快捷键：切换鼠标穿透
bind = SUPER CTRL, P, exec, killall -s USR1 hyprpet
```

## 架构

```
src/
├── main.rs      # SCTK + calloop 主循环、layer surface、鼠标交互、信号控制
├── renderer.rs  # wl_shm SlotPool 渲染：创建 buffer → 绘制 → attach → damage → commit
├── sprite.rs    # PNG 加载/生成、预乘 ARGB、nearest 放大 blit
└── pet.rs       # 宠物状态：位置/缩放/穿透/悬停/拖拽
```

关键设计：

- **全屏 surface + 内部坐标**：宠物移动只是改内部坐标重绘，零 Wayland 协议开销、无窗口闪烁
- **input region 三态**：常态 = 宠物 bbox；拖拽 = 全屏（防指针脱离断拖）；穿透 = 空 region
- **calloop `run` 语义**：`dispatch(None)` 无事件时阻塞，callback 只在事件后执行 —— 初始帧在进入主循环前手动 flush（`on_idle`）

## 路线图

- [ ] Phase 2：Aseprite JSON / `actions.toml` 数据驱动 FSM、重力抛掷物理、热重载、右键换肤菜单
- [ ] Phase 3：Hyprland IPC 感知（窗口攀爬/全屏避让）、zbus 通知气泡、rodio 音效
- [ ] Phase 4：MCP/rmcp Agent 接入、像素对话框、LLM 工具调用

## 测试

```bash
cargo test   # 含 PNG 往返颜色回归测试（u8 溢出 bug）
```
