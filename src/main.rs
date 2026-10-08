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

mod assets;
mod builtin_cat;
mod fsm;
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
use smithay_client_toolkit::seat::pointer::{PointerEvent, PointerEventKind, PointerHandler};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{
    delegate_compositor, delegate_layer, delegate_output, delegate_pointer, delegate_registry,
    delegate_seat, delegate_shm,
};
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_pointer::WlPointer;
use wayland_client::protocol::wl_region::WlRegion;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{delegate_noop, Connection, QueueHandle};

use crate::assets::Character;
use crate::fsm::Fsm;
use crate::pet::Pet;
use crate::physics::Physics;
use crate::renderer::Renderer;
use crate::ui::{FontKit, Menu, MenuEntry};

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;

/// 来自信号线程 / 文件监视器的事件
enum ControlEvent {
    TogglePassthrough,
    Reload,
    Quit,
}

/// 一个显示器对应的 layer surface 及其状态
struct PetSurface {
    output: WlOutput,
    /// 保持 layer surface 存活（drop 即销毁）
    #[allow(dead_code)]
    layer: LayerSurface,
    wl_surface: WlSurface,
    /// 该显示器在全局布局中的原点
    origin: (i32, i32),
    /// surface 尺寸（= 显示器可用区域分辨率）
    size: (u32, u32),
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
        let origin = self
            .output_state
            .info(output)
            .map(|i| i.logical_position.unwrap_or(i.location))
            .unwrap_or((0, 0));

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
            configured: false,
        });
        eprintln!("[hyprpet] 新显示器 surface: origin=({},{})", origin.0, origin.1);
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
        let Some(fsm) = &mut self.fsm else { return false };
        if fsm.set(name) {
            return true;
        }
        // 角色没有该动作时回落 idle
        name != "idle" && fsm.set("idle")
    }

    // ---------- 菜单 ----------

    fn open_menu(&mut self) {
        if self.font.is_none() {
            self.font = FontKit::load();
        }
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

    fn toggle_passthrough(&mut self) {
        self.pet.passthrough = !self.pet.passthrough;
        self.region_dirty = true;
        self.needs_redraw = true;
        eprintln!("[hyprpet] 穿透模式: {}", if self.pet.passthrough { "开" } else { "关" });
    }

    // ---------- 渲染 ----------

    fn redraw(&mut self) -> Result<()> {
        let Some(renderer) = &mut self.renderer else {
            return Ok(());
        };
        let frame = self.fsm.as_ref().map(|f| f.frame()).unwrap_or(0);
        let sheet = self.character.as_ref().map(|c| &c.sheet);
        let menu = self.menu.as_ref();
        let pet = &self.pet;
        let debug_fill = self.debug_fill;

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
                    if let Some(sheet) = sheet {
                        let rect = sheet.frame(frame);
                        sheet.blit_frame(pixels, buf_w, vp, rect, (pet.x, pet.y), pet.scale, pet.passthrough);
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
        let ground = self.ground_y(bbox);
        let mut moved = false;
        let mut animated = false;

        if self.physics.active {
            // 抛掷/下落：重力积分
            self.pet.x = (self.pet.x as f32 + self.physics.vx * dt).round() as i32;
            self.pet.y = (self.pet.y as f32 + self.physics.vy * dt).round() as i32;
            self.physics.vy = (self.physics.vy + Physics::GRAVITY * dt).min(Physics::TERMINAL);
            moved = true;
            self.pet.clamp_to(bbox.0, bbox.1, bbox.2, bbox.3);
            if self.pet.y >= ground && self.physics.vy > 0.0 {
                let impact = self.physics.vy;
                self.pet.y = ground;
                self.physics.stop();
                if impact > 700.0 {
                    self.set_action("land");
                } else {
                    self.set_action("idle");
                }
                animated = true;
            }
        } else if self.drag_offset.is_none() {
            // FSM 驱动的位移（行走）+ 贴地
            let def = self.fsm.as_ref().and_then(|f| f.def().cloned());
            if let Some(def) = def {
                if let Some([mx, my]) = def.movement {
                    self.pet.x = (self.pet.x as f32 + mx * dt).round() as i32;
                    self.pet.y = (self.pet.y as f32 + my * dt).round() as i32;
                    moved = true;
                    // 边界自动转向
                    if def.auto_turn && (self.pet.x <= bbox.0 || self.pet.x + self.pet.width_px() >= bbox.2) {
                        let current = self.fsm.as_ref().map(|f| f.current.clone()).unwrap_or_default();
                        let mirrored = mirror_action(&current);
                        if self.set_action(&mirrored) {
                            animated = true;
                        }
                    }
                }
                // 非拖拽/非物理时保持贴地
                self.pet.y = ground;
            }
        }

        // FSM 动画推进
        if let Some(fsm) = &mut self.fsm {
            if fsm.tick(dt) {
                animated = true;
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
                        if speed > Physics::TOSS_THRESHOLD {
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

delegate_compositor!(App);
delegate_output!(App);
delegate_seat!(App);
delegate_pointer!(App);
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

    // 首次运行写入内置角色
    let builtin_dir = assets::ensure_builtin_character().context("初始化内置角色失败")?;

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

    let mut app = App {
        qh: qh.clone(),
        compositor_state,
        seat_state,
        output_state,
        shm_state,
        registry_state,
        layer_shell,
        watcher: None,
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

    // 信号 + 文件监视共用控制 channel
    let (tx, rx) = channel::channel::<ControlEvent>();
    std::thread::spawn({
        let tx = tx.clone();
        move || {
            use signal_hook::consts::{SIGINT, SIGTERM, SIGUSR1};
            let mut signals =
                signal_hook::iterator::Signals::new([SIGUSR1, SIGTERM, SIGINT]).expect("注册信号失败");
            for sig in signals.forever() {
                let ev = match sig {
                    SIGUSR1 => ControlEvent::TogglePassthrough,
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
            ChannelEvent::Msg(ControlEvent::Reload) => app.pending_reload = true,
            ChannelEvent::Msg(ControlEvent::Quit) | ChannelEvent::Closed => std::process::exit(0),
        })
        .map_err(|e| anyhow::anyhow!("注册信号事件源失败: {e:?}"))?;

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
