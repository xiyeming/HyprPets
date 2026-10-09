//! hyprpet — Hyprland 像素桌面宠物
//!
//! 架构要点：
//! - 每个显示器一个 Layer::Overlay 全屏 surface，宠物使用全局布局坐标，
//!   跨屏拖动 = 全局坐标越过显示器边界，自动在目标 surface 上绘制（双向裁剪）
//! - input region 每 surface 独立：常态 = 宠物 bbox 与该 surface 的交集；
//!   拖拽 = 所有 surface 全屏；菜单 = 所有 surface 全屏（模态）；穿透 = 空 region
//! - 角色 = characters/<dir>（meta.toml + sprites.png[+sprites.json] + actions.toml），
//!   notify 热重载，右键菜单换肤
//! - 渲染走 wl_shm SlotPool + nearest 采样，主线程事件循环（Wayland 要求）

mod agent;
mod assets;
mod audio;
mod builtin_cat;
mod builtin_slime;
mod chat;
mod cli;
mod mcp_serve;
mod settings;
mod tray;
mod fsm;
mod hypr;
mod pet;
mod physics;
mod renderer;
mod sprite;
mod ui;

use std::time::Instant;

use anyhow::{Context, Result};
use calloop::channel::{self, Event as ChannelEvent};
use calloop::timer::TimeoutAction;
use calloop::EventLoop;
use calloop_wayland_source::WaylandSource;
use notify::{RecursiveMode, Watcher};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::seat::keyboard::{KeyEvent, KeyboardHandler, Modifiers};
use smithay_client_toolkit::seat::pointer::{PointerEvent, PointerEventKind, PointerHandler};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{
    delegate_compositor, delegate_keyboard, delegate_layer, delegate_output, delegate_pointer,
    delegate_registry, delegate_seat, delegate_shm,
};
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::wl_keyboard::WlKeyboard;
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_pointer::WlPointer;
use wayland_client::protocol::wl_region::WlRegion;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{delegate_noop, Connection, Dispatch, QueueHandle};
use wayland_protocols::wp::text_input::zv3::client::zwp_text_input_manager_v3::ZwpTextInputManagerV3;
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notification_v1::{self, ExtIdleNotificationV1};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notifier_v1::ExtIdleNotifierV1;
use wayland_protocols::wp::text_input::zv3::client::zwp_text_input_v3::{self, ZwpTextInputV3};

use crate::agent::{AgentEvent, AgentRequest};
use crate::assets::Character;
use crate::chat::{Chat, Who};
use crate::audio::Audio;
use crate::fsm::Fsm;
use crate::hypr::{HyprSockets, WinCache};
use crate::pet::{Pet, PetMode};
use crate::physics::Physics;
use crate::renderer::Renderer;
use crate::settings::Settings;
use crate::tray::TrayMsg;
use crate::ui::{Bubble, FontKit, Menu, MenuCmd, MenuItem as UiMenuItem};

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;

/// config.toml 根结构
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default)]
struct PetConfig {
    behavior: BehaviorConfig,
}

/// 行为配置（~/.config/hyprpet/config.toml 的 [behavior] 表）
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default)]
struct BehaviorConfig {
    /// 无电脑活动多少秒后才允许自主移动
    move_after_idle_seconds: u64,
}

impl Default for BehaviorConfig {
    fn default() -> Self {
        BehaviorConfig { move_after_idle_seconds: 120 }
    }
}

impl BehaviorConfig {
    fn load() -> BehaviorConfig {
        let path = assets::characters_dir().join("../config.toml");
        let path = path.canonicalize().unwrap_or(path);
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| toml::from_str::<PetConfig>(&s).ok())
            .map(|c| c.behavior)
            .unwrap_or_default()
    }
}

/// 来自信号线程 / 文件监视器的事件
enum ControlEvent {
    TogglePassthrough,
    OpenChat,
    ToggleMini,
    Reload,
    Notify(String),
    Quit,
}

/// 收边方向
#[derive(Clone, Copy, PartialEq)]
enum EdgeDir {
    Left,
    Right,
    Top,
    Bottom,
}

/// 客户端键盘自动重复状态（合成器/fcitx5 不发重复事件时本地驱动）
struct KeyRepeat {
    raw_code: u32,
    keysym: smithay_client_toolkit::seat::keyboard::Keysym,
    utf8: Option<String>,
    next_fire: Instant,
    rate: u32,
}

/// 收边探头状态机相位
#[derive(Clone, Copy, PartialEq)]
enum PeekPhase {
    /// 滑入边缘（anchor → tucked）
    HideSlide,
    /// 藏好等待，随机时长后探头
    Tucked,
    /// 向外探出（tucked → peek）
    OutSlide,
    /// 探出张望，随机时长后缩回
    Peek,
    /// 缩回（peek → tucked）
    InSlide,
    /// 用户触发弹出（当前位置 → anchor）
    ShowSlide,
}

/// 收边探头状态：藏入边缘后周期性探出半张脸张望
struct EdgePeek {
    dir: EdgeDir,
    phase: PeekPhase,
    t: f32,
    /// Tucked / Peek 的目标停留时长
    wait: f32,
    from: (i32, i32),
    tucked: (i32, i32),
    peek: (i32, i32),
    anchor: (i32, i32),
}

fn ease_out(k: f32) -> f32 {
    1.0 - (1.0 - k) * (1.0 - k)
}

fn ease_in(k: f32) -> f32 {
    k * k
}

fn lerp_pos(a: (i32, i32), b: (i32, i32), k: f32) -> (i32, i32) {
    (
        (a.0 as f32 + (b.0 - a.0) as f32 * k).round() as i32,
        (a.1 as f32 + (b.1 - a.1) as f32 * k).round() as i32,
    )
}

/// 简易随机（时间种子），避免引入 rand 依赖
fn rnd_range(min: f32, max: f32) -> f32 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 ^ (d.as_secs() << 16))
        .unwrap_or(0);
    min + (nanos % 1000) as f32 / 1000.0 * (max - min)
}

/// 一个显示器对应的 layer surface 及其状态
struct PetSurface {
    output: WlOutput,
    /// 保持 layer surface 存活（drop 即销毁）
    #[allow(dead_code)]
    layer: LayerSurface,
    wl_surface: WlSurface,
    /// 该显示器在全局布局中的原点（可见区域，已补偿 exclusive zone）
    origin: (i32, i32),
    /// surface 尺寸（= 显示器可用区域分辨率）
    size: (u32, u32),
    /// 显示器名称（Hyprland monitor name，用于窗口匹配）
    name: String,
    configured: bool,
}

struct App {
    qh: QueueHandle<App>,
    compositor_state: CompositorState,
    seat_state: SeatState,
    output_state: OutputState,
    shm_state: Shm,
    registry_state: RegistryState,
    /// 保持 global 绑定存活
    #[allow(dead_code)]
    layer_shell: LayerShell,
    /// 系统托盘句柄
    tray_handle: Option<ksni::blocking::Handle<tray::PetTray>>,
    /// 保持文件监视器存活
    #[allow(dead_code)]
    watcher: Option<notify::RecommendedWatcher>,
    /// Hyprland IPC（无则功能降级）
    hypr: Option<HyprSockets>,
    win_cache: WinCache,
    /// 窗口缓存刷新防抖时间点
    win_refresh_at: Option<Instant>,

    audio: Audio,
    bubble: Option<Bubble>,
    chat: Option<Chat>,
    settings: Option<Settings>,
    agent_tx: Option<std::sync::mpsc::Sender<AgentRequest>>,
    timers: Vec<(Instant, String)>,
    /// text-input-v3（IME 输入）
    behavior: BehaviorConfig,
    seat: Option<WlSeat>,
    idle_notifier: Option<ExtIdleNotifierV1>,
    idle_notification: Option<ExtIdleNotificationV1>,
    /// 用户是否处于活动状态（false = 已空闲，宠物可漫游）
    user_active: bool,
    ti_manager: Option<ZwpTextInputManagerV3>,
    text_input: Option<ZwpTextInputV3>,
    ti_entered: bool,
    /// enter 事件携带的焦点 surface（光标坐标换算用）
    ti_surface: Option<WlSurface>,
    /// 本轮 done 周期内是否收到过 preedit_string
    ti_preedit_pending: bool,
    /// mini 模式：进入前的缩放（恢复用）
    normal_scale: u32,
    mini_scale: u32,
    /// 收边探头状态机（Some = 已收边）
    edge_peek: Option<EdgePeek>,
    /// 当前键盘修饰键（Ctrl+V 粘贴用）
    mods: Modifiers,
    /// 合成器报告的重复速率/延迟（次/秒, 毫秒）
    key_repeat: Option<(u32, u32)>,
    /// 进行中的按键重复
    key_repeat_state: Option<KeyRepeat>,
    /// 左键按下起点（单击判定）
    press_start: Option<(f64, f64)>,
    /// 本次悬停是否已触发过反应（避免重复）
    hover_reacted: bool,
    /// 双击检测：上次左键按下的时间与位置
    last_press: Option<(Instant, (f64, f64))>,
    /// 状态感知冷却
    last_sense: Instant,
    sense_acc: f32,
    cursor_acc: f32,
    keyboard: Option<WlKeyboard>,

    /// 每显示器一个全屏 surface
    surfaces: Vec<PetSurface>,
    renderer: Option<Renderer>,

    character: Option<Character>,
    fsm: Option<Fsm>,
    physics: Physics,
    menu: Option<Menu>,
    /// 菜单关闭后字体缓存保留
    font: Option<FontKit>,

    pet: Pet,
    /// 初始位置是否已处理（--pos 或已放置）
    placed: bool,

    pointer: Option<WlPointer>,
    pointer_pos: (f64, f64),
    /// 拖拽中保存的 指针位置-宠物左上角 偏移（全局坐标）
    drag_offset: Option<(f64, f64)>,
    /// 拖拽速度采样（最近 150ms）
    drag_samples: Vec<((f64, f64), Instant)>,

    /// 热重载信号（在 on_idle 中合并处理）
    pending_reload: bool,
    last_tick: Instant,
    last_reload: Instant,

    /// 调试用：纯色填充整帧
    debug_fill: Option<u32>,

    needs_redraw: bool,
    region_dirty: bool,
}

impl App {
    // ---------- 布局 ----------

    /// 所有已配置 surface 的联合包围盒（全局坐标）
    fn layout_bbox(&self) -> Option<(i32, i32, i32, i32)> {
        let mut bbox: Option<(i32, i32, i32, i32)> = None;
        for s in &self.surfaces {
            if !s.configured {
                continue;
            }
            let (x0, y0) = s.origin;
            let (x1, y1) = (x0 + s.size.0 as i32, y0 + s.size.1 as i32);
            bbox = Some(match bbox {
                None => (x0, y0, x1, y1),
                Some(b) => (b.0.min(x0), b.1.min(y0), b.2.max(x1), b.3.max(y1)),
            });
        }
        bbox
    }

    fn ground_y(&self, bbox: (i32, i32, i32, i32)) -> i32 {
        bbox.3 - self.pet.height_px()
    }

    fn create_surface_for(&mut self, output: &WlOutput) {
        if self.surfaces.iter().any(|s| s.output == *output) {
            return;
        }
        let (origin, mon_name) = self
            .output_state
            .info(output)
            .map(|i| {
                (
                    i.logical_position.unwrap_or(i.location),
                    i.name.clone().unwrap_or_default(),
                )
            })
            .unwrap_or(((0, 0), String::new()));

        let wl_surface = self.compositor_state.create_surface(&self.qh);
        let layer = self.layer_shell.create_layer_surface(
            &self.qh,
            wl_surface.clone(),
            Layer::Overlay,
            Some("hyprpet"),
            Some(output),
        );
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_size(0, 0);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.set_exclusive_zone(0);
        wl_surface.commit();

        self.surfaces.push(PetSurface {
            output: output.clone(),
            layer,
            wl_surface,
            origin,
            size: (0, 0),
            name: mon_name.clone(),
            configured: false,
        });
        eprintln!("[hyprpet] 新显示器 surface: {} origin=({},{})", mon_name, origin.0, origin.1);
    }

    // ---------- 角色 ----------

