//! hyprpet 作为 MCP server：`hyprpet mcp serve`（stdio）。
//!
//! 其他 agent（Claude Code / pi / Codex / opencode…）通过 MCP 协议
//! 获得宠物控制工具（say/act/move/skin/mini/hide/show/menu/skins），
//! 每个工具经控制 socket 转发给运行中的宠物守护进程。
//!
//! 用 `hyprpet mcp setup <agent>` 一键注册到各 agent 配置。

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{ServerHandler, ServiceExt, schemars, tool, tool_handler, tool_router};
use rmcp::handler::server::tool::ToolRouter;

/// 向运行中的宠物守护进程发送控制命令
fn ctl(cmd: &str) -> String {
    let path = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("hyprpet.sock");
    let mut stream = match UnixStream::connect(path) {
        Ok(s) => s,
        Err(e) => return format!("err: 无法连接宠物进程（请先启动 hyprpet）: {e}"),
    };
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(2)));
    if let Err(e) = stream.write_all(format!("{cmd}\n").as_bytes()) {
        return format!("err: 发送失败: {e}");
    }
    let mut chunk = [0u8; 256];
    match stream.read(&mut chunk) {
        Ok(n) if n > 0 => String::from_utf8_lossy(&chunk[..n]).trim().to_string(),
        Ok(_) => "err: 无响应".into(),
        Err(e) => format!("err: 读取失败: {e}"),
    }
}

type ToolResult = Result<rmcp::model::CallToolResult, rmcp::ErrorData>;

fn ok(text: String) -> ToolResult {
    Ok(rmcp::model::CallToolResult::success(vec![rmcp::model::Content::text(text)]))
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SayRequest {
    #[schemars(description = "气泡内容")]
    text: String,
}
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ActRequest {
    #[schemars(description = "动作名：idle/walk_left/walk_right/sleep/grabbed/fall/land/petted/alert/tired/celebrate/study")]
    action: String,
}
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct MoveRequest {
    x: i32,
    y: i32,
}
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SkinRequest {
    #[schemars(description = "角色目录名，可用 skins 工具列出")]
    name: String,
}

#[derive(Debug, Clone)]
pub struct PetMcp {
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl PetMcp {
    fn new() -> Self {
        Self { tool_router: Self::tool_router() }
    }

    #[tool(description = "让桌面宠物弹出说话气泡（通知提醒/播报任务结果）")]
    async fn say(&self, Parameters(SayRequest { text }): Parameters<SayRequest>) -> ToolResult {
        ok(ctl(&format!("say {text}")))
    }

    #[tool(description = "让宠物做指定动作")]
    async fn act(&self, Parameters(ActRequest { action }): Parameters<ActRequest>) -> ToolResult {
        ok(ctl(&format!("act {action}")))
    }

    #[tool(description = "移动宠物到屏幕全局坐标 (x, y)")]
    async fn move_to(&self, Parameters(MoveRequest { x, y }): Parameters<MoveRequest>) -> ToolResult {
        ok(ctl(&format!("pos {x} {y}")))
    }

    #[tool(description = "切换宠物皮肤")]
    async fn skin(&self, Parameters(SkinRequest { name }): Parameters<SkinRequest>) -> ToolResult {
        ok(ctl(&format!("skin {name}")))
    }

    #[tool(description = "切换 mini 模式（缩小/恢复）")]
    async fn mini(&self) -> ToolResult {
        ok(ctl("mini"))
    }

    #[tool(description = "收进屏幕边缘（留把手，周期性探头偷看）")]
    async fn hide(&self) -> ToolResult {
        ok(ctl("hide"))
    }

    #[tool(description = "从屏幕边缘弹出")]
    async fn show(&self) -> ToolResult {
        ok(ctl("show"))
    }

    #[tool(description = "唤起宠物右键菜单")]
    async fn menu(&self) -> ToolResult {
        ok(ctl("menu"))
    }

    #[tool(description = "列出可用皮肤")]
    async fn skins(&self) -> ToolResult {
        ok(ctl("skins"))
    }

}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for PetMcp {
    fn get_info(&self) -> rmcp::model::ServerInfo {
        rmcp::model::ServerInfo {
            instructions: Some(
                "hyprpet 桌面宠物控制：say 播报、act 动作、move 移动、skin 换肤、mini 模式、hide/show 收边。任务有进展时用 say 通知用户。"
                    .into(),
            ),
            capabilities: rmcp::model::ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}

/// 运行 MCP stdio server（阻塞直到 stdin 关闭）
pub fn serve() -> i32 {
    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("创建运行时失败: {e}");
            return 1;
        }
    };
    rt.block_on(async {
        match PetMcp::new().serve(rmcp::transport::stdio()).await {
            Ok(service) => {
                let _ = service.waiting().await;
                0
            }
            Err(e) => {
                eprintln!("MCP server 启动失败: {e}");
                1
            }
        }
    })
}
