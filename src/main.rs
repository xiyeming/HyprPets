//! hyprpet — Hyprland 像素桌面宠物
//!
//! 架构要点：
//! - 每个显示器一个 Layer::Overlay 全屏 surface，宠物使用全局布局坐标，
//!   跨屏拖动 = 全局坐标越过显示器边界，自动在目标 surface 上绘制（双向裁剪）
//! - input region 每 surface 独立：常态 = 宠物 bbox 与该 surface 的交集；
//!   拖拽 = 所有 surface 全屏（保证跨屏过程中不断流）；穿透 = 空 region
//! - wl_shm SlotPool 软件渲染 + nearest 采样，主线程事件循环（Wayland 要求）

mod pet;
mod renderer;
mod sprite;

use std::path::PathBuf;

use anyhow::{Context, Result};
use calloop::channel::{self, Event as ChannelEvent};
use calloop::EventLoop;
use calloop_wayland_source::WaylandSource;
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

use crate::pet::Pet;
use crate::renderer::Renderer;
use crate::sprite::{default_sprite_path, Sprite};

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;

/// 来自信号线程的控制事件
enum ControlEvent {
    TogglePassthrough,
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

    /// 每显示器一个全屏 surface
    surfaces: Vec<PetSurface>,
    renderer: Option<Renderer>,

    sprite: Sprite,
    pet: Pet,
    /// 初始位置是否已由用户显式指定（--pos）或已放置
    placed: bool,

    pointer: Option<WlPointer>,
    /// 指针在全局布局坐标中的位置
    pointer_pos: (f64, f64),
    /// 拖拽中保存的 指针位置-宠物左上角 偏移（全局坐标）
    drag_offset: Option<(f64, f64)>,

    /// 调试用：纯色填充整帧
    debug_fill: Option<u32>,

    needs_redraw: bool,
    region_dirty: bool,
}

impl App {
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