    /// 加载角色并重建 FSM，返回是否成功
    fn apply_character(&mut self, dir: &std::path::Path) -> bool {
        match Character::load(dir) {
            Ok(character) => {
                self.pet.w = character.sheet.frame_w;
                self.pet.h = character.sheet.frame_h;
                if let Some(scale) = character.meta.scale {
                    // mini 模式下不重置当前缩放（只更新正常态记忆值），
                    // 否则热重载/换肤会把 mini 宠物弹回全尺寸
                    self.normal_scale = scale.max(1);
                    if !self.pet.is_mini() {
                        self.pet.scale = scale.max(1);
                    }
                }
                let mut fsm = Fsm::new(character.actions.clone(), &character.default_action);
                // 尽量延续切换前的动作名
                if let Some(prev) = self.fsm.as_ref().map(|f| f.current.clone()) {
                    fsm.set(&prev);
                }
                self.fsm = Some(fsm);
                if let Some(h) = &self.tray_handle {
                    let icon = tray::tray_icon_from_sheet(&character.sheet, character.preview_frame(), 22);
                    h.update(|t| t.icon = icon);
                }
                self.character = Some(character);
                if let Some(b) = self.layout_bbox() {
                    // 收边隐藏中的位置是刻意越界的，钳制会把它弹回屏内
                    if !self.mini_hidden() {
                        self.pet.clamp_to(b.0, b.1, b.2, b.3);
                    }
                }
                self.region_dirty = true;
                self.needs_redraw = true;
                true
            }
            Err(e) => {
                eprintln!("[hyprpet] 加载角色 {} 失败: {e:#}", dir.display());
                false
            }
        }
    }

    fn reload_character(&mut self) {
        self.pending_reload = false;
        // 节流：一次文件保存会触发多个事件，300ms 内合并
        if self.last_reload.elapsed() < std::time::Duration::from_millis(300) {
            return;
        }
        self.last_reload = Instant::now();
        // 行为配置热重载
        self.behavior = BehaviorConfig::load();
        self.recreate_idle_notification();
        let Some(dir) = self.character.as_ref().map(|c| c.dir.clone()) else {
            return;
        };
        if self.apply_character(&dir) {
            eprintln!("[hyprpet] 角色已热重载");
        }
    }

    fn save_current_character(&self) {
        let Some(c) = &self.character else { return };
        let name = c.dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        // 收边隐藏中保存恢复位置（anchor），避免重启后宠物卡在边缘
        let (sx, sy) = self
            .edge_peek
            .as_ref()
            .map(|ep| ep.anchor)
            .unwrap_or((self.pet.x, self.pet.y));
        let state = format!(
            "current = \"{name}\"\nmode = \"{mode}\"\nx = {x}\ny = {y}\n",
            mode = if self.pet.is_mini() { "mini" } else { "normal" },
            x = sx,
            y = sy
        );
        let path = assets::characters_dir().join("state.toml");
        std::fs::write(path, state).ok();
    }

    /// 切换角色（菜单点击）
    fn switch_character(&mut self, dir: &std::path::Path) {
        if self.apply_character(dir) {
            self.save_current_character();
            eprintln!("[hyprpet] 已切换角色: {}", dir.display());
        }
    }

    fn set_action(&mut self, name: &str) -> bool {
        let switched = {
            let Some(fsm) = &mut self.fsm else { return false };
            if fsm.set(name) {
                true
            } else {
                // 角色没有该动作时回落 idle
                name != "idle" && fsm.set("idle")
            }
        };
        if switched {
            self.play_action_sound();
        }
        switched
    }

    fn play_action_sound(&self) {
        let (Some(c), Some(fsm)) = (&self.character, &self.fsm) else { return };
        let Some(def) = c.actions.get(&fsm.current) else { return };
        if let Some(sound) = &def.sound {
            self.audio.play(&c.dir.join(sound));
        }
    }

    /// 宠物中心点所在显示器
    fn monitor_of(&self, x: i32) -> Option<&PetSurface> {
        self.surfaces
            .iter()
            .find(|s| s.configured && x >= s.origin.0 && x < s.origin.0 + s.size.0 as i32)
    }

    fn monitor_rect_of(&self, x: i32) -> Option<(i32, i32, i32, i32)> {
        self.monitor_of(x).map(|m| {
            (m.origin.0, m.origin.1, m.origin.0 + m.size.0 as i32, m.origin.1 + m.size.1 as i32)
        })
    }

    /// 宠物 x 范围内的候选地板：显示器底缘 + 该显示器可见工作区的窗口顶缘
    /// 宠物 x 范围内的候选地板（**可站立的宠物 y** = 表面 y - 宠物高度）：
    /// 显示器下沿与可见工作区窗口顶缘
    fn floors_at(&self, x0: i32, x1: i32) -> Vec<i32> {
        let mut v = Vec::new();
        let Some(mon) = self.monitor_of((x0 + x1) / 2) else { return v };
        let stand = mon.origin.1 + mon.size.1 as i32 - self.pet.height_px();
        v.push(stand);
        for w in self.win_cache.visible_windows_on(&mon.name) {
            if w.fullscreen > 0 {
                continue;
            }
            if w.x < x1 && w.x + w.w > x0 {
                v.push(w.y - self.pet.height_px());
            }
        }
        v.sort_unstable();
        v
    }

    /// 地板状态：站立 / 下方有空可坠落 / 前方是墙（地板在上方）
    fn floor_status(&self, x0: i32, x1: i32, y: i32) -> FloorStatus {
        let floors = self.floors_at(x0, x1);
        if let Some(&f) = floors.iter().find(|&&f| (f - y).abs() <= 4) {
            return FloorStatus::OnFloor(f);
        }
        if let Some(&f) = floors.iter().find(|&&f| f > y + 4) {
            return FloorStatus::Gap(f);
        }
        FloorStatus::Wall
    }

    /// 全屏避让：所在显示器有可见全屏窗口时淡出并暂停漫游
    /// 气泡过期 / 定时器 / 光标闪烁 / 系统感知（mini 与普通共用）
    fn tick_timers_bubble_cursor(&mut self, dt: f32, now: Instant) {
        if let Some(b) = &self.bubble {
            if b.until <= now {
                self.bubble = None;
                self.needs_redraw = true;
            }
        }

        let due: Vec<String> = self
            .timers
            .iter()
            .filter(|(t, _)| *t <= now)
            .map(|(_, l)| l.clone())
            .collect();
        if !due.is_empty() {
            self.timers.retain(|(t, _)| *t > now);
            for label in due {
                self.bubble = Some(Bubble {
                    text: format!("⏰ {label}"),
                    until: now + std::time::Duration::from_secs(8),
                });
                self.audio.play(&audio::sounds_dir().join("pop.wav"));
                self.set_action("alert");
            }
            self.needs_redraw = true;
        }

        if self.chat.is_some() || self.settings.is_some() {
            self.cursor_acc += dt;
            if self.cursor_acc >= 0.5 {
                self.cursor_acc = 0.0;
                if let Some(c) = &mut self.chat {
                    c.toggle_cursor();
                }
                if let Some(st) = &mut self.settings {
                    st.toggle_cursor();
                }
                self.needs_redraw = true;
            }
        }

        if self.pet.is_mini() {
            return;
        }
        self.sense_acc += dt;
        if self.sense_acc >= 5.0 {
            self.sense_acc = 0.0;
            self.system_sense(now);
        }
    }

    /// 收边探头状态机：藏入 → 随机等待 → 探出张望 → 缩回，循环
    fn tick_peek(&mut self, dt: f32) {
        let (mut new_pos, mut finished, mut action) = (None, false, None);
        if let Some(ep) = &mut self.edge_peek {
            ep.t += dt;
            match ep.phase {
                PeekPhase::HideSlide => {
                    let k = (ep.t / 0.35).min(1.0);
                    new_pos = Some(lerp_pos(ep.from, ep.tucked, ease_out(k)));
                    if k >= 1.0 {
                        ep.phase = PeekPhase::Tucked;
                        ep.t = 0.0;
                        ep.wait = rnd_range(2.5, 7.0);
                    }
                }
                PeekPhase::Tucked => {
                    if ep.t >= ep.wait {
                        ep.phase = PeekPhase::OutSlide;
                        ep.t = 0.0;
                    }
                }
                PeekPhase::OutSlide => {
                    let k = (ep.t / 0.4).min(1.0);
                    new_pos = Some(lerp_pos(ep.tucked, ep.peek, ease_out(k)));
                    if k >= 1.0 {
                        ep.phase = PeekPhase::Peek;
                        ep.t = 0.0;
                        ep.wait = rnd_range(1.2, 2.6);
                        action = Some("alert");
                    }
                }
                PeekPhase::Peek => {
                    if ep.t >= ep.wait {
                        ep.phase = PeekPhase::InSlide;
                        ep.t = 0.0;
                        action = Some("idle");
                    }
                }
                PeekPhase::InSlide => {
                    let k = (ep.t / 0.3).min(1.0);
                    new_pos = Some(lerp_pos(ep.peek, ep.tucked, ease_in(k)));
                    if k >= 1.0 {
                        ep.phase = PeekPhase::Tucked;
                        ep.t = 0.0;
                        ep.wait = rnd_range(2.5, 7.0);
                    }
                }
                PeekPhase::ShowSlide => {
                    let k = (ep.t / 0.4).min(1.0);
                    new_pos = Some(lerp_pos(ep.from, ep.anchor, ease_out(k)));
                    if k >= 1.0 {
                        finished = true;
                    }
                }
            }
        }
        if let Some(p) = new_pos {
            self.pet.x = p.0;
            self.pet.y = p.1;
            self.needs_redraw = true;
        }
        if let Some(a) = action {
            self.set_action(a);
        }
        if finished {
            self.edge_peek = None;
            if let Some(b) = self.layout_bbox() {
                self.pet.clamp_to(b.0, b.1, b.2, b.3);
            }
            self.region_dirty = true;
            self.save_current_character();
        }
    }

