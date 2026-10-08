//! hyprpet — Hyprland 像素桌面宠物（Phase 1）
//!
//! 架构要点：
//! - 每显示器一个 Layer::Overlay 全屏 surface，宠物 = 内部坐标，移动零协议开销
//! - input region 动态跟随宠物 bbox；穿透模式 = 置空 region（同一套机制）
//! - 拖拽时临时把 region 扩到全屏，避免快速拖动时指针脱离 region 导致断拖
//! - 渲染走 softbuffer（wl_shm）+ nearest 采样，主线程事件循环（Wayland 要求）

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
    /// 保持 layer surface 存活（drop 即销毁）
    #[allow(dead_code)]
    layer: Option<LayerSurface>,
    wl_surface: Option<WlSurface>,
    renderer: Option<Renderer>,

    /// surface 尺寸（= 输出分辨率）
    size: (u32, u32),
    configured: bool,

    sprite: Sprite,
    pet: Pet,
    /// 调试用：纯色填充整帧
    debug_fill: Option<u32>,

    pointer: Option<WlPointer>,
    pointer_pos: (f64, f64),
    /// 拖拽中保存的 指针位置-宠物左上角 偏移
    drag_offset: Option<(f64, f64)>,

    needs_redraw: bool,
    region_dirty: bool,
}

impl App {
    /// 按当前状态重设 input region 并提交。
    ///
    /// - 穿透：空 region（完全不收事件）
    /// - 拖拽：全屏 region（防止快速拖动指针脱离导致断拖）
    /// - 常态：宠物 bbox
    fn apply_input_region(&mut self) {
        let Some(surface) = self.wl_surface.as_ref() else { return };
        if self.pet.passthrough {
            surface.set_input_region(None);
        } else {
            let region = self.compositor_state.wl_compositor().create_region(&self.qh, ());
            if self.drag_offset.is_some() {
                region.add(0, 0, self.size.0 as i32, self.size.1 as i32);
            } else {
                region.add(
                    self.pet.x,
                    self.pet.y,
                    self.pet.width_px(),
                    self.pet.height_px(),
                );
            }
            surface.set_input_region(Some(&region));
            region.destroy();
        }
        surface.commit();
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
        let (Some(renderer), Some(surface)) = (&mut self.renderer, &self.wl_surface) else {
            return Ok(());
        };
        let (width, height) = self.size;
        let sprite = &self.sprite;
        let pet = &self.pet;
        renderer.present(surface, width, height, |pixels, buf_w| {
            if let Some(c) = self.debug_fill {
                pixels.fill(c);
            } else {
                sprite.blit(pixels, buf_w, pet);
            }
        })
    }

    fn on_pointer_frame(&mut self, events: &[PointerEvent]) {
        for event in events {
            let Some(surface) = self.wl_surface.as_ref() else { return };
            if event.surface != *surface {
                continue;
            }
            match event.kind {
                PointerEventKind::Enter { .. } => {
                    self.pet.hovered = true;
                }
                PointerEventKind::Leave { .. } => {
                    self.pet.hovered = false;
                    if self.drag_offset.is_some() {
                        // 指针脱离但 region 已扩到全屏，正常不会发生；防御性处理
                        self.drag_offset = None;
                        self.pet.dragged = false;
                        self.region_dirty = true;
                    }
                }
                PointerEventKind::Motion { .. } => {
                    self.pointer_pos = event.position;
                    if let Some((dx, dy)) = self.drag_offset {
                        self.pet.x = (event.position.0 - dx).round() as i32;
                        self.pet.y = (event.position.1 - dy).round() as i32;
                        self.pet.clamp_to(self.size.0, self.size.1);
                        self.needs_redraw = true;
                        self.region_dirty = true;
                    }
                }
                PointerEventKind::Press { button, .. } => {
                    if button == BTN_LEFT && !self.pet.passthrough {
                        self.drag_offset =
                            Some((event.position.0 - self.pet.x as f64, event.position.1 - self.pet.y as f64));
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
        eprintln!("[hyprpet] 缩放系数变化: {new_factor}");
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

    fn new_output(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _output: WlOutput) {}

    fn update_output(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _output: WlOutput) {}

    fn output_destroyed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _output: WlOutput) {}
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
        _layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let (w, h) = configure.new_size;
        let first = !self.configured;
        self.size = (w, h);
        if first && w > 0 && h > 0 {
            self.configured = true;
            let shm = &self.shm_state;
            match Renderer::new(shm, w, h) {
                Ok(r) => {
                    self.renderer = Some(r);
                }
                Err(e) => eprintln!("[hyprpet] 创建渲染器失败: {e:#}"),
            }
            // 初始位置：右下角
            self.pet.x = w as i32 - self.pet.width_px() - 48;
            self.pet.y = h as i32 - self.pet.height_px() - 48;
            self.pet.clamp_to(w, h);
            self.region_dirty = true;
        }
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
                     \x20     --pos <x,y>     初始位置（默认右下角）\n\
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
    let debug_fill = args.debug_fill;
    if let Some((x, y)) = args.pos {
        pet.x = x;
        pet.y = y;
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

    // 全屏 overlay surface：宠物是内部坐标，移动它零协议开销
    let wl_surface = compositor_state.create_surface(&qh);
    let layer = layer_shell.create_layer_surface(
        &qh,
        wl_surface.clone(),
        Layer::Overlay,
        Some("hyprpet"),
        None,
    );
    layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
    layer.set_size(0, 0);
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
    layer.set_exclusive_zone(0);
    wl_surface.commit();

    let mut app = App {
        qh: qh.clone(),
        compositor_state,
        seat_state,
        output_state,
        shm_state,
        registry_state,
        layer_shell,
        layer: Some(layer),
        wl_surface: Some(wl_surface),
        renderer: None,
        size: (0, 0),
        configured: false,
        sprite,
        pet,
        debug_fill,
        pointer: None,
        pointer_pos: (0.0, 0.0),
        drag_offset: None,
        needs_redraw: false,
        region_dirty: false,
    };

    // 首轮握手：拿到 configure（SCTK 内部自动 ack）
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

    eprintln!(
        "[hyprpet] 启动: 贴图 {}x{} scale={} pos=({},{}) pid={}",
        app.pet.w,
        app.pet.h,
        app.pet.scale,
        app.pet.x,
        app.pet.y,
        std::process::id()
    );

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