    /// 创建某显示器的全屏 layer surface
    fn create_surface_for(&mut self, output: &WlOutput) {
        // 已存在则不重建
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

    /// 按当前状态重设所有 surface 的 input region 并提交。
    ///
    /// - 穿透：空 region（完全不收事件）
    /// - 拖拽：所有 surface 全屏 region（跨屏拖动不断流）
    /// - 常态：宠物 bbox 与各 surface 的交集（局部坐标）
    fn apply_input_region(&mut self) {
        let dragging = self.drag_offset.is_some();
        for s in &self.surfaces {
            if self.pet.passthrough {
                s.wl_surface.set_input_region(None);
            } else {
                let region = self.compositor_state.wl_compositor().create_region(&self.qh, ());
                if dragging {
                    region.add(0, 0, s.size.0 as i32, s.size.1 as i32);
                } else {
                    // 全局 bbox ∩ surface 区域 → 转局部坐标
                    let x0 = self.pet.x.max(s.origin.0);
                    let y0 = self.pet.y.max(s.origin.1);
                    let x1 = (self.pet.x + self.pet.width_px()).min(s.origin.0 + s.size.0 as i32);
                    let y1 = (self.pet.y + self.pet.height_px()).min(s.origin.1 + s.size.1 as i32);
                    if x0 < x1 && y0 < y1 {
                        region.add(
                            x0 - s.origin.0,
                            y0 - s.origin.1,
                            x1 - x0,
                            y1 - y0,
                        );
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
        eprintln!(
            "[hyprpet] 穿透模式: {}",
            if self.pet.passthrough { "开" } else { "关" }
        );
    }

    fn redraw(&mut self) -> Result<()> {
        let Some(renderer) = &mut self.renderer else {
            return Ok(());
        };
        let sprite = &self.sprite;
        let pet = &self.pet;
        let debug_fill = self.debug_fill;
        // 逐显示器绘制：宠物在哪个 surface 上就出现在哪里，跨屏时两边各画一份（裁剪后）
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
                    } else {
                        sprite.blit(pixels, buf_w, vp, pet);
                    }
                })
                .context("present 失败")?;
        }
        Ok(())
    }

    /// 首次放置宠物（无 --pos 时放联合包围盒右下角）并钳制
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

    fn surface_origin_of(&self, surface: &WlSurface) -> Option<(i32, i32)> {
        self.surfaces
            .iter()
            .find(|s| s.wl_surface == *surface)
            .map(|s| s.origin)
    }

    fn on_pointer_frame(&mut self, events: &[PointerEvent]) {
        for event in events {
            let Some(origin) = self.surface_origin_of(&event.surface) else {
                continue;
            };
            // 转全局布局坐标
            let global = (event.position.0 + origin.0 as f64, event.position.1 + origin.1 as f64);
            match event.kind {
                PointerEventKind::Enter { .. } => {
                    self.pet.hovered = true;
                }
                PointerEventKind::Leave { .. } => {
                    self.pet.hovered = false;
                    // 拖拽中不取消：跨屏时旧 surface 必然 Leave，
                    // 目标 surface 是全屏 region，会继续推送 Motion/Release
                }
                PointerEventKind::Motion { .. } => {
                    self.pointer_pos = global;
                    if let Some((dx, dy)) = self.drag_offset {
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
                    if button == BTN_LEFT && !self.pet.passthrough {
                        self.drag_offset = Some((global.0 - self.pet.x as f64, global.1 - self.pet.y as f64));
                        self.pet.dragged = true;
                        self.region_dirty = true;
                        self.needs_redraw = true;
                    } else if button == BTN_RIGHT {
                        eprintln!("[hyprpet] 右键菜单将在 Phase 2 提供");
                    }
                }
                PointerEventKind::Release { button, .. } => {
                    if button == BTN_LEFT && self.drag_offset.is_some() {
                        self.drag_offset = None;
                        self.pet.dragged = false;
                        self.region_dirty = true;
                        self.needs_redraw = true;
                    }
                }
                PointerEventKind::Axis { .. } => {}
            }
        }
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
        // xdg-output 信息（逻辑原点/尺寸）到达后同步到 surface
        if let Some(info) = self.output_state.info(&output) {
            let origin = info.logical_position.unwrap_or(info.location);
            let logical_size = info.logical_size.map(|(w, h)| (w as u32, h as u32));
            for s in &mut self.surfaces {
                if s.output == output {
                    s.origin = origin;
                    if let Some(ls) = logical_size {
                        // logical_size 是全屏尺寸；surface 实际尺寸以 configure 为准
                        let _ = ls;
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
    image: PathBuf,
    scale: u32,
    pos: Option<(i32, i32)>,
    debug_fill: Option<u32>,
}

fn parse_args() -> Args {
    let mut image = default_sprite_path();
    let mut scale = 4u32;
    let mut pos = None;
    let mut debug_fill = None;
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-i" | "--image" => {
                if let Some(v) = it.next() {
                    image = PathBuf::from(v);
                }
            }
            "-s" | "--scale" => {
                if let Some(v) = it.next() {
                    scale = v.parse().unwrap_or(4).max(1);
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
                     \x20 -i, --image <路径>   宠物贴图 PNG（默认 ~/.config/hyprpet/pet.png，缺失时自动生成）\n\
                     \x20 -s, --scale <倍率>   像素放大倍率（默认 4）\n\
                     \x20     --pos <x,y>     初始位置（全局布局坐标，默认右下角）\n\
                     \n\
                     信号:\n\
                     \x20 kill -USR1 <pid>     切换鼠标穿透模式"
                );
                std::process::exit(0);
            }
            other => eprintln!("[hyprpet] 忽略未知参数: {other}"),
        }
    }
    Args { image, scale, pos, debug_fill }
}

fn main() -> Result<()> {
    let args = parse_args();
    let sprite = Sprite::load_or_default(&args.image)?;
    let mut pet = Pet::new(sprite.width, sprite.height, args.scale);
    let mut placed = false;
    if let Some((x, y)) = args.pos {
        pet.x = x;
        pet.y = y;
        placed = true; // 不自动 reposition，但仍会在首个 bbox 可用时钳制
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
        surfaces: Vec::new(),
        renderer: None,
        sprite,
        pet,
        placed,
        pointer: None,
        pointer_pos: (0.0, 0.0),
        drag_offset: None,
        debug_fill: args.debug_fill,
        needs_redraw: false,
        region_dirty: false,
    };

    // 首轮握手：new_output（创建各显示器 surface）+ 首批 configure
    event_queue.roundtrip(&mut app)?;

    let mut event_loop: EventLoop<'_, App> = EventLoop::try_new()?;
    WaylandSource::new(conn, event_queue)
        .insert(event_loop.handle())
        .map_err(|e| anyhow::anyhow!("注册 Wayland 事件源失败: {e:?}"))?;

    // 信号控制线程：SIGUSR1 切穿透，SIGTERM/SIGINT 退出
    let (tx, rx) = channel::channel::<ControlEvent>();
    std::thread::spawn(move || {
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
    });
    event_loop
        .handle()
        .insert_source(rx, |ev, _, app| match ev {
            ChannelEvent::Msg(ControlEvent::TogglePassthrough) => app.toggle_passthrough(),
            ChannelEvent::Msg(ControlEvent::Quit) | ChannelEvent::Closed => std::process::exit(0),
        })
        .map_err(|e| anyhow::anyhow!("注册信号事件源失败: {e:?}"))?;

    // 进入主循环前先消费一轮初始状态（configure 已在 roundtrip 中收到），
    // 否则 dispatch(None) 会阻塞到有事件才执行 callback，首帧被延迟。
    on_idle(&mut app);

    event_loop.run(None, &mut app, on_idle)?;

    Ok(())
}

/// 每轮 dispatch 后的收尾：应用 input region 变化 + 按需重绘
fn on_idle(app: &mut App) {
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
