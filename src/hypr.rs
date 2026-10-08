//! Hyprland IPC：事件监听（socket2）+ 命令查询（socket）。
//!
//! 不依赖 hyprland-rs / tokio —— 事件 socket 是纯文本行流，
//! 注册为 calloop Generic fd 源；查询用阻塞式 request/response。
//!
//! ```text
//! $XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/
//! ├── .socket2.sock   # 事件（只读行流：event>>data）
//! └── .socket.sock    # 命令（写 "j/clients" 等，读到 EOF 为响应）
//! ```

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

/// 一个可见窗口的几何信息（已按显示器/工作区过滤前的原始条目）
#[derive(Debug, Clone)]
pub struct WinInfo {
    pub monitor: String,
    pub workspace: i64,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub fullscreen: u8,
}

#[derive(Debug, Clone)]
pub struct MonInfo {
    pub id: i64,
    pub name: String,
    pub active_workspace: i64,
}

/// Hyprland 窗口/显示器缓存
#[derive(Debug, Default)]
pub struct WinCache {
    pub windows: Vec<WinInfo>,
    pub monitors: Vec<MonInfo>,
}

impl WinCache {
    /// 某显示器当前可见且非全屏的窗口
    pub fn visible_windows_on(&self, monitor: &str) -> impl Iterator<Item = &WinInfo> {
        let ws = self
            .monitors
            .iter()
            .find(|m| m.name == monitor)
            .map(|m| m.active_workspace);
        self.windows.iter().filter(move |w| {
            w.monitor == monitor && ws.map(|ws| w.workspace == ws).unwrap_or(false)
        })
    }

    /// 某显示器是否有全屏窗口
    pub fn has_fullscreen_on(&self, monitor: &str) -> bool {
        self.visible_windows_on(monitor).any(|w| w.fullscreen > 0)
    }
}

/// Hyprland socket 路径（实例签名来自环境）
pub struct HyprSockets {
    event: UnixStream,
    cmd: PathBuf,
}

/// 事件行 → 是否需要刷新窗口缓存
pub fn event_triggers_refresh(line: &str) -> bool {
    matches!(
        line.split(">>").next().unwrap_or(""),
        "workspace"
            | "fullscreen"
            | "openwindow"
            | "closewindow"
            | "movewindow"
            | "changefloatingmode"
            | "monitoradded"
            | "monitorremoved"
            | "activespecial"
    )
}

impl HyprSockets {
    /// 连接事件 socket（失败表示不在 Hyprland 下，返回 None）
    pub fn connect() -> Option<HyprSockets> {
        let sig = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE")?;
        let base: PathBuf = std::env::var_os("XDG_RUNTIME_HOME")
            .or_else(|| std::env::var_os("XDG_RUNTIME_DIR"))
            .map(PathBuf::from)?
            .join("hypr")
            .join(sig);
        let event = UnixStream::connect(base.join(".socket2.sock")).ok()?;
        event.set_nonblocking(true).ok()?;
        Some(HyprSockets { event, cmd: base.join(".socket.sock") })
    }

    /// 事件 socket（用于注册 calloop Generic 源）
    pub fn event_stream(&self) -> UnixStream {
        self.event.try_clone().expect("clone event socket")
    }

    /// 发送命令并读取 JSON 响应
    pub fn command_json(&self, command: &str) -> Result<serde_json::Value> {
        let mut stream = UnixStream::connect(&self.cmd).context("连接 hypr 命令 socket 失败")?;
        stream.write_all(command.as_bytes())?;
        stream.shutdown(std::net::Shutdown::Write)?;
        let mut resp = String::new();
        stream.read_to_string(&mut resp)?;
        serde_json::from_str(&resp).context("解析 hyprctl JSON 响应失败")
    }

    /// 刷新窗口/显示器缓存
    pub fn refresh_cache(&self) -> WinCache {
        let mut cache = WinCache::default();
        if let Ok(v) = self.command_json("j/monitors") {
            if let Ok(mons) = serde_json::from_value::<Vec<MonJson>>(v) {
                cache.monitors = mons
                    .into_iter()
                    .map(|m| MonInfo { id: m.id, name: m.name, active_workspace: m.active_workspace.id })
                    .collect();
            }
        }
        if let Ok(v) = self.command_json("j/clients") {
            if let Ok(clients) = serde_json::from_value::<Vec<ClientJson>>(v) {
                cache.windows = clients
                    .into_iter()
                    .filter(|c| c.mapped && c.workspace.id > 0)
                    .filter_map(|c| {
                        let name = cache.monitors.iter().find(|m| m.id == c.monitor)?.name.clone();
                        Some(WinInfo {
                            monitor: name,
                            workspace: c.workspace.id,
                            x: c.at[0],
                            y: c.at[1],
                            w: c.size[0],
                            h: c.size[1],
                            fullscreen: c.fullscreen as u8,
                        })
                    })
                    .collect();
            }
        }
        cache
    }
}

#[derive(Debug, Deserialize)]
struct MonJson {
    id: i64,
    name: String,
    active_workspace: WsJson,
}

#[derive(Debug, Deserialize)]
struct WsJson {
    id: i64,
}

#[derive(Debug, Deserialize)]
struct ClientJson {
    #[serde(default)]
    mapped: bool,
    workspace: WsJson,
    at: [i32; 2],
    size: [i32; 2],
    #[serde(default)]
    fullscreen: i32,
    #[serde(default)]
    monitor: i64,
}