    /// 系统状态感知：高负载 → tired；检测到编码进程 → study
    fn system_sense(&mut self, now: Instant) {
        if self.physics.active || self.drag_offset.is_some() || self.chat.is_some() {
            return;
        }
        // 冷却
        if now.duration_since(self.last_sense) < std::time::Duration::from_secs(90) {
            return;
        }
        let load = std::fs::read_to_string("/proc/loadavg")
            .ok()
            .and_then(|s| s.split_whitespace().next().and_then(|v| v.parse::<f64>().ok()))
            .unwrap_or(0.0);
        let cores = std::thread::available_parallelism().map(|n| n.get() as f64).unwrap_or(4.0);
        let mem_avail_pct = std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|s| {
                let mut lines = s.lines();
                let total = lines
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1).and_then(|v| v.parse::<f64>().ok()))?;
                let avail = s
                    .lines()
                    .find(|l| l.starts_with("MemAvailable"))
                    .and_then(|l| l.split_whitespace().nth(1).and_then(|v| v.parse::<f64>().ok()))?;
                Some(avail / total * 100.0)
            })
            .unwrap_or(100.0);

        if load > cores * 1.5 || mem_avail_pct < 8.0 {
            if self.set_action("tired") {
                self.last_sense = now;
            }
            return;
        }

        // 编码进程检测（claude / codex / cursor-agent / cargo / rustc）
        if let Ok(entries) = std::fs::read_dir("/proc") {
            let busy = entries.flatten().any(|e| {
                let pid = e.file_name().to_string_lossy().chars().all(|c| c.is_ascii_digit());
                if !pid {
                    return false;
                }
                std::fs::read_to_string(e.path().join("comm"))
                    .map(|c| {
                        let c = c.trim();
                        ["claude", "codex", "cursor-agent", "cargo", "rustc", "gemini", "pi"]
                            .iter()
                            .any(|p| c.starts_with(p))
                    })
                    .unwrap_or(false)
            });
            if busy && self.pet.hovered {
                // 仅当用户看着宠物时进入陪读
                if self.set_action("study") {
                    self.last_sense = now;
                }
            }
        }
    }

    /// 宠物 bbox 是否与可见窗口实体相交（窗口侧壁/穿模检测）
    fn window_blocking(&self, x0: i32, y0: i32) -> bool {
        let x1 = x0 + self.pet.width_px();
        let y1 = y0 + self.pet.height_px();
        let Some(mon) = self.monitor_of((x0 + x1) / 2) else { return false };
        self.win_cache.visible_windows_on(&mon.name).any(|w| {
            w.fullscreen == 0 && w.x < x1 && w.x + w.w > x0 && w.y < y1 && w.y + w.h > y0
        })
    }

    fn evading(&self) -> bool {
        let cx = self.pet.x + self.pet.width_px() / 2;
        self.monitor_of(cx)
            .map(|m| self.win_cache.has_fullscreen_on(&m.name))
            .unwrap_or(false)
    }

    // ---------- 菜单 ----------

    /// 构造菜单条目（换肤 + 配置项，状态实时）
    fn menu_items(&self) -> Vec<UiMenuItem> {
        let mut items: Vec<UiMenuItem> = assets::scan_characters()
            .into_iter()
            .filter_map(|(name, dir)| {
                Character::load(&dir).ok().map(|c| {
                    let preview_idx = c.preview_frame();
                    UiMenuItem::Skin { name, dir, preview: c.sheet, preview_idx }
                })
            })
            .collect();
        if !items.is_empty() {
            items.push(UiMenuItem::Separator);
        }
        items.push(UiMenuItem::Action {
            label: format!("漫游阈值：{}s（点击切换）", self.behavior.move_after_idle_seconds),
            checked: None,
            cmd: MenuCmd::CycleIdle,
        });
        items.push(UiMenuItem::Action {
            label: "mini 模式".into(),
            checked: Some(self.pet.is_mini()),
            cmd: MenuCmd::ToggleMini,
        });
        items.push(UiMenuItem::Action {
            label: "鼠标穿透".into(),
            checked: Some(self.pet.passthrough),
            cmd: MenuCmd::TogglePassthrough,
        });
        items.push(UiMenuItem::Action {
            label: if self.mini_hidden() { "弹出边缘" } else { "收进边缘" }.into(),
            checked: None,
            cmd: MenuCmd::HideEdge,
        });
        items.push(UiMenuItem::Action {
            label: "打开聊天".into(),
            checked: None,
            cmd: MenuCmd::OpenChat,
        });
        items.push(UiMenuItem::Action {
            label: "LLM 设置…".into(),
            checked: None,
            cmd: MenuCmd::OpenSettings,
        });
        items.push(UiMenuItem::Separator);
        items.push(UiMenuItem::Action {
            label: "退出 hyprpet".into(),
            checked: None,
            cmd: MenuCmd::Quit,
        });
        items
    }

    fn open_menu(&mut self) {
        self.ensure_font();
        self.close_chat();
        let Some(font) = self.font.take() else {
            eprintln!("[hyprpet] 无可用系统字体，菜单不可用");
            return;
        };
        let entries = self.menu_items();
        if entries.is_empty() {
            self.font = Some(font);
            return;
        }
        let Some(bbox) = self.layout_bbox() else {
            self.font = Some(font);
            return;
        };
        self.menu = Some(Menu::layout(
            entries,
            (self.pet.x, self.pet.y),
            self.pet.height_px(),
            bbox,
            font,
        ));
        self.region_dirty = true;
        self.needs_redraw = true;
    }

    /// 托盘/菜单共用的命令执行
    fn execute_cmd(&mut self, cmd: MenuCmd) {
        match cmd {
            MenuCmd::ToggleMini => self.toggle_mini(),
            MenuCmd::TogglePassthrough => self.toggle_passthrough(),
            MenuCmd::OpenChat => {
                if !self.pet.is_mini() {
                    self.open_chat();
                }
            }
            MenuCmd::OpenSettings => {
                if !self.pet.is_mini() {
                    self.open_settings();
                }
            }
            MenuCmd::HideEdge => {
                if !self.pet.is_mini() {
                    self.toggle_mini();
                }
                if self.mini_hidden() {
                    self.mini_show();
                } else {
                    self.mini_hide();
                }
            }
            MenuCmd::Quit => std::process::exit(0),
            MenuCmd::CycleIdle => {
                const STEPS: [u64; 4] = [30, 60, 120, 300];
                let cur = self.behavior.move_after_idle_seconds;
                let next = STEPS.iter().find(|&&s| s > cur).copied().unwrap_or(STEPS[0]);
                self.behavior.move_after_idle_seconds = next;
                self.recreate_idle_notification();
                // 持久化到 config.toml（watcher 会热重载，双写无害）
                let cfg = assets::characters_dir().join("../config.toml");
                let _ = std::fs::write(
                    cfg,
                    format!("[behavior]\nmove_after_idle_seconds = {next}\n"),
                );
            }
        }
        self.sync_tray();
    }

    /// 托盘勾选状态同步
    fn sync_tray(&mut self) {
        if let Some(h) = &self.tray_handle {
            let mini = self.pet.is_mini();
            let passthrough = self.pet.passthrough;
            let idle = self.behavior.move_after_idle_seconds;
            let hidden = self.mini_hidden();
            h.update(|t| {
                t.mini = mini;
                t.passthrough = passthrough;
                t.idle_secs = idle;
                t.hidden_edge = hidden;
            });
        }
    }

    fn close_menu(&mut self) {
        if let Some(menu) = self.menu.take() {
            self.font = Some(menu.font);
        }
        self.region_dirty = true;
        self.needs_redraw = true;
    }

    // ---------- 输入区域 ----------

    /// - 穿透：空 region
    /// - 拖拽或菜单：所有 surface 全屏（跨屏拖动 / 模态菜单不断流）
    /// - 常态：宠物 bbox 与各 surface 的交集
    fn apply_input_region(&mut self) {
        let fullscreen = self.drag_offset.is_some() || self.menu.is_some();
        // 弹窗（聊天/设置）开放时 region = 宠物 ∪ 弹窗，点击外面直达其他窗口
        let mut rects: Vec<(i32, i32, i32, i32)> = Vec::new();
        if !fullscreen {
            let (px, py) = self
                .edge_peek
                .as_ref()
                .map(|ep| ep.peek)
                .unwrap_or((self.pet.x, self.pet.y));
            rects.push((px, py, self.pet.width_px(), self.pet.height_px()));
            if let Some(c) = &self.chat {
                let (x, y, w, h) = c.rect;
                rects.push((x, y, w as i32, h as i32));
            }
            if let Some(st) = &self.settings {
                let (x, y, w, h) = st.rect;
                rects.push((x, y, w as i32, h as i32));
            }
        }
        for s in &self.surfaces {
            if self.pet.passthrough {
                s.wl_surface.set_input_region(None);
            } else {
                let region = self.compositor_state.wl_compositor().create_region(&self.qh, ());
                if fullscreen {
                    region.add(0, 0, s.size.0 as i32, s.size.1 as i32);
                } else {
                    for &(rx, ry, rw, rh) in &rects {
                        let x0 = rx.max(s.origin.0);
                        let y0 = ry.max(s.origin.1);
                        let x1 = (rx + rw).min(s.origin.0 + s.size.0 as i32);
                        let y1 = (ry + rh).min(s.origin.1 + s.size.1 as i32);
                        if x0 < x1 && y0 < y1 {
                            region.add(x0 - s.origin.0, y0 - s.origin.1, x1 - x0, y1 - y0);
                        }
                    }
                }
                s.wl_surface.set_input_region(Some(&region));
                region.destroy();
            }
            s.wl_surface.commit();
        }
        self.region_dirty = false;
    }

    /// 懒加载系统字体（菜单/气泡共用）
    fn ensure_font(&mut self) {
        if self.font.is_none() {
            self.font = FontKit::load();
        }
    }

    // ---------- mini 模式 ----------

    /// 切换 mini / 正常模式
    fn toggle_mini(&mut self) {
        if self.pet.is_mini() {
            self.pet.mode = PetMode::Normal;
            self.pet.scale = self.normal_scale.max(1);
            self.edge_peek = None;
        } else {
            self.close_chat();
            self.pet.mode = PetMode::Mini;
            self.normal_scale = self.pet.scale;
            self.pet.scale = self.mini_scale.max(1);
            self.physics.stop();
            self.set_action("idle");
        }
        if let Some(b) = self.layout_bbox() {
            self.pet.clamp_to(b.0, b.1, b.2, b.3);
        }
        self.region_dirty = true;
        self.needs_redraw = true;
        self.save_current_character();
        eprintln!("[hyprpet] 模式: {}", if self.pet.is_mini() { "mini" } else { "正常" });
    }

    /// 收进最近的屏幕边缘：滑入动画 + 周期性探头偷看；非 mini 自动先进入 mini
    fn mini_hide(&mut self) {
        if !self.pet.is_mini() {
            self.toggle_mini();
        }
        if self.edge_peek.is_some() {
            return;
        }
        let cx = self.pet.x + self.pet.width_px() / 2;
        let Some(m) = self.monitor_rect_of(cx) else { return };
        const TAB: i32 = 6;
        /// 探头时露出的比例
        const PEEK_RATIO: f32 = 0.55;
        let (w, h) = (self.pet.width_px(), self.pet.height_px());
        let cy = self.pet.y + h / 2;
        // 与邻屏共享的边不能藏：身体会跨到邻屏完整显示出来
        let adjoin = |other: &PetSurface| -> bool {
            other.configured
                && (other.origin.0 - m.2).abs() <= 1
                && (other.origin.1 - m.1).abs() <= 1
        };
        let blocked_left = self.surfaces.iter().any(|s| s.configured && (s.origin.0 + s.size.0 as i32 - m.0).abs() <= 1);
        let blocked_right = self.surfaces.iter().any(|s| adjoin(s) && s.origin.0 >= m.2 - 1);
        let blocked_top = self
            .surfaces
            .iter()
            .any(|s| s.configured && (s.origin.1 + s.size.1 as i32 - m.1).abs() <= 1);
        let blocked_bottom = self.surfaces.iter().any(|s| adjoin(s) && s.origin.1 >= m.3 - 1);
        let dl = if blocked_left { i32::MAX } else { cx - m.0 };
        let dr = if blocked_right { i32::MAX } else { m.2 - cx };
        let dt = if blocked_top { i32::MAX } else { cy - m.1 };
        let db = if blocked_bottom { i32::MAX } else { m.3 - cy };
        let min = dl.min(dr).min(dt).min(db);
        let pw = (w as f32 * PEEK_RATIO) as i32;
        let ph = (h as f32 * PEEK_RATIO) as i32;
        let (dir, tucked, peek) = if min == dl {
            (EdgeDir::Left, (m.0 - w + TAB, self.pet.y), (m.0 - w + pw, self.pet.y))
        } else if min == dr {
            (EdgeDir::Right, (m.2 - TAB, self.pet.y), (m.2 - pw, self.pet.y))
        } else if min == dt {
            (EdgeDir::Top, (self.pet.x, m.1 - h + TAB), (self.pet.x, m.1 - h + ph))
        } else {
            (EdgeDir::Bottom, (self.pet.x, m.3 - TAB), (self.pet.x, m.3 - ph))
        };
        self.edge_peek = Some(EdgePeek {
            dir,
            phase: PeekPhase::HideSlide,
            t: 0.0,
            wait: 0.0,
            from: (self.pet.x, self.pet.y),
            tucked,
            peek,
            anchor: (self.pet.x, self.pet.y),
        });
        self.region_dirty = true;
        self.needs_redraw = true;
        self.save_current_character();
    }

    /// 从边缘弹出：滑回原位（动画）
    fn mini_show(&mut self) {
        if !self.pet.is_mini() {
            return;
        }
        if let Some(ep) = &mut self.edge_peek {
            ep.from = (self.pet.x, self.pet.y);
            ep.phase = PeekPhase::ShowSlide;
            ep.t = 0.0;
        } else if let Some(b) = self.layout_bbox() {
            self.pet.x = (b.0 + b.2) / 2 - self.pet.width_px() / 2;
            self.pet.y = b.3 - self.pet.height_px() - 60;
            self.region_dirty = true;
            self.needs_redraw = true;
        }
    }

    fn mini_hidden(&self) -> bool {
        self.edge_peek.is_some()
    }

    /// 顶部收边且在隐藏相位：sprite 垂直翻转，头朝屏内探出
    fn pet_flip_y(&self) -> bool {
        match &self.edge_peek {
            Some(ep) => {
                ep.dir == EdgeDir::Top
                    && matches!(
                        ep.phase,
                        PeekPhase::Tucked | PeekPhase::OutSlide | PeekPhase::Peek | PeekPhase::InSlide
                    )
            }
            None => false,
        }
    }

    // ---------- 外部控制（agent 联动） ----------

    /// 处理控制 socket 命令，返回响应行
    fn handle_ctl(&mut self, line: &str) -> String {
        let mut it = line.split_whitespace();
        let cmd = it.next().unwrap_or("");
        match cmd {
            "say" => {
                let text: String = it.collect::<Vec<_>>().join(" ");
                if text.is_empty() {
                    return "err: say <text>".into();
                }
                self.bubble = Some(Bubble {
                    text: text.chars().take(60).collect(),
                    until: Instant::now() + std::time::Duration::from_secs(6),
                });
                self.audio.play(&audio::sounds_dir().join("pop.wav"));
                self.set_action("alert");
                self.needs_redraw = true;
                "ok".into()
            }
            "act" => {
                let name = it.next().unwrap_or("");
                if name.is_empty() {
                    return "err: act <action>".into();
                }
                if self.set_action(name) {
                    self.needs_redraw = true;
                    "ok".into()
                } else {
                    format!("err: 无动作 {name}")
                }
            }
            "pos" => {
                let (Ok(x), Ok(y)) = (it.next().unwrap_or("").parse::<i32>(), it.next().unwrap_or("").parse::<i32>()) else {
                    return "err: pos <x> <y>".into();
                };
                self.pet.x = x;
                self.pet.y = y;
                self.physics.stop();
                if self.edge_peek.is_some() {
                    self.edge_peek = None;
                }
                if let Some(b) = self.layout_bbox() {
                    self.pet.clamp_to(b.0, b.1, b.2, b.3);
                }
                self.region_dirty = true;
                self.needs_redraw = true;
                "ok".into()
            }
            "skin" => {
                let name = it.next().unwrap_or("");
                let dir = assets::characters_dir().join(name);
                if dir.is_dir() {
                    self.switch_character(&dir);
                    "ok".into()
                } else {
                    format!("err: 无角色 {name}")
                }
            }
            "mini" => {
                self.toggle_mini();
                "ok".into()
            }
            "hide" => {
                self.mini_hide();
                "ok".into()
            }
            "show" => {
                self.mini_show();
                "ok".into()
            }
            "menu" => {
                self.open_menu();
                "ok".into()
            }
            "settings" => {
                self.open_settings();
                "ok".into()
            }
            "skins" => assets::scan_characters()
                .iter()
                .map(|(n, _)| n.clone())
                .collect::<Vec<_>>()
                .join(","),
            _ => "err: 未知命令（say/act/pos/skin/skins/mini/hide/show）".into(),
        }
    }

    fn on_notification(&mut self, summary: String) {
        self.ensure_font();
        self.bubble = Some(Bubble { text: summary, until: Instant::now() + std::time::Duration::from_secs(5) });
        self.audio.play(&audio::sounds_dir().join("pop.wav"));
        self.set_action("alert");
        self.needs_redraw = true;
    }

    fn toggle_passthrough(&mut self) {
        self.pet.passthrough = !self.pet.passthrough;
        self.region_dirty = true;
        self.needs_redraw = true;
        eprintln!("[hyprpet] 穿透模式: {}", if self.pet.passthrough { "开" } else { "关" });
    }

    // ---------- 渲染 ----------

    fn redraw(&mut self) -> Result<()> {
        let frame = self.fsm.as_ref().map(|f| f.frame()).unwrap_or(0);
        let sheet = self.character.as_ref().map(|c| &c.sheet);
        let menu = self.menu.as_ref();
        let bubble = self.bubble.as_ref();
        let chat = self.chat.as_ref();
        let settings = self.settings.as_ref();
        let font = self.font.as_ref();
        let evade = self.evading();
        let flip_y = self.pet_flip_y();
        let pet = &self.pet;
        let debug_fill = self.debug_fill;
        let mon_rect = self.monitor_rect_of(pet.x + pet.width_px() / 2);
        let Some(renderer) = &mut self.renderer else {
            return Ok(());
        };

        for i in 0..self.surfaces.len() {
            let s = &self.surfaces[i];
            if !s.configured || s.size.0 == 0 || s.size.1 == 0 {
                continue;
            }
            let wl_surface = s.wl_surface.clone();
            let (width, height, viewport) = (s.size.0, s.size.1, s.origin);
            renderer
                .present(&wl_surface, width, height, viewport, |pixels, buf_w, vp| {
                    if let Some(c) = debug_fill {
                        pixels.fill(c);
                        return;
                    }
                    let alpha = if pet.passthrough {
                        102
                    } else if evade {
                        110
                    } else {
                        255
                    };
                    if let Some(sheet) = sheet {
                        let rect = sheet.frame(frame);
                        sheet.blit_frame(
                            pixels, buf_w, vp, rect, (pet.x, pet.y), pet.scale, alpha, flip_y,
                        );
                    }
                    if let (Some(b), Some(f), Some(mr)) = (bubble, font, mon_rect) {
                        ui::draw_bubble(
                            b,
                            f,
                            pixels,
                            buf_w,
                            vp,
                            (pet.x, pet.y, pet.width_px(), pet.height_px()),
                            mr,
                        );
                    }
                    if let Some(c) = chat {
                        if let Some(f) = font {
                            c.draw(f, pixels, buf_w, vp);
                        }
                    }
                    if let (Some(st), Some(f)) = (settings, font) {
                        st.draw(f, pixels, buf_w, vp);
                    }
                    if let Some(m) = menu {
                        m.draw(pixels, buf_w, vp);
                    }
                })
                .context("present 失败")?;
        }
        Ok(())
    }

    fn place_pet_if_needed(&mut self) {
        if self.placed {
            return;
        }
        if let Some((x0, y0, x1, y1)) = self.layout_bbox() {
            self.pet.x = x1 - self.pet.width_px() - 48;
            self.pet.y = y1 - self.pet.height_px() - 48;
            self.pet.clamp_to(x0, y0, x1, y1);
            self.placed = true;
        }
    }

    // ---------- 主循环 tick ----------

    fn on_tick(&mut self) {
        let now = Instant::now();
        let dt = now.duration_since(self.last_tick).as_secs_f32().min(0.05);
        self.last_tick = now;

        self.tick_key_repeat();

        let Some(bbox) = self.layout_bbox() else { return };
        let mut moved = false;
        let mut animated = false;

        // mini 模式：固定不动（仍走气泡/定时器/光标/跟随分支）
        if self.pet.is_mini() {
            if let Some(fsm) = &mut self.fsm {
                if fsm.tick(dt) {
                    animated = true;
                }
            }
            if animated {
                self.needs_redraw = true;
            }
            self.tick_peek(dt);
            self.tick_timers_bubble_cursor(dt, now);
            return;
        }

        if self.physics.active {
            // 抛掷/下落：重力积分 + 地板穿越检测（可落在窗口顶缘）
            let old_y = self.pet.y;
            self.pet.x = (self.pet.x as f32 + self.physics.vx * dt).round() as i32;
            self.pet.y = (self.pet.y as f32 + self.physics.vy * dt).round() as i32;
            self.physics.vy = (self.physics.vy + Physics::GRAVITY * dt).min(Physics::TERMINAL);
            moved = true;
            // 水平方向钳制在所在显示器内
            if let Some(m) = self.monitor_rect_of(self.pet.x + self.pet.width_px() / 2) {
                let max_x = m.2 - self.pet.width_px();
                let nx = self.pet.x.clamp(m.0, max_x.max(m.0));
                if nx != self.pet.x {
                    self.pet.x = nx;
                    self.physics.vx = 0.0;
                }
            }
            // 地板穿越：旧位置之下、新位置之上的最高地板
            let x0 = self.pet.x;
            let x1 = x0 + self.pet.width_px();
            let landed = self
                .floors_at(x0, x1)
                .into_iter()
                .filter(|&f| f >= old_y - 1 && f <= self.pet.y)
                .min();
            if let Some(f) = landed {
                let impact = self.physics.vy;
                self.pet.y = f;
                self.physics.stop();
                if impact > 700.0 {
                    self.set_action("land");
                } else {
                    self.set_action("idle");
                }
                animated = true;
            } else {
                // 天花板：向上抛出不越出屏幕顶，停在顶部由重力回落
                if self.pet.y < bbox.1 {
                    self.pet.y = bbox.1;
                    self.physics.vy = 0.0;
                }
                self.pet.y = self.pet.y.min(bbox.3 - self.pet.height_px());
            }
        } else if self.drag_offset.is_none() {
            let def = self.fsm.as_ref().and_then(|f| f.def().cloned());
            if let Some(def) = def {
                // 贴地（地板可能是窗口顶缘）
                let x0 = self.pet.x;
                let x1 = x0 + self.pet.width_px();
                match self.floor_status(x0, x1, self.pet.y) {
                    FloorStatus::OnFloor(f) => {
                        if self.pet.y != f {
                            self.pet.y = f;
                            moved = true;
                        }
                    }
                    FloorStatus::Gap(_) => {
                        // 走出地板边缘 → 坠落
                        self.physics.toss(0.0, 0.0);
                        self.set_action("fall");
                        animated = true;
                    }
                    FloorStatus::Wall => {
                        // 被放置在墙沿上，保持现状
                    }
                }

                // FSM 驱动的位移（行走）：仅在用户空闲时允许
                if let Some([mx, my]) = def.movement {
                    let evading = self.evading() || self.user_active;
                    if self.user_active && !evading {
                        // 活动状态强制 idle（停止漫游）
                        if self.set_action("idle") {
                            animated = true;
                        }
                    }
                    if !evading {
                        let old_x = self.pet.x;
                        self.pet.x = (self.pet.x as f32 + mx * dt).round() as i32;
                        self.pet.y = (self.pet.y as f32 + my * dt).round() as i32;
                        moved = true;

                        // 显示器边界 = 墙
                        let mut hit_wall = false;
                        if let Some(m) = self.monitor_rect_of(self.pet.x + self.pet.width_px() / 2) {
                            let max_x = m.2 - self.pet.width_px();
                            if self.pet.x < m.0 || self.pet.x > max_x {
                                self.pet.x = self.pet.x.clamp(m.0, max_x.max(m.0));
                                hit_wall = true;
                            }
                        } else if self.pet.x <= bbox.0 || self.pet.x + self.pet.width_px() >= bbox.2 {
                            self.pet.clamp_to(bbox.0, bbox.1, bbox.2, bbox.3);
                            hit_wall = true;
                        }

                        // 地板状态：走出边缘 → 坠落；撞窗口侧壁 → 转向
                        match self.floor_status(self.pet.x, self.pet.x + self.pet.width_px(), self.pet.y) {
                            FloorStatus::OnFloor(_) => {}
                            FloorStatus::Gap(_) => {
                                self.physics.toss(mx * 0.4, 0.0);
                                self.set_action("fall");
                            }
                            FloorStatus::Wall => {
                                self.pet.x = old_x;
                                hit_wall = true;
                            }
                        }
                        // 窗口实体碰撞（下落掠过窗口上方后站在窗顶不算，站在地板走入窗口区域算）
                        if !hit_wall && self.window_blocking(self.pet.x, self.pet.y) {
                            self.pet.x = old_x;
                            hit_wall = true;
                        }

                        if (def.auto_turn && hit_wall) || (def.auto_turn && (self.pet.x <= bbox.0 || self.pet.x + self.pet.width_px() >= bbox.2)) {
                            let current = self.fsm.as_ref().map(|f| f.current.clone()).unwrap_or_default();
                            let mirrored = mirror_action(&current);
                            if self.set_action(&mirrored) {
                                animated = true;
                            }
                        }
                    }
                }
            }
        }

        self.tick_timers_bubble_cursor(dt, now);

        // 窗口缓存防抖刷新
        if let Some(t) = self.win_refresh_at {
            if now >= t {
                self.win_refresh_at = None;
                if let Some(h) = &self.hypr {
                    self.win_cache = h.refresh_cache();
                    self.needs_redraw = true;
                }
            }
        }

        // FSM 动画推进
        if let Some(fsm) = &mut self.fsm {
            if fsm.tick(dt) {
                animated = true;
            }
        }


        // 对话框跟随宠物移动
        if moved && self.chat.is_some() {
            let bbox = self.layout_bbox();
            if let (Some(c), Some(b)) = (&mut self.chat, bbox) {
                c.follow((self.pet.x, self.pet.y), self.pet.width_px(), self.pet.height_px(), b);
            }
        }

        if moved {
            self.region_dirty = true;
            self.needs_redraw = true;
        } else if animated {
            self.needs_redraw = true;
        }
    }

    // ---------- 指针 ----------

    fn surface_origin_of(&self, surface: &WlSurface) -> Option<(i32, i32)> {
        self.surfaces.iter().find(|s| s.wl_surface == *surface).map(|s| s.origin)
    }

    fn drag_velocity(&self) -> (f64, f64) {
        let now = Instant::now();
        let window: Vec<_> = self
            .drag_samples
            .iter()
            .filter(|(_, t)| now.duration_since(*t).as_millis() <= 150)
            .collect();
        if window.len() < 2 {
            return (0.0, 0.0);
        }
        let ((p0, t0), (p1, t1)) = (window[0], window[window.len() - 1]);
        let dt = t1.duration_since(*t0).as_secs_f64();
        if dt <= 0.0 {
            return (0.0, 0.0);
        }
        ((p1.0 - p0.0) / dt, (p1.1 - p0.1) / dt)
    }

    fn on_pointer_frame(&mut self, events: &[PointerEvent]) {
        for event in events {
            let Some(origin) = self.surface_origin_of(&event.surface) else {
                continue;
            };
            let global = (event.position.0 + origin.0 as f64, event.position.1 + origin.1 as f64);
            match event.kind {
                PointerEventKind::Enter { .. } => {
                    self.pet.hovered = true;
                    // 悬停反应：宠物注意到你（alert 睁大眼；仅在空闲/睡觉时触发一次）
                    if !self.hover_reacted
                        && self.drag_offset.is_none()
                        && self.menu.is_none()
                        && self.chat.is_none()
                        && !self.mini_hidden()
                        && !self.physics.active
                    {
                        let cur = self.fsm.as_ref().map(|f| f.current.clone()).unwrap_or_default();
                        if (cur == "idle" || cur == "sleep") && self.set_action("alert") {
                            self.hover_reacted = true;
                            self.needs_redraw = true;
                        }
                    }
                }
                PointerEventKind::Leave { .. } => {
                    self.pet.hovered = false;
                    self.hover_reacted = false;
                    // 悬停结束：若还停留在悬停触发的 alert 则回 idle
                    if self.drag_offset.is_none()
                        && self.fsm.as_ref().map(|f| f.current.as_str()) == Some("alert")
                    {
                        self.set_action("idle");
                    }
                    // 拖拽/菜单中不取消：跨屏时旧 surface 必然 Leave，
                    // 目标 surface 是全屏 region，会继续推送事件
                }
                PointerEventKind::Motion { .. } => {
                    self.pointer_pos = global;
                    if self.menu.is_some() {
                        let hover = self.menu.as_ref().and_then(|m| m.hit(global));
                        if self.menu.as_ref().map(|m| m.hover).flatten() != hover {
                            if let Some(m) = &mut self.menu {
                                m.hover = hover;
                            }
                            self.needs_redraw = true;
                        }
                        continue;
                    }
                    if let Some((dx, dy)) = self.drag_offset {
                        self.drag_samples.push((global, Instant::now()));
                        let cutoff = Instant::now() - std::time::Duration::from_millis(200);
                        self.drag_samples.retain(|(_, t)| *t >= cutoff);

                        self.pet.x = (global.0 - dx).round() as i32;
                        self.pet.y = (global.1 - dy).round() as i32;
                        if let Some((x0, y0, x1, y1)) = self.layout_bbox() {
                            self.pet.clamp_to(x0, y0, x1, y1);
                        }
                        self.needs_redraw = true;
                        self.region_dirty = true;
                    }
                }
                PointerEventKind::Press { button, .. } => {
                    if button == BTN_LEFT {
                        // 菜单命中：换肤 or 配置命令
                        if self.menu.is_some() {
                            let hit = self.menu.as_ref().and_then(|mm| mm.hit(global));
                            if let Some(idx) = hit {
                                let pick = self.menu.as_ref().and_then(|mm| mm.items.get(idx)).and_then(|it| match it {
                                    UiMenuItem::Skin { dir, .. } => Some((dir.clone(), None)),
                                    UiMenuItem::Action { cmd, .. } => Some((std::path::PathBuf::new(), Some(*cmd))),
                                    UiMenuItem::Separator => None,
                                });
                                match pick {
                                    Some((dir, None)) => {
                                        self.close_menu();
                                        self.switch_character(&dir);
                                    }
                                    Some((_, Some(cmd))) => {
                                        self.execute_cmd(cmd);
                                        // 就地刷新勾选状态（font 留在 menu 内）
                                        if let Some(old) = self.menu.take() {
                                            let bbox = self.layout_bbox();
                                            let items = self.menu_items();
                                            if let Some(b) = bbox {
                                                let hover = old.hover;
                                                let mut new_menu = Menu::layout(
                                                    items,
                                                    (self.pet.x, self.pet.y),
                                                    self.pet.height_px(),
                                                    b,
                                                    old.font,
                                                );
                                                new_menu.hover = hover.filter(|h| *h < new_menu.items.len());
                                                self.menu = Some(new_menu);
                                            } else {
                                                self.font = Some(old.font);
                                            }
                                        }
                                    }
                                    None => {}
                                }
                                continue;
                            }
                            // 点到菜单外：关闭
                            self.close_menu();
                            continue;
                        }
                        // 双击唤起聊天
                        let now = Instant::now();
                        let dbl = self
                            .last_press
                            .map(|(t, p)| {
                                now.duration_since(t) < std::time::Duration::from_millis(350)
                                    && (global.0 - p.0).abs() < 8.0
                                    && (global.1 - p.1).abs() < 8.0
                            })
                            .unwrap_or(false);
                        self.last_press = Some((now, global));
                        if dbl {
                            self.drag_offset = None;
                            self.pet.dragged = false;
                            if self.pet.is_mini() {
                                // mini 双击：已隐藏则弹出，可见则收起
                                if self.mini_hidden() {
                                    self.mini_show();
                                } else {
                                    self.mini_hide();
                                }
                            } else {
                                self.open_chat();
                            }
                            continue;
                        }
                        if self.chat.is_some() {
                            // 聊天模态：点击输入框外关闭
                            let inside = self.chat.as_ref().map(|c| {
                                let (x, y, w, h) = c.rect;
                                global.0 >= x as f64
                                    && global.0 <= (x + w as i32) as f64
                                    && global.1 >= y as f64
                                    && global.1 <= (y + h as i32) as f64
                            });
                            if inside == Some(false) {
                                self.close_chat();
                            }
                            continue;
                        }
                        if self.mini_hidden() {
                            // 点把手弹出
                            self.mini_show();
                            continue;
                        }
                        if !self.pet.passthrough {
                            self.press_start = Some(global);
                            self.drag_offset =
                                Some((global.0 - self.pet.x as f64, global.1 - self.pet.y as f64));
                            self.drag_samples.clear();
                            self.drag_samples.push((global, Instant::now()));
                            self.pet.dragged = true;
                            self.set_action("grabbed");
                            self.region_dirty = true;
                            self.needs_redraw = true;
                        }
                    } else if button == BTN_RIGHT {
                        self.close_chat();
                        if self.menu.is_some() {
                            self.close_menu();
                        } else {
                            self.open_menu();
                        }
                    }
                }
                PointerEventKind::Release { button, .. } => {
                    if button == BTN_LEFT && self.drag_offset.is_some() {
                        let (vx, vy) = self.drag_velocity();
                        self.drag_offset = None;
                        self.drag_samples.clear();
                        self.pet.dragged = false;
                        // 单击判定（位移 <6px）
                        let mut petted = false;
                        if let Some(p0) = self.press_start.take() {
                            let moved = (global.0 - p0.0).abs() > 6.0 || (global.1 - p0.1).abs() > 6.0;
                            if !moved && self.pet.is_mini() && !self.mini_hidden() {
                                self.mini_hide();
                                continue;
                            }
                            // 触摸：单击抚摸 → petted（角色缺失该动作则静默回落）
                            if !moved && !self.pet.is_mini() && !self.physics.active {
                                petted = self.set_action("petted");
                                if petted {
                                    self.audio.play(&audio::sounds_dir().join("pop.wav"));
                                    self.needs_redraw = true;
                                }
                            }
                        }
                        let speed = (vx.hypot(vy)) as f32;
                        // 快速甩动 → 抛掷；或宠物悬在空中 → 小掉落自然落地
                        //（空中判定：距最近可站立地板 >4px）
                        let off_ground = !matches!(
                            self.floor_status(self.pet.x, self.pet.x + self.pet.width_px(), self.pet.y),
                            FloorStatus::OnFloor(_)
                        );
                        // 抚摸触发的释放不再走落地/抛掷逻辑（否则 land 会覆盖 petted）
                        if petted {
                            self.region_dirty = true;
                            continue;
                        }
                        if speed > Physics::TOSS_THRESHOLD || off_ground {
                            self.physics.toss(vx as f32, vy as f32);
                            self.set_action("fall");
                        } else {
                            self.set_action("land");
                        }
                        self.region_dirty = true;
                        self.needs_redraw = true;
                        if self.pet.is_mini() {
                            self.save_current_character();
                        }
                    }
                }
                PointerEventKind::Axis { .. } => {}
            }
        }
    }
}

