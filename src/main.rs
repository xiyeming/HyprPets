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
mod chat;
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
use smithay_client_toolkit::seat::keyboard::{KeyEvent, KeyboardHandler};
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
use wayland_protocols::wp::text_input::zv3::client::zwp_text_input_v3::{self, ZwpTextInputV3};

use crate::agent::{AgentEvent, AgentRequest};
use crate::assets::Character;
use crate::chat::{Chat, Who};
use crate::audio::Audio;
use crate::fsm::Fsm;
use crate::hypr::{HyprSockets, WinCache};
use crate::pet::Pet;
use crate::physics::Physics;
use crate::renderer::Renderer;
use crate::ui::{Bubble, FontKit, Menu, MenuEntry};

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;

/// 来自信号线程 / 文件监视器的事件
enum ControlEvent {
    TogglePassthrough,
    OpenChat,
    Reload,
    Notify(String),
    Quit,
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
    agent_tx: Option<std::sync::mpsc::Sender<AgentRequest>>,
    timers: Vec<(Instant, String)>,
    /// text-input-v3（IME 输入）
    ti_manager: Option<ZwpTextInputManagerV3>,
    text_input: Option<ZwpTextInputV3>,
    ti_entered: bool,
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
                    self.pet.scale = scale.max(1);
                }
                let mut fsm = Fsm::new(character.actions.clone(), &character.default_action);
                // 尽量延续切换前的动作名
                if let Some(prev) = self.fsm.as_ref().map(|f| f.current.clone()) {
                    fsm.set(&prev);
                }
                self.fsm = Some(fsm);
                self.character = Some(character);
                if let Some(b) = self.layout_bbox() {
                    self.pet.clamp_to(b.0, b.1, b.2, b.3);
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
        let state = format!("current = \"{name}\"\n");
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

    fn open_menu(&mut self) {
        self.ensure_font();
        let Some(font) = self.font.take() else {
            eprintln!("[hyprpet] 无可用系统字体，菜单不可用");
            return;
        };
        let entries: Vec<MenuEntry> = assets::scan_characters()
            .into_iter()
            .filter_map(|(name, dir)| {
                Character::load(&dir).ok().map(|c| {
                    let preview_idx = c.preview_frame();
                    MenuEntry { name, preview: c.sheet, preview_idx, dir }
                })
            })
            .collect();
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
        for s in &self.surfaces {
            if self.pet.passthrough {
                s.wl_surface.set_input_region(None);
            } else {
                let region = self.compositor_state.wl_compositor().create_region(&self.qh, ());
                if fullscreen {
                    region.add(0, 0, s.size.0 as i32, s.size.1 as i32);
                } else {
                    let x0 = self.pet.x.max(s.origin.0);
                    let y0 = self.pet.y.max(s.origin.1);
                    let x1 = (self.pet.x + self.pet.width_px()).min(s.origin.0 + s.size.0 as i32);
                    let y1 = (self.pet.y + self.pet.height_px()).min(s.origin.1 + s.size.1 as i32);
                    if x0 < x1 && y0 < y1 {
                        region.add(x0 - s.origin.0, y0 - s.origin.1, x1 - x0, y1 - y0);
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
        let font = self.font.as_ref();
        let evade = self.evading();
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
                        sheet.blit_frame(pixels, buf_w, vp, rect, (pet.x, pet.y), pet.scale, alpha);
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

        let Some(bbox) = self.layout_bbox() else { return };
        let mut moved = false;
        let mut animated = false;

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

                // FSM 驱动的位移（行走）
                if let Some([mx, my]) = def.movement {
                    let evading = self.evading();
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

        // 气泡过期
        if let Some(b) = &self.bubble {
            if b.until <= now {
                self.bubble = None;
                self.needs_redraw = true;
            }
        }

        // 定时器到期
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

        // 光标闪烁
        if self.chat.is_some() {
            self.cursor_acc += dt;
            if self.cursor_acc >= 0.5 {
                self.cursor_acc = 0.0;
                if let Some(c) = &mut self.chat {
                    c.toggle_cursor();
                }
                self.needs_redraw = true;
            }
        }

        // 系统状态感知（每 5s，冷却 90s）
        self.sense_acc += dt;
        if self.sense_acc >= 5.0 {
            self.sense_acc = 0.0;
            self.system_sense(now);
        }

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
                }
                PointerEventKind::Leave { .. } => {
                    self.pet.hovered = false;
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
                            self.open_chat();
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
                        if self.menu.is_some() {
                            // 模态菜单：命中切换，未命中关闭
                            let hit = self.menu.as_ref().and_then(|m| m.hit(global));
                            if let Some(idx) = hit {
                                let dir = self
                                    .menu
                                    .as_ref()
                                    .and_then(|m| m.entries.get(idx).map(|e| e.dir.clone()));
                                self.close_menu();
                                if let Some(dir) = dir {
                                    self.switch_character(&dir);
                                }
                            } else {
                                self.close_menu();
                            }
                            continue;
                        }
                        if !self.pet.passthrough {
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
                        let speed = (vx.hypot(vy)) as f32;
                        // 快速甩动 → 抛掷；或宠物悬在空中 → 小掉落自然落地
                        //（空中判定：距最近可站立地板 >4px）
                        let off_ground = !matches!(
                            self.floor_status(self.pet.x, self.pet.x + self.pet.width_px(), self.pet.y),
                            FloorStatus::OnFloor(_)
                        );
                        if speed > Physics::TOSS_THRESHOLD || off_ground {
                            self.physics.toss(vx as f32, vy as f32);
                            self.set_action("fall");
                        } else {
                            self.set_action("land");
                        }
                        self.region_dirty = true;
                        self.needs_redraw = true;
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

    fn new_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: WlSeat) {}

    fn remove_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: WlSeat) {
        self.pointer = None;
    }

    fn new_capability(&mut self, _conn: &Connection, qh: &QueueHandle<Self>, seat: WlSeat, capability: Capability) {
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
    }

    fn press_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &WlKeyboard,
        _serial: u32,
        event: KeyEvent,
    ) {
        self.on_key_press(&event);
    }

    fn release_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &WlKeyboard,
        _serial: u32,
        _event: KeyEvent,
    ) {
    }

    fn update_modifiers(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &WlKeyboard,
        _serial: u32,
        _modifiers: smithay_client_toolkit::seat::keyboard::Modifiers,
        _layout: u32,
    ) {
    }

    fn update_repeat_info(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &WlKeyboard,
        _info: smithay_client_toolkit::seat::keyboard::RepeatInfo,
    ) {
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

    fn ti_disable(&mut self) {
        if let Some(ti) = &self.text_input {
            ti.disable();
            ti.commit();
        }
        if let Some(c) = &mut self.chat {
            c.preedit.clear();
        }
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

    fn on_key_press(&mut self, event: &KeyEvent) {
        if self.chat.is_none() {
            return;
        }
        let code: u32 = event.keysym.into();
        match code {
            KS_RETURN => self.submit_chat(),
            KS_ESCAPE => self.close_chat(),
            KS_BACKSPACE => {
                self.chat.as_mut().unwrap().input.pop();
                self.needs_redraw = true;
            }
            _ if !self.ti_active() => {
                // 无 IME：物理按键文本直接输入；IME 激活时文本由 commit_string 提供
                if let Some(u) = &event.utf8 {
                    if !u.chars().any(|c| c.is_control()) {
                        self.chat.as_mut().unwrap().input.push_str(u);
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
        // 独占键盘焦点
        for s in &self.surfaces {
            s.layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
            s.wl_surface.commit();
        }
        // 焦点已就绪（ti_entered）则直接启用 IME；否则等 enter 事件
        if self.ti_entered {
            self.ti_enable();
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
        for s in &self.surfaces {
            s.layer.set_keyboard_interactivity(KeyboardInteractivity::None);
            s.wl_surface.commit();
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
            zwp_text_input_v3::Event::Enter { .. } => {
                data.ti_entered = true;
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
            }
            zwp_text_input_v3::Event::PreeditString { text, .. } => {
                if let Some(chat) = &mut data.chat {
                    chat.preedit = text.unwrap_or_default();
                    data.needs_redraw = true;
                }
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
    let args = parse_args();

    // 首次运行写入内置角色与提示音
    let builtin_dir = assets::ensure_builtin_character().context("初始化内置角色失败")?;
    audio::ensure_builtin_sounds();

    // 读取上次使用的角色
    let state_path = assets::characters_dir().join("state.toml");
    let current_dir = std::fs::read_to_string(&state_path)
        .ok()
        .and_then(|s| toml::from_str::<std::collections::HashMap<String, String>>(&s).ok())
        .and_then(|m| m.get("current").cloned())
        .map(|name| assets::characters_dir().join(name))
        .filter(|p| p.exists());

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
        agent_tx: None,
        timers: Vec::new(),
        ti_manager,
        text_input: None,
        ti_entered: false,
        last_press: None,
        last_sense: Instant::now() - std::time::Duration::from_secs(120),
        sense_acc: 0.0,
        cursor_acc: 0.0,
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
                signal_hook::iterator::Signals::new([SIGUSR1, SIGUSR2, SIGTERM, SIGINT]).expect("注册信号失败");
            for sig in signals.forever() {
                let ev = match sig {
                    SIGUSR1 => ControlEvent::TogglePassthrough,
                    SIGUSR2 => ControlEvent::OpenChat,
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
            if let Err(e) = watcher.watch(&assets::characters_dir(), RecursiveMode::Recursive) {
                eprintln!("[hyprpet] 监视角色目录失败: {e}");
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
            ChannelEvent::Msg(ControlEvent::OpenChat) => app.open_chat(),
            ChannelEvent::Msg(ControlEvent::Reload) => app.pending_reload = true,
            ChannelEvent::Msg(ControlEvent::Notify(summary)) => app.on_notification(summary),
            ChannelEvent::Msg(ControlEvent::Quit) | ChannelEvent::Closed => std::process::exit(0),
        })
        .map_err(|e| anyhow::anyhow!("注册信号事件源失败: {e:?}"))?;

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
            ChannelEvent::Closed => {}
        })
        .map_err(|e| anyhow::anyhow!("注册 agent 事件源失败: {e:?}"))?;

    // 加载角色
    let dir = current_dir.unwrap_or(builtin_dir);
    app.apply_character(&dir);

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
