//! SNI 系统托盘（ksni blocking）。
//!
//! 托盘线程独立运行，与主线程双向通信：
//! - 托盘 → 主线程：菜单命令 / 左键激活（calloop channel）
//! - 主线程 → 托盘：Handle::update 刷新勾选状态与图标

use crate::ui::MenuCmd;

/// 托盘发往主线程的消息
#[derive(Debug, Clone, Copy)]
pub enum TrayMsg {
    Cmd(MenuCmd),
    /// 左键点击托盘图标
    Activate,
}

pub struct PetTray {
    pub icon: ksni::Icon,
    title: String,
    tx: calloop::channel::Sender<TrayMsg>,
    pub mini: bool,
    pub passthrough: bool,
    pub idle_secs: u64,
    pub hidden_edge: bool,
}

impl PetTray {
    pub fn new(icon: ksni::Icon, title: String, tx: calloop::channel::Sender<TrayMsg>) -> Self {
        PetTray {
            icon,
            title,
            tx,
            mini: false,
            passthrough: false,
            idle_secs: 120,
            hidden_edge: false,
        }
    }

    fn send(&self, msg: TrayMsg) {
        let _ = self.tx.send(msg);
    }
}

use ksni::blocking::TrayMethods;

impl ksni::Tray for PetTray {
    fn id(&self) -> String {
        "hyprpet".into()
    }

    fn title(&self) -> String {
        self.title.clone()
    }

    fn status(&self) -> ksni::Status {
        ksni::Status::Active
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        vec![self.icon.clone()]
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: self.title.clone(),
            description: String::new(),
            icon_name: String::new(),
            icon_pixmap: vec![],
        }
    }

    /// 左键：切 mini
    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(TrayMsg::Activate);
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::*;
        let tx = self.tx.clone();
        let item = move |cmd: MenuCmd| {
            let tx = tx.clone();
            Box::new(move |_: &mut Self| {
                let _ = tx.send(TrayMsg::Cmd(cmd));
            }) as Box<dyn Fn(&mut Self) + Send>
        };
        vec![
            MenuItem::Checkmark(CheckmarkItem {
                label: "mini 模式".into(),
                checked: self.mini,
                activate: item(MenuCmd::ToggleMini),
                ..Default::default()
            }),
            MenuItem::Checkmark(CheckmarkItem {
                label: "鼠标穿透".into(),
                checked: self.passthrough,
                activate: item(MenuCmd::TogglePassthrough),
                ..Default::default()
            }),
            MenuItem::Standard(StandardItem {
                label: format!("漫游阈值：{}s（点击切换）", self.idle_secs),
                activate: item(MenuCmd::CycleIdle),
                ..Default::default()
            }),
            MenuItem::Separator,
            MenuItem::Standard(StandardItem {
                label: "打开聊天".into(),
                activate: item(MenuCmd::OpenChat),
                ..Default::default()
            }),
            MenuItem::Standard(StandardItem {
                label: if self.hidden_edge { "弹出边缘" } else { "收进边缘" }.into(),
                activate: item(MenuCmd::HideEdge),
                ..Default::default()
            }),
            MenuItem::Separator,
            MenuItem::Standard(StandardItem {
                label: "退出 hyprpet".into(),
                activate: item(MenuCmd::Quit),
                ..Default::default()
            }),
        ]
    }
}

/// 启动托盘服务（独立线程，返回句柄）
pub fn spawn_tray(
    icon: ksni::Icon,
    title: String,
    tx: calloop::channel::Sender<TrayMsg>,
) -> Option<ksni::blocking::Handle<PetTray>> {
    let tray = PetTray::new(icon, title, tx);
    let handle = tray.spawn().ok()?;
    eprintln!("[hyprpet] 系统托盘已注册");
    Some(handle)
}

/// 把宠物帧渲染为托盘图标（22x22，ARGB32 网络字节序，去预乘近似）
pub fn tray_icon_from_sheet(sheet: &crate::sprite::Sheet, idx: usize, size: i32) -> ksni::Icon {
    const TRANSPARENT: u32 = 0x00FFFFFF;
    let f = sheet.frame(idx);
    let mut data = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            // nearest 缩放到 size
            let sx = (x * f.w as i32) / size;
            let sy = (y * f.h as i32) / size;
            let c = sheet
                .pixels_ref_at(f.x as i32 + sx, f.y as i32 + sy)
                .copied()
                .unwrap_or(TRANSPARENT);
            let a = (c >> 24) as u8;
            if a == 0 {
                data.extend_from_slice(&[0, 0, 0, 0]);
            } else {
                // 去预乘（近似）
                let up = |v: u32| ((v * 255) / a as u32) as u8;
                data.push(up((c >> 16) & 0xff));
                data.push(up((c >> 8) & 0xff));
                data.push(up(c & 0xff));
                data.push(a);
            }
        }
    }
    ksni::Icon { width: size, height: size, data }
}