/// 地板状态
enum FloorStatus {
    /// 站立在地板 f 上
    OnFloor(i32),
    /// 下方最近地板 f（走出边缘）
    Gap(i32),
    /// 地板全在上方（窗口侧壁）
    Wall,
}

/// walk_left ↔ walk_right 镜像命名
fn mirror_action(name: &str) -> String {
    if let Some(base) = name.strip_suffix("_left") {
        format!("{base}_right")
    } else if let Some(base) = name.strip_suffix("_right") {
        format!("{base}_left")
    } else {
        name.to_string()
    }
}

// ---------- SCTK handler 实现 ----------

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        new_factor: i32,
    ) {
        eprintln!("[hyprpet] 缩放系数变化: {new_factor}（当前版本按 1 处理）");
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _surface: &WlSurface, _time: u32) {}

    fn surface_enter(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _surface: &WlSurface, _output: &WlOutput) {}

    fn surface_leave(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _surface: &WlSurface, _output: &WlOutput) {}
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, output: WlOutput) {
        self.create_surface_for(&output);
        self.needs_redraw = true;
    }

    fn update_output(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, output: WlOutput) {
        if let Some(info) = self.output_state.info(&output) {
            let (ox, oy) = info.logical_position.unwrap_or(info.location);
            for s in &mut self.surfaces {
                if s.output == output {
                    s.origin = if s.configured {
                        (ox, oy + (info.logical_size.map(|(_, h)| h as i32).unwrap_or(0) - s.size.1 as i32).max(0))
                    } else {
                        (ox, oy)
                    };
                    if let Some(name) = &info.name {
                        s.name = name.clone();
                    }
                }
            }
            self.place_pet_if_needed();
            self.region_dirty = true;
            self.needs_redraw = true;
        }
    }

    fn output_destroyed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, output: WlOutput) {
        self.surfaces.retain(|s| s.output != output);
        self.region_dirty = true;
        self.needs_redraw = true;
        eprintln!("[hyprpet] 显示器移除，剩余 surface: {}", self.surfaces.len());
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, seat: WlSeat) {
        if self.seat.is_none() {
            self.seat = Some(seat.clone());
            self.recreate_idle_notification();
        }
    }

    fn remove_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: WlSeat) {
        self.pointer = None;
    }

    fn new_capability(&mut self, _conn: &Connection, qh: &QueueHandle<Self>, seat: WlSeat, capability: Capability) {
        // 初始 seat 的 new_seat 回调不会触发（SCTK 0.19 行为），
        // 在 capability 到达时存 seat 并创建空闲通知
        if self.seat.is_none() {
            self.seat = Some(seat.clone());
            self.recreate_idle_notification();
        }
        if capability == Capability::Pointer && self.pointer.is_none() {
            match self.seat_state.get_pointer(qh, &seat) {
                Ok(pointer) => self.pointer = Some(pointer),
                Err(e) => eprintln!("[hyprpet] 获取 pointer 失败: {e}"),
            }
        }
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            match self.seat_state.get_keyboard(qh, &seat, None) {
                Ok(kbd) => self.keyboard = Some(kbd),
                Err(e) => eprintln!("[hyprpet] 获取 keyboard 失败: {e}"),
            }
            if let Some(m) = &self.ti_manager {
                self.text_input = Some(m.get_text_input(&seat, qh, ()));
            }
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer {
            self.pointer = None;
        }
        if capability == Capability::Keyboard {
            self.keyboard = None;
        }
    }
}

// keysym 标准值（xkb）
const KS_RETURN: u32 = 0xFF0D;
const KS_ESCAPE: u32 = 0xFF1B;
const KS_BACKSPACE: u32 = 0xFF08;

impl KeyboardHandler for App {
    fn enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &WlKeyboard,
        _surface: &WlSurface,
        _serial: u32,
        _raw: &[u32],
        _keysyms: &[smithay_client_toolkit::seat::keyboard::Keysym],
    ) {
    }

    fn leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &WlKeyboard,
        _surface: &WlSurface,
        _serial: u32,
    ) {
        // 键盘焦点被让走（用户点击了其他窗口）→ 关闭弹窗，释放输入
        if self.settings.is_some() {
            self.close_settings();
        }
        if self.chat.is_some() {
            self.close_chat();
        }
    }

    fn press_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &WlKeyboard,
        _serial: u32,
        event: KeyEvent,
    ) {
        // 同一键未释放又收到 press = 合成器/IM 发来的合成重复：忽略，
        // 由本地重复计时器统一驱动（避免双重触发）
        if let Some(r) = &self.key_repeat_state {
            if r.raw_code == event.raw_code {
                return;
            }
        }
        self.arm_key_repeat(&event);
        self.on_key_press(&event);
    }

    fn release_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &WlKeyboard,
        _serial: u32,
        event: KeyEvent,
    ) {
        if let Some(r) = &self.key_repeat_state {
            if r.raw_code == event.raw_code {
                self.key_repeat_state = None;
            }
        }
    }

    fn update_modifiers(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &WlKeyboard,
        _serial: u32,
        modifiers: smithay_client_toolkit::seat::keyboard::Modifiers,
        _layout: u32,
    ) {
        self.mods = modifiers;
    }

    fn update_repeat_info(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &WlKeyboard,
        info: smithay_client_toolkit::seat::keyboard::RepeatInfo,
    ) {
        use smithay_client_toolkit::seat::keyboard::RepeatInfo;
        self.key_repeat = match info {
            RepeatInfo::Repeat { rate, delay } => Some((rate.get(), delay)),
            RepeatInfo::Disable => None,
        };
    }

    fn update_keymap(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &WlKeyboard,
        _keymap: smithay_client_toolkit::seat::keyboard::Keymap,
    ) {
    }
}

impl App {
    /// 按下键时武装本地重复（Return/Escape 不重复；速率来自合成器 repeat_info）
    fn arm_key_repeat(&mut self, event: &KeyEvent) {
        self.key_repeat_state = None;
        let Some((rate, delay)) = self.key_repeat else { return };
        if rate == 0 {
            return;
        }
        let code: u32 = event.keysym.into();
        if matches!(code, KS_RETURN | KS_ESCAPE) {
            return;
        }
        self.key_repeat_state = Some(KeyRepeat {
            raw_code: event.raw_code,
            keysym: event.keysym,
            utf8: event.utf8.clone(),
            next_fire: Instant::now() + std::time::Duration::from_millis(delay as u64),
            rate,
        });
    }

    /// 重复计时器到点：重放按键动作
    fn tick_key_repeat(&mut self) {
        let now = Instant::now();
        let fire = self
            .key_repeat_state
            .as_ref()
            .filter(|r| now >= r.next_fire)
            .map(|r| (r.keysym, r.utf8.clone(), r.rate));
        if let Some((keysym, utf8, rate)) = fire {
            if let Some(r) = &mut self.key_repeat_state {
                r.next_fire = now + std::time::Duration::from_millis((1000 / rate.max(1)) as u64);
            }
            self.on_key_press(&KeyEvent { time: 0, raw_code: 0, keysym, utf8 });
        }
    }

    /// 读取系统剪贴板文本
    fn clipboard_text() -> Option<String> {
        let out = std::process::Command::new("wl-paste")
            .arg("--no-newline")
            .arg("--type")
            .arg("text")
            .output()
            .ok()?;
        if out.status.success() {
            Some(String::from_utf8_lossy(&out.stdout).to_string())
        } else {
            None
        }
    }

    /// 当前是否按下 Ctrl+V（keysym v/V + ctrl 修饰）
    fn is_paste(&self, code: u32) -> bool {
        self.mods.ctrl && (code == 0x76 || code == 0x56) // v / V
    }

    /// text-input-v3 是否已就绪（IME 文本走 commit_string 路径）
    fn ti_active(&self) -> bool {
        self.ti_entered && self.text_input.is_some()
    }

    fn ti_enable(&mut self) {
        if let Some(ti) = &self.text_input {
            ti.enable();
            ti.commit();
        }
    }

    /// 向 IME 报告光标矩形（候选框跟随），surface-local 坐标
    fn ti_update_cursor(&mut self) {
        let (Some(ti), Some(chat)) = (&self.text_input, &self.chat) else { return };
        let Some(font) = &self.font else { return };
        let (gx, gy, w, h) = chat.cursor_global(font);
        // 焦点 surface 的原点偏移（全局 → surface-local）
        let local = self
            .ti_surface
            .as_ref()
            .and_then(|s| self.surfaces.iter().find(|ps| ps.wl_surface == *s))
            .map(|ps| (gx - ps.origin.0, gy - ps.origin.1, w, h))
            .unwrap_or((gx, gy, w, h));
        ti.set_cursor_rectangle(local.0, local.1, local.2, local.3);
        ti.commit();
    }

    fn ti_disable(&mut self) {
        if let Some(ti) = &self.text_input {
            ti.disable();
            ti.commit();
        }
        if let Some(c) = &mut self.chat {
            c.preedit.clear();
        }
        self.ti_preedit_pending = false;
    }

    /// 提交聊天输入
    fn submit_chat(&mut self) {
        let text = self
            .chat
            .as_mut()
            .map(|c| {
                let t = std::mem::take(&mut c.input);
                c.preedit.clear();
                t
            })
            .unwrap_or_default();
        if !text.trim().is_empty() {
            if let Some(c) = &mut self.chat {
                c.push(Who::User, &text);
                c.busy = true;
            }
            if let Some(tx) = &self.agent_tx {
                let _ = tx.send(AgentRequest::Chat(text));
            }
        }
        self.needs_redraw = true;
    }

    /// 设置对话框按键：↑↓/Tab 切换字段，Enter 保存，Esc 取消
    fn settings_key(&mut self, event: &KeyEvent) {
        const KS_TAB: u32 = 0xFF09;
        const KS_UP: u32 = 0xFF52;
        const KS_DOWN: u32 = 0xFF54;
        let code: u32 = event.keysym.into();
        if self.is_paste(code) {
            if let (Some(st), Some(text)) = (&mut self.settings, Self::clipboard_text()) {
                // 过滤控制字符，限制长度，追加到聚焦字段
                let clean: String = text.chars().filter(|c| !c.is_control()).take(256).collect();
                if !clean.is_empty() {
                    st.focused_mut().push_str(&clean);
                    self.needs_redraw = true;
                }
            }
            return;
        }
        match code {
            KS_RETURN => {
                if let Some(st) = self.settings.take() {
                    st.save();
                }
                // 保存后收回键盘焦点
                if self.chat.is_none() {
                    for s in &self.surfaces {
                        s.layer.set_keyboard_interactivity(KeyboardInteractivity::None);
                        s.wl_surface.commit();
                    }
                }
                self.bubble = Some(Bubble {
                    text: "设置已保存".into(),
                    until: Instant::now() + std::time::Duration::from_secs(3),
                });
                self.region_dirty = true;
                self.needs_redraw = true;
            }
            KS_ESCAPE => self.close_settings(),
            KS_TAB => {
                if let Some(st) = &mut self.settings {
                    if event.utf8.as_deref() == Some("\u{1b}") {
                        // Shift+Tab
                    }
                    st.focus_next();
                }
                self.needs_redraw = true;
            }
            KS_DOWN => {
                if let Some(st) = &mut self.settings {
                    st.focus_next();
                }
                self.needs_redraw = true;
            }
            KS_UP => {
                if let Some(st) = &mut self.settings {
                    st.focus_prev();
                }
                self.needs_redraw = true;
            }
            KS_BACKSPACE => {
                if let Some(st) = &mut self.settings {
                    st.focused_mut().pop();
                }
                self.needs_redraw = true;
            }
            _ => {
                if let (Some(st), Some(u)) = (&mut self.settings, &event.utf8) {
                    if !u.chars().any(|c| c.is_control()) {
                        st.focused_mut().push_str(u);
                        self.needs_redraw = true;
                    }
                }
            }
        }
    }

    fn on_key_press(&mut self, event: &KeyEvent) {
        if self.settings.is_some() {
            self.settings_key(event);
            return;
        }
        if self.chat.is_none() {
            return;
        }
        let code: u32 = event.keysym.into();
        if self.is_paste(code) {
            if let (Some(chat), Some(text)) = (&mut self.chat, Self::clipboard_text()) {
                let clean: String = text.chars().filter(|c| !c.is_control()).take(500).collect();
                if !clean.is_empty() {
                    chat.input.push_str(&clean);
                    self.ti_update_cursor();
                    self.needs_redraw = true;
                }
            }
            return;
        }
        match code {
            KS_RETURN => self.submit_chat(),
            KS_ESCAPE => self.close_chat(),
            KS_BACKSPACE => {
                self.chat.as_mut().unwrap().input.pop();
                self.ti_update_cursor();
                self.needs_redraw = true;
            }
            _ if !self.ti_active() => {
                // 无 IME：物理按键文本直接输入；IME 激活时文本由 commit_string 提供
                if let Some(u) = &event.utf8 {
                    if !u.chars().any(|c| c.is_control()) {
                        self.chat.as_mut().unwrap().input.push_str(u);
                        self.ti_update_cursor();
                        self.needs_redraw = true;
                    }
                }
            }
            _ => {}
        }
    }

    fn open_chat(&mut self) {
        if self.chat.is_some() {
            return;
        }
        self.close_menu();
        self.ensure_font();
        let Some(bbox) = self.layout_bbox() else { return };
        self.chat = Some(Chat::new(
            (self.pet.x, self.pet.y),
            self.pet.width_px(),
            self.pet.height_px(),
            bbox,
        ));
        // OnDemand：点击获得焦点，点其他窗口焦点自然让出（标准弹窗语义）
        for s in &self.surfaces {
            s.layer.set_keyboard_interactivity(KeyboardInteractivity::OnDemand);
            s.wl_surface.commit();
        }
        // 焦点已就绪（ti_entered）则直接启用 IME；否则等 enter 事件
        if self.ti_entered {
            self.ti_enable();
        }
        self.region_dirty = true;
        self.needs_redraw = true;
    }

    fn open_settings(&mut self) {
        if self.settings.is_some() {
            return;
        }
        self.close_menu();
        self.close_chat();
        self.ensure_font();
        let Some(bbox) = self.layout_bbox() else { return };
        self.settings = Some(Settings::load(
            (self.pet.x, self.pet.y),
            self.pet.width_px(),
            self.pet.height_px(),
            bbox,
        ));
        for s in &self.surfaces {
            s.layer.set_keyboard_interactivity(KeyboardInteractivity::OnDemand);
            s.wl_surface.commit();
        }
        self.region_dirty = true;
        self.needs_redraw = true;
    }

    fn close_settings(&mut self) {
        if self.settings.is_none() {
            return;
        }
        self.settings = None;
        if self.chat.is_none() {
            for s in &self.surfaces {
                s.layer.set_keyboard_interactivity(KeyboardInteractivity::None);
                s.wl_surface.commit();
            }
        }
        self.region_dirty = true;
        self.needs_redraw = true;
    }

    fn close_chat(&mut self) {
        if self.chat.is_none() {
            return;
        }
        self.chat = None;
        self.ti_disable();
        if self.settings.is_none() {
            for s in &self.surfaces {
                s.layer.set_keyboard_interactivity(KeyboardInteractivity::None);
                s.wl_surface.commit();
            }
        }
        self.region_dirty = true;
        self.needs_redraw = true;
    }

    fn on_agent_reply(&mut self, text: String) {
        if let Some(chat) = &mut self.chat {
            chat.push(Who::Pet, &text);
            chat.busy = false;
        } else {
            // 对话框已关闭：用气泡展示回复
            self.bubble = Some(Bubble {
                text: text.chars().take(60).collect(),
                until: Instant::now() + std::time::Duration::from_secs(6),
            });
            self.audio.play(&audio::sounds_dir().join("pop.wav"));
        }
        self.needs_redraw = true;
    }

    fn on_agent_timer(&mut self, minutes: f32, label: String) {
        self.timers.push((Instant::now() + std::time::Duration::from_secs((minutes * 60.0) as u64), label));
    }
}

impl PointerHandler for App {
    fn pointer_frame(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _pointer: &WlPointer, events: &[PointerEvent]) {
        self.on_pointer_frame(events);
    }
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm_state
    }
}

impl LayerShellHandler for App {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _layer: &LayerSurface) {
        eprintln!("[hyprpet] layer surface 被关闭，退出");
        std::process::exit(0);
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let (w, h) = configure.new_size;
        let mut first_any = false;
        for s in &mut self.surfaces {
            if s.layer == *layer {
                if !s.configured {
                    first_any = true;
                }
                s.size = (w, h);
                s.configured = true;
                // 可见原点修正：layer surface 可能被 exclusive zone 下推
                // （如顶部 bar 占 26px），全局坐标系必须基于实际可见区域
                if let Some(info) = self.output_state.info(&s.output) {
                    let (ox, oy) = info.logical_position.unwrap_or(info.location);
                    if let Some((_, lh)) = info.logical_size {
                        s.origin = (ox, oy + (lh as i32 - h as i32).max(0));
                    }
                }
            }
        }
        if first_any && w > 0 && h > 0 && self.renderer.is_none() {
            match Renderer::new(&self.shm_state, w, h) {
                Ok(r) => self.renderer = Some(r),
                Err(e) => eprintln!("[hyprpet] 创建渲染器失败: {e:#}"),
            }
        }
        self.place_pet_if_needed();
        self.region_dirty = true;
        self.needs_redraw = true;
    }
}

impl App {
    /// 按配置的阈值重建空闲通知（配置热更新用）
    fn recreate_idle_notification(&mut self) {
        let (Some(notifier), Some(seat)) = (&self.idle_notifier, &self.seat) else { return };
        if let Some(old) = self.idle_notification.take() {
            old.destroy();
        }
        let timeout_ms = (self.behavior.move_after_idle_seconds.max(1)) as u32 * 1000;
        self.idle_notification =
            Some(notifier.get_idle_notification(timeout_ms, seat, &self.qh, ()));
        eprintln!("[hyprpet] 空闲漫游阈值: {}s", self.behavior.move_after_idle_seconds);
    }
}

/// ext-idle-notify-v1：合成器报告全局输入空闲状态
impl Dispatch<ExtIdleNotificationV1, ()> for App {
    fn event(
        data: &mut App,
        _: &ExtIdleNotificationV1,
        event: ext_idle_notification_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<App>,
    ) {
        match event {
            ext_idle_notification_v1::Event::Idled => {
                data.user_active = false;
            }
            ext_idle_notification_v1::Event::Resumed => {
                data.user_active = true;
                // 活动恢复：立即停止漫游
                if data.set_action("idle") {
                    data.needs_redraw = true;
                }
            }
            _ => {}
        }
    }
}

delegate_noop!(App: ignore ExtIdleNotifierV1);

impl Dispatch<ZwpTextInputV3, ()> for App {
    fn event(
        data: &mut App,
        _: &ZwpTextInputV3,
        event: zwp_text_input_v3::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<App>,
    ) {
        match event {
            zwp_text_input_v3::Event::Enter { surface } => {
                data.ti_entered = true;
                data.ti_surface = Some(surface);
                if data.chat.is_some() {
                    data.ti_enable();
                }
            }
            zwp_text_input_v3::Event::Leave { .. } => {
                data.ti_entered = false;
                data.ti_disable();
            }
            zwp_text_input_v3::Event::CommitString { text } => {
                if let (Some(chat), Some(t)) = (&mut data.chat, text) {
                    if !t.is_empty() {
                        chat.input.push_str(&t);
                        data.needs_redraw = true;
                    }
                }
                data.ti_update_cursor();
            }
            zwp_text_input_v3::Event::PreeditString { text, .. } => {
                data.ti_preedit_pending = true;
                if let Some(chat) = &mut data.chat {
                    chat.preedit = text.unwrap_or_default();
                    data.needs_redraw = true;
                }
                data.ti_update_cursor();
            }
            // done 是一轮更新结束：本轮无 preedit_string 则清空缓存（防拼音残留）
            zwp_text_input_v3::Event::Done { .. } => {
                if !data.ti_preedit_pending {
                    if let Some(chat) = &mut data.chat {
                        if !chat.preedit.is_empty() {
                            chat.preedit.clear();
                            data.needs_redraw = true;
                        }
                    }
                }
                data.ti_preedit_pending = false;
            }
            _ => {}
        }
    }
}

delegate_noop!(App: ignore ZwpTextInputManagerV3);

delegate_compositor!(App);
delegate_output!(App);
delegate_seat!(App);
delegate_pointer!(App);
delegate_keyboard!(App);
delegate_layer!(App);
delegate_shm!(App);
delegate_registry!(App);
delegate_noop!(App: ignore WlRegion);

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    fn runtime_add_global(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _name: u32,
        _interface: &str,
        _version: u32,
    ) {
    }

    fn runtime_remove_global(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _name: u32,
        _interface: &str,
    ) {
    }
}

// ---------- 入口 ----------

struct Args {
    scale: Option<u32>,
    pos: Option<(i32, i32)>,
    debug_fill: Option<u32>,
}

fn parse_args() -> Args {
    let mut scale = None;
    let mut pos = None;
    let mut debug_fill = None;
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-s" | "--scale" => {
                if let Some(v) = it.next() {
                    scale = v.parse().ok().filter(|v| *v >= 1);
                }
            }
            "--pos" => {
                if let Some(v) = it.next() {
                    let mut parts = v.split(',');
                    if let (Some(x), Some(y)) = (parts.next(), parts.next()) {
                        pos = Some((x.parse().unwrap_or(48), y.parse().unwrap_or(48)));
                    }
                }
            }
            "--debug-fill" => {
                if let Some(v) = it.next() {
                    debug_fill = u32::from_str_radix(v.trim_start_matches("0x"), 16).ok();
                }
            }
            "-h" | "--help" => {
                println!(
                    "hyprpet — Hyprland 像素桌面宠物\n\
                     \n\
                     用法: hyprpet [选项]\n\
                     \n\
                     选项:\n\
                     \x20 -s, --scale <倍率>   默认像素放大倍率（角色 meta.toml 的 scale 优先）\n\
                     \x20     --pos <x,y>     初始位置（全局布局坐标，默认右下角）\n\
                     \n\
                     角色目录: ~/.config/hyprpet/characters/<名字>/\n\
                     \x20 meta.toml + sprites.png(+sprites.json) + actions.toml\n\
                     \n\
                     交互: 左键拖拽/抛掷  右键换肤菜单  kill -USR1 切穿透"
                );
                std::process::exit(0);
            }
            other => eprintln!("[hyprpet] 忽略未知参数: {other}"),
        }
    }
    Args { scale, pos, debug_fill }
}

fn main() -> Result<()> {
    // hyprpet mcp ... 子命令（CLI 模式，不启动桌面宠物）
    let raw: Vec<String> = std::env::args().skip(1).collect();
    if raw.first().map(|s| s.as_str()) == Some("mcp") {
        std::process::exit(cli::mcp_main(&raw[1..]));
    }
    let args = parse_args();

    // 首次运行写入内置角色与提示音
    let builtin_dir = assets::ensure_builtin_character().context("初始化内置角色失败")?;
    assets::ensure_builtin_slime().context("初始化内置史莱姆失败")?;
    audio::ensure_builtin_sounds();

    // 读取上次状态（角色 / 模式 / 位置）
    let state_path = assets::characters_dir().join("state.toml");
    let saved_state: std::collections::HashMap<String, String> = std::fs::read_to_string(&state_path)
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default();
    let current_dir = saved_state
        .get("current")
        .map(|name| assets::characters_dir().join(name))
        .filter(|p| p.exists());
    let saved_mode = saved_state.get("mode").map(|s| s.as_str()).unwrap_or("normal");
    let saved_pos = saved_state
        .get("x")
        .and_then(|x| x.parse::<i32>().ok())
        .zip(saved_state.get("y").and_then(|y| y.parse::<i32>().ok()));

    let mut pet = Pet::new(32, 32, args.scale.unwrap_or(4));
    let mut placed = false;
    if let Some((x, y)) = args.pos {
        pet.x = x;
        pet.y = y;
        placed = true;
    }

    let conn = Connection::connect_to_env().context("无法连接 Wayland display")?;
    let (globals, mut event_queue) = registry_queue_init::<App>(&conn)?;
    let qh = event_queue.handle();

    let compositor_state = CompositorState::bind(&globals, &qh)?;
    let seat_state = SeatState::new(&globals, &qh);
    let output_state = OutputState::new(&globals, &qh);
    let shm_state = Shm::bind(&globals, &qh)?;
    let registry_state = RegistryState::new(&globals);
    let layer_shell = LayerShell::bind(&globals, &qh)?;
    let ti_manager = globals
        .bind::<ZwpTextInputManagerV3, App, ()>(&qh, 1..=1, ())
        .ok();
    let idle_notifier = match globals
        .bind::<ExtIdleNotifierV1, App, ()>(&qh, 1..=1, ())
    {
        Ok(n) => Some(n),
        Err(_) => None,
    };

    let mut app = App {
        qh: qh.clone(),
        compositor_state,
        seat_state,
        output_state,
        shm_state,
        registry_state,
        layer_shell,
        watcher: None,
        hypr: HyprSockets::connect(),
        win_cache: WinCache::default(),
        win_refresh_at: None,
        audio: Audio::new(),
        bubble: None,
        chat: None,
        settings: None,
        agent_tx: None,
        timers: Vec::new(),
        ti_manager,
        text_input: None,
        ti_entered: false,
        ti_surface: None,
        ti_preedit_pending: false,
        last_press: None,
        last_sense: Instant::now() - std::time::Duration::from_secs(120),
        sense_acc: 0.0,
        cursor_acc: 0.0,
        tray_handle: None,
        behavior: BehaviorConfig::load(),
        seat: None,
        idle_notifier,
        idle_notification: None,
        user_active: true,
        normal_scale: args.scale.unwrap_or(4),
        mini_scale: 2,
        edge_peek: None,
        press_start: None,
        mods: Modifiers::default(),
        key_repeat: None,
        key_repeat_state: None,
        hover_reacted: false,
        keyboard: None,
        surfaces: Vec::new(),
        renderer: None,
        character: None,
        fsm: None,
        physics: Physics::default(),
        menu: None,
        font: None,
        pet,
        placed,
        pointer: None,
        pointer_pos: (0.0, 0.0),
        drag_offset: None,
        drag_samples: Vec::new(),
        pending_reload: false,
        last_tick: Instant::now(),
        last_reload: Instant::now() - std::time::Duration::from_secs(1),
        debug_fill: args.debug_fill,
        needs_redraw: false,
        region_dirty: false,
    };

    // 首轮握手：new_output（创建各显示器 surface）+ 首批 configure
    event_queue.roundtrip(&mut app)?;

    let mut event_loop: EventLoop<'_, App> = EventLoop::try_new()?;

    // Wayland 事件源
    WaylandSource::new(conn, event_queue)
        .insert(event_loop.handle())
        .map_err(|e| anyhow::anyhow!("注册 Wayland 事件源失败: {e:?}"))?;

    // 动画/物理心跳（60Hz）
    let timer = calloop::timer::Timer::from_duration(std::time::Duration::from_millis(16));
    event_loop
        .handle()
        .insert_source(timer, |_, _, app| {
            app.on_tick();
            TimeoutAction::ToDuration(std::time::Duration::from_millis(16))
        })
        .map_err(|e| anyhow::anyhow!("注册定时器失败: {e:?}"))?;

    // 信号 + 文件监视 + 通知共用控制 channel
    let (tx, rx) = channel::channel::<ControlEvent>();

    // Hyprland 事件 socket（Generic fd 源，可读时防抖刷新窗口缓存）
    let mut event_buf = String::new();
    if let Some(hypr) = &app.hypr {
        use calloop::generic::Generic;
        use calloop::{Interest, Mode};
        use std::io::Read as _;
        let generic = Generic::new(hypr.event_stream(), Interest::READ, Mode::Level);
        if let Err(e) = event_loop.handle().insert_source(generic, move |_, stream, app| {
            use calloop::PostAction;
            let stream = unsafe { stream.get_mut() };
            let mut chunk = [0u8; 4096];
            loop {
                match stream.read(&mut chunk) {
                    Ok(0) => return Ok(PostAction::Continue),
                    Ok(n) => {
                        event_buf.push_str(&String::from_utf8_lossy(&chunk[..n]));
                        while let Some(pos) = event_buf.find('\n') {
                            let line: String = event_buf[..pos].trim().to_string();
                            event_buf.drain(..=pos);
                            if hypr::event_triggers_refresh(&line) {
                                app.win_refresh_at =
                                    Some(Instant::now() + std::time::Duration::from_millis(150));
                            }
                        }
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        return Ok(PostAction::Continue)
                    }
                    Err(_) => return Ok(PostAction::Remove),
                }
            }
        }) {
            eprintln!("[hyprpet] 注册 Hyprland 事件源失败: {e:?}");
        }
        app.win_refresh_at = Some(Instant::now() + std::time::Duration::from_millis(50));
    }

    // DBus 系统通知监听（zbus blocking 线程）
    std::thread::spawn({
        let tx = tx.clone();
        move || {
            let conn = match zbus::blocking::Connection::session() {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("[hyprpet] DBus 会话连接失败，通知气泡禁用: {e}");
                    return;
                }
            };
            // BecomeMonitor：通知守护进程（quickshell/swaync 等）不广播 Notify 信号，
            // 只能以总线监视模式窃听 method call。此后此连接只读。
            let rules = vec!["type='method_call',member='Notify'".to_string()];
            if let Err(e) = conn.call_method(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus.Monitoring"),
                "BecomeMonitor",
                &(rules, 0u32),
            ) {
                eprintln!("[hyprpet] BecomeMonitor 失败（DBus 策略限制），通知气泡禁用: {e}");
                return;
            }
            eprintln!("[hyprpet] 通知监听已启用（DBus monitor 模式）");
            use zbus::blocking::MessageIterator;
            let iter = match MessageIterator::for_match_rule(
                zbus::MatchRule::builder().msg_type(zbus::message::Type::MethodCall).build(),
                &conn,
                None,
            ) {
                Ok(i) => i,
                Err(e) => {
                    eprintln!("[hyprpet] 通知订阅失败: {e}");
                    return;
                }
            };
            type Hints = std::collections::HashMap<String, zbus::zvariant::OwnedValue>;
            for msg in iter {
                let Ok(msg) = msg else { continue };
                let hdr = msg.header();
                if hdr.interface().map(|i| i.as_str()) != Some("org.freedesktop.Notifications")
                    || hdr.member().map(|m| m.as_str()) != Some("Notify")
                {
                    continue;
                }
                match msg
                    .body()
                    .deserialize::<(String, u32, String, String, String, Vec<String>, Hints, i32)>()
                {
                    Ok((_app, _id, _icon, summary, _body, _actions, _hints, _timeout)) => {
                        if tx.send(ControlEvent::Notify(summary)).is_err() {
                            break;
                        }
                    }
                    Err(e) => eprintln!("[hyprpet] 通知解析失败: {e}"),
                }
            }
        }
    });
    std::thread::spawn({
        let tx = tx.clone();
        move || {
            use signal_hook::consts::{SIGINT, SIGTERM, SIGUSR1, SIGUSR2};
            let mut signals =
                signal_hook::iterator::Signals::new([SIGUSR1, SIGUSR2, 10, SIGTERM, SIGINT]).expect("注册信号失败");
            for sig in signals.forever() {
                // SIGUSR3 无 signal-hook 常量，值为 10
                let ev = match sig {
                    SIGUSR1 => ControlEvent::TogglePassthrough,
                    SIGUSR2 => ControlEvent::OpenChat,
                    10 => ControlEvent::ToggleMini,
                    _ => ControlEvent::Quit,
                };
                if tx.send(ev).is_err() {
                    break;
                }
            }
        }
    });

    // 热重载监视
    match notify::RecommendedWatcher::new(
        move |res: notify::Result<notify::Event>| {
            if let Ok(ev) = res {
                if !matches!(ev.kind, notify::EventKind::Access(_)) {
                    let _ = tx.send(ControlEvent::Reload);
                }
            }
        },
        notify::Config::default(),
    ) {
        Ok(mut watcher) => {
            let watch_dir = assets::characters_dir()
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(assets::characters_dir);
            if let Err(e) = watcher.watch(&watch_dir, RecursiveMode::Recursive) {
                eprintln!("[hyprpet] 监视配置目录失败: {e}");
            } else {
                app.watcher = Some(watcher);
            }
        }
        Err(e) => eprintln!("[hyprpet] 创建文件监视器失败: {e}"),
    }

    event_loop
        .handle()
        .insert_source(rx, |ev, _, app| match ev {
            ChannelEvent::Msg(ControlEvent::TogglePassthrough) => app.toggle_passthrough(),
            ChannelEvent::Msg(ControlEvent::OpenChat) => {
                if !app.pet.is_mini() {
                    app.open_chat();
                }
            }
            ChannelEvent::Msg(ControlEvent::ToggleMini) => app.toggle_mini(),
            ChannelEvent::Msg(ControlEvent::Reload) => app.pending_reload = true,
            ChannelEvent::Msg(ControlEvent::Notify(summary)) => app.on_notification(summary),
            ChannelEvent::Msg(ControlEvent::Quit) | ChannelEvent::Closed => std::process::exit(0),
        })
        .map_err(|e| anyhow::anyhow!("注册信号事件源失败: {e:?}"))?;

    // 控制 socket（agent 联动：say/act/pos/skin/mini/hide/show）
    let ctl_path = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir())
        .join("hyprpet.sock");
    std::fs::remove_file(&ctl_path).ok();
    match std::os::unix::net::UnixListener::bind(&ctl_path) {
        Ok(listener) => {
            use calloop::generic::Generic;
            use calloop::{Interest, Mode};
            use std::io::{Read as _, Write as _};
            listener.set_nonblocking(true).ok();
            let generic = Generic::new(listener, Interest::READ, Mode::Level);
            if let Err(e) = event_loop.handle().insert_source(generic, move |_, listener, app| {
                use calloop::PostAction;
                loop {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            stream.set_read_timeout(Some(std::time::Duration::from_millis(300))).ok();
                            let mut buf = Vec::new();
                            let mut chunk = [0u8; 512];
                            let mut line = String::new();
                            loop {
                                match stream.read(&mut chunk) {
                                    Ok(0) => break,
                                    Ok(n) => {
                                        buf.extend_from_slice(&chunk[..n]);
                                        if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                                            line = String::from_utf8_lossy(&buf[..pos]).trim().to_string();
                                            break;
                                        }
                                    }
                                    Err(ref e)
                                        if e.kind() == std::io::ErrorKind::WouldBlock
                                            || e.kind() == std::io::ErrorKind::TimedOut =>
                                    {
                                        if buf.is_empty() {
                                            break;
                                        }
                                        line = String::from_utf8_lossy(&buf).trim().to_string();
                                        break;
                                    }
                                    Err(_) => break,
                                }
                            }
                            let resp = if line.is_empty() {
                                "err: 空命令".to_string()
                            } else {
                                app.handle_ctl(&line)
                            };
                            let _ = stream.write_all(format!("{resp}\n").as_bytes());
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            return Ok(PostAction::Continue)
                        }
                        Err(_) => return Ok(PostAction::Remove),
                    }
                }
            }) {
                eprintln!("[hyprpet] 控制 socket 注册失败: {e:?}");
            } else {
                eprintln!("[hyprpet] 控制 socket: {}", ctl_path.display());
            }
        }
        Err(e) => eprintln!("[hyprpet] 控制 socket 绑定失败: {e}"),
    }

    // 系统托盘（SNI）
    {
        let (tray_tx, tray_rx) = channel::channel::<TrayMsg>();
        let icon = app
            .character
            .as_ref()
            .map(|c| tray::tray_icon_from_sheet(&c.sheet, c.preview_frame(), 22))
            .unwrap_or(ksni::Icon { width: 22, height: 22, data: vec![0; 22 * 22 * 4] });
        app.tray_handle = tray::spawn_tray(icon, "HyprPet".into(), tray_tx);
        event_loop
            .handle()
            .insert_source(tray_rx, |ev, _, app| match ev {
                ChannelEvent::Msg(TrayMsg::Cmd(cmd)) => app.execute_cmd(cmd),
                ChannelEvent::Msg(TrayMsg::Activate) => {
                    app.toggle_mini();
                    app.sync_tray();
                }
                ChannelEvent::Closed => {}
            })
            .map_err(|e| anyhow::anyhow!("注册托盘事件源失败: {e:?}"))?;
    }

    // Agent worker 线程（LLM + 工具 + MCP）
    let (agent_evt_tx, agent_evt_rx) = channel::channel::<AgentEvent>();
    app.agent_tx = Some(agent::spawn_agent(agent_evt_tx));
    event_loop
        .handle()
        .insert_source(agent_evt_rx, |ev, _, app| match ev {
            ChannelEvent::Msg(AgentEvent::Reply(text)) => app.on_agent_reply(text),
            ChannelEvent::Msg(AgentEvent::Busy(b)) => {
                if let Some(c) = &mut app.chat {
                    c.busy = b;
                }
            }
            ChannelEvent::Msg(AgentEvent::Timer(minutes, label)) => app.on_agent_timer(minutes, label),
            ChannelEvent::Msg(AgentEvent::ToggleMini) => app.toggle_mini(),
            ChannelEvent::Msg(AgentEvent::Hide) => app.mini_hide(),
            ChannelEvent::Msg(AgentEvent::Show) => app.mini_show(),
            ChannelEvent::Closed => {}
        })
        .map_err(|e| anyhow::anyhow!("注册 agent 事件源失败: {e:?}"))?;

    // 加载角色并恢复模式/位置
    let dir = current_dir.unwrap_or(builtin_dir);
    app.apply_character(&dir);
    if saved_mode == "mini" {
        app.toggle_mini();
    }
    if let Some((x, y)) = saved_pos {
        app.pet.x = x;
        app.pet.y = y;
        if let Some(b) = app.layout_bbox() {
            app.pet.clamp_to(b.0, b.1, b.2, b.3);
        }
        app.region_dirty = true;
        app.needs_redraw = true;
    }

    // 进入主循环前先消费一轮初始状态
    on_idle(&mut app);

    event_loop.run(None, &mut app, on_idle)?;

    Ok(())
}

/// 每轮 dispatch 后的收尾：应用 input region 变化 + 按需重绘 + 热重载
fn on_idle(app: &mut App) {
    if app.pending_reload {
        app.reload_character();
    }
    if app.region_dirty {
        app.apply_input_region();
    }
    if app.needs_redraw {
        app.needs_redraw = false;
        if let Err(e) = app.redraw() {
            eprintln!("[hyprpet] 绘制失败: {e:#}");
        }
    }
}
