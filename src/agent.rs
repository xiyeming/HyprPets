//! AI Agent：LLM 工具调用循环 + 内置工具 + MCP 客户端。
//!
//! 线程模型：主线程经 calloop channel 发 `AgentRequest::Chat`，
//! worker 线程（本模块 spawn）阻塞式执行（reqwest blocking + rmcp/tokio），
//! 结果经 `AgentEvent` 送回主线程。无 LLM 配置时支持 `/` 本地命令。
//!
//! 安全模型：shell 仅白名单前缀可执行；MCP 工具由用户显式配置启用。

use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;

use anyhow::{Context, Result};
use serde::Deserialize;

// ---------- 配置 ----------

#[derive(Debug, Clone, Deserialize, Default)]
pub struct AgentConfig {
    pub llm: Option<LlmConfig>,
    #[serde(default)]
    pub security: Security,
    #[serde(default)]
    pub mcp: Vec<McpServer>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LlmConfig {
    /// openai（OpenAI 兼容 /chat/completions）| anthropic（/v1/messages）
    #[serde(default = "default_backend")]
    pub backend: String,
    #[serde(default = "default_base_url")]
    pub base_url: String,
    /// "env:NAME" | "cmd:命令" | 字面量
    pub api_key: Option<String>,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "default_system_prompt")]
    pub system_prompt: String,
}

fn default_backend() -> String {
    "openai".into()
}
fn default_base_url() -> String {
    "https://api.openai.com/v1".into()
}
fn default_model() -> String {
    "gpt-4o-mini".into()
}
fn default_system_prompt() -> String {
    "你是一个住在用户桌面上的像素小猫宠物。用简短、可爱、口语化的中文回答（不超过 3 句话），\
     可以适当用喵语和 emoji。需要时使用工具完成任务。"
        .into()
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Security {
    /// 允许执行的命令前缀（run_command 工具）
    #[serde(default = "default_shell_allow")]
    pub shell_allow: Vec<String>,
}

fn default_shell_allow() -> Vec<String> {
    ["ls", "date", "uptime", "df", "free", "cat /proc", "echo", "whoami", "pwd"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

#[derive(Debug, Clone, Deserialize)]
pub struct McpServer {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

impl AgentConfig {
    pub fn load() -> AgentConfig {
        let path = config_dir().join("agent.toml");
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn api_key(&self) -> Option<String> {
        let key = self.llm.as_ref()?.api_key.as_ref()?;
        if let Some(name) = key.strip_prefix("env:") {
            std::env::var(name.trim()).ok().filter(|v| !v.is_empty())
        } else if let Some(cmd) = key.strip_prefix("cmd:") {
            Command::new("sh")
                .arg("-c")
                .arg(cmd)
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .filter(|v| !v.is_empty())
        } else {
            Some(key.clone())
        }
    }
}

/// 保存配置（手写 TOML，保留 mcp 段落；api_key 等字符串安全转义）
pub fn save_config(cfg: &AgentConfig) {
    fn q(s: &str) -> String {
        toml::Value::String(s.to_string()).to_string()
    }
    let mut out = String::new();
    if let Some(llm) = &cfg.llm {
        out += "[llm]\n";
        out += &format!("backend = {}\n", q(&llm.backend));
        out += &format!("base_url = {}\n", q(&llm.base_url));
        if let Some(k) = &llm.api_key {
            out += &format!("api_key = {}\n", q(k));
        }
        out += &format!("model = {}\n", q(&llm.model));
        if !llm.system_prompt.is_empty() && llm.system_prompt != default_system_prompt() {
            out += &format!("system_prompt = {}\n", q(&llm.system_prompt));
        }
        out += "\n";
    }
    out += "[security]\nshell_allow = [";
    for (i, item) in cfg.security.shell_allow.iter().enumerate() {
        if i > 0 {
            out += ", ";
        }
        out += &q(item);
    }
    out += "]\n";
    for m in &cfg.mcp {
        out += "\n[[mcp]]\n";
        out += &format!("name = {}\n", q(&m.name));
        out += &format!("command = {}\n", q(&m.command));
        out += "args = [";
        for (i, arg) in m.args.iter().enumerate() {
            if i > 0 {
                out += ", ";
            }
            out += &q(arg);
        }
        out += "]\n";
    }
    let _ = std::fs::create_dir_all(config_dir());
    if let Err(e) = std::fs::write(config_dir().join("agent.toml"), out) {
        eprintln!("[agent] 保存配置失败: {e}");
    }
}

pub fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("hyprpet")
}

// ---------- 消息协议 ----------

pub enum AgentRequest {
    Chat(String),
}

pub enum AgentEvent {
    /// 宠物要说的话（LLM 回复 / 本地命令输出）
    Reply(String),
    /// 定时器请求（主线程到时触发气泡）
    Timer(f32, String),
    /// 工作状态（气泡里显示 … ）
    Busy(bool),
    /// mini 模式切换 / 收边 / 弹出
    ToggleMini,
    Hide,
    Show,
}

// ---------- MCP 工具镜像 ----------

struct McpTool {
    server: String,
    name: String,
    description: String,
    schema: serde_json::Value,
}

// ---------- worker ----------

/// 启动 agent worker 线程，返回请求发送端
pub fn spawn_agent(
    tx_main: calloop::channel::Sender<AgentEvent>,
) -> mpsc::Sender<AgentRequest> {
    let (tx_req, rx_req) = mpsc::channel::<AgentRequest>();
    std::thread::spawn(move || {
        let config = AgentConfig::load();
        let http = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .ok();

        // MCP 初始化（独立 tokio runtime）
        let mut mcp_tools: Vec<McpTool> = Vec::new();
        let rt = tokio::runtime::Runtime::new().ok();
        let mut mcp_peers: Vec<(String, rmcp::service::RunningService<rmcp::RoleClient, ()>)> = Vec::new();
        if !config.mcp.is_empty() {
            if let Some(rt) = &rt {
                for server in &config.mcp {
                    match init_mcp_server(rt, server) {
                        Ok((tools, running)) => {
                            eprintln!("[agent] MCP server '{}' 已连接（{} 个工具）", server.name, tools.len());
                            mcp_tools.extend(tools);
                            mcp_peers.push((server.name.clone(), running));
                        }
                        Err(e) => eprintln!("[agent] MCP server '{}' 连接失败: {e:#}", server.name),
                    }
                }
            }
        }

        let mut mcp_sig = format!("{:?}", config.mcp);
        while let Ok(req) = rx_req.recv() {
            match req {
                AgentRequest::Chat(text) => {
                    // 配置热重载（菜单设置 / hyprpet mcp CLI 改动即生效）
                    let fresh = AgentConfig::load();
                    let new_sig = format!("{:?}", fresh.mcp);
                    if new_sig != mcp_sig {
                        mcp_sig = new_sig;
                        for (_, r) in &mcp_peers {
                            r.cancellation_token().cancel();
                        }
                        mcp_peers.clear();
                        mcp_tools.clear();
                        if let Some(rt) = &rt {
                            for server in &fresh.mcp {
                                match init_mcp_server(rt, server) {
                                    Ok((tools, running)) => {
                                        eprintln!("[agent] MCP '{}' 已连接（{} 个工具）", server.name, tools.len());
                                        mcp_tools.extend(tools);
                                        mcp_peers.push((server.name.clone(), running));
                                    }
                                    Err(e) => eprintln!("[agent] MCP '{}' 连接失败: {e:#}", server.name),
                                }
                            }
                        }
                    }
                    let _ = tx_main.send(AgentEvent::Busy(true));
                    let reply = handle_chat(&fresh, http.as_ref(), rt.as_ref(), &mcp_peers, &mcp_tools, &text, &tx_main);
                    let _ = tx_main.send(AgentEvent::Busy(false));
                    let _ = tx_main.send(AgentEvent::Reply(reply));
                }
            }
        }
    });
    tx_req
}

/// 本地 `/` 命令（无 LLM 也可用）
fn local_command(text: &str, config: &AgentConfig, tx_main: &calloop::channel::Sender<AgentEvent>) -> Option<String> {
    let mut parts = text.split_whitespace();
    let cmd = parts.next()?;
    match cmd {
        "/timer" => {
            let minutes: f32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(5.0);
            let label = parts.collect::<Vec<_>>().join(" ");
            let label = if label.is_empty() { "计时结束".into() } else { label };
            let _ = tx_main.send(AgentEvent::Timer(minutes, label.clone()));
            Some(format!("好的喵，{} 分钟后提醒你：{}", minutes, label))
        }
        "/status" => Some(system_status()),
        "/mini" => {
            let _ = tx_main.send(AgentEvent::ToggleMini);
            Some("已切换 mini 模式喵".into())
        }
        "/hide" => {
            let _ = tx_main.send(AgentEvent::Hide);
            Some("已收进屏幕边缘，点把手或 /show 弹出".into())
        }
        "/show" => {
            let _ = tx_main.send(AgentEvent::Show);
            Some("弹出来啦".into())
        }
        "/help" => Some(
            "可用命令：\n/timer <分钟> [事项] — 定时提醒\n/status — 系统状态\n/shell <命令> — 白名单命令执行\n/mini — 切换 mini 模式\n/hide — 收进屏幕边缘\n/show — 弹出\n\n配置 LLM（~/.config/hyprpet/agent.toml）后可直接聊天"
                .into(),
        ),
        _ => {
            if let Some(rest) = text.strip_prefix("/shell ") {
                Some(run_whitelisted(rest, config))
            } else {
                None
            }
        }
    }
}

fn handle_chat(
    config: &AgentConfig,
    http: Option<&reqwest::blocking::Client>,
    rt: Option<&tokio::runtime::Runtime>,
    mcp_peers: &[(String, rmcp::service::RunningService<rmcp::RoleClient, ()>)],
    mcp_tools: &[McpTool],
    text: &str,
    tx_main: &calloop::channel::Sender<AgentEvent>,
) -> String {
    if let Some(out) = local_command(text, config, tx_main) {
        return out;
    }
    let (Some(llm), Some(http)) = (&config.llm, http) else {
        return "喵？我还没有接入大脑～\n在 ~/.config/hyprpet/agent.toml 配置 [llm] 后，我就能回答问题、调用工具了。输入 /help 查看本地命令。".into();
    };
    let Some(api_key) = config.api_key() else {
        return "LLM API key 未配置（支持 env:NAME / cmd:命令 / 字面量 三种方式）".into();
    };

    let mut messages: Vec<serde_json::Value> = vec![
        serde_json::json!({"role": "system", "content": llm.system_prompt}),
        serde_json::json!({"role": "user", "content": text}),
    ];
    let tools = tool_schemas(llm, mcp_tools);

    for _round in 0..6 {
        let resp = match llm.backend.as_str() {
            "anthropic" => call_anthropic(http, llm, &api_key, &messages, &tools),
            _ => call_openai(http, llm, &api_key, &messages, &tools),
        };
        let Ok(resp) = resp else {
            return "呜……大脑短路了（LLM 请求失败），待会再试喵".into();
        };

        // 提取文本与工具调用
        let (text_out, calls) = resp;
        if calls.is_empty() {
            return if text_out.is_empty() {
                "（沉默）".into()
            } else {
                text_out
            };
        }
        if !text_out.is_empty() {
            // 中间思考文本不入库，直接追加用户可见气泡由主线程处理；这里仅入 messages
        }
        push_assistant(&mut messages, llm, &text_out, &calls);

        // 执行工具
        for (id, name, args) in calls {
            let result = if let Some(mcp) = mcp_tools.iter().find(|t| tool_full_name(t) == name) {
                exec_mcp_tool(rt, mcp_peers, mcp, args)
            } else {
                exec_builtin(&name, &args, config, tx_main)
            };
            push_tool_result(&mut messages, llm, &id, &result);
        }
    }
    "工具调用次数超限，先到这里喵".into()
}

/// 工具 schema（OpenAI 格式；anthropic 复用，调用时再转换）
fn tool_schemas(llm: &LlmConfig, mcp_tools: &[McpTool]) -> Vec<serde_json::Value> {
    let builtin = builtin_tool_schemas();
    let mcp = mcp_tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": tool_full_name(t),
                    "description": t.description,
                    "parameters": t.schema,
                }
            })
        })
        .collect::<Vec<_>>();
    let _ = llm;
    builtin.into_iter().chain(mcp).collect()
}

fn tool_full_name(t: &McpTool) -> String {
    format!("mcp_{}_{}", t.server.replace(['-', '.'], "_"), t.name.replace(['-', '.'], "_"))
}

fn builtin_tool_schemas() -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "get_system_status",
                "description": "获取系统 CPU / 内存 / 进程概况",
                "parameters": {"type": "object", "properties": {}},
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "set_timer",
                "description": "设置一个定时提醒（到点弹出气泡通知）",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "minutes": {"type": "number", "description": "分钟数"},
                        "label": {"type": "string", "description": "提醒内容"},
                    },
                    "required": ["minutes"],
                },
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "get_clipboard",
                "description": "读取系统剪贴板文本（最多前 500 字符）",
                "parameters": {"type": "object", "properties": {}},
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "run_command",
                "description": "执行白名单内的本地命令并返回输出",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": {"type": "string", "description": "完整命令行"},
                    },
                    "required": ["command"],
                },
            }
        }),
    ]
}

// ---------- LLM API ----------

type ToolCall = (String, String, serde_json::Value);

fn call_openai(
    http: &reqwest::blocking::Client,
    llm: &LlmConfig,
    api_key: &str,
    messages: &[serde_json::Value],
    tools: &[serde_json::Value],
) -> Result<(String, Vec<ToolCall>)> {
    let body = serde_json::json!({
        "model": llm.model,
        "messages": messages,
        "tools": tools,
        "temperature": 0.7,
    });
    let resp = http
        .post(format!("{}/chat/completions", llm.base_url.trim_end_matches('/')))
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .context("LLM 请求发送失败")?;
    if !resp.status().is_success() {
        anyhow::bail!("LLM HTTP {}: {}", resp.status(), resp.text().unwrap_or_default().chars().take(200).collect::<String>());
    }
    let v: serde_json::Value = resp.json()?;
    let msg = &v["choices"][0]["message"];
    let text = msg["content"].as_str().unwrap_or("").to_string();
    let calls = msg["tool_calls"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|c| {
                    Some((
                        c["id"].as_str()?.to_string(),
                        c["function"]["name"].as_str()?.to_string(),
                        serde_json::from_str(c["function"]["arguments"].as_str().unwrap_or("{}")).ok()?,
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    Ok((text, calls))
}

fn call_anthropic(
    http: &reqwest::blocking::Client,
    llm: &LlmConfig,
    api_key: &str,
    messages: &[serde_json::Value],
    tools: &[serde_json::Value],
) -> Result<(String, Vec<ToolCall>)> {
    // 转换 OpenAI 消息格式 → Anthropic
    let mut anth_messages = Vec::new();
    for m in messages {
        let role = m["role"].as_str().unwrap_or("user");
        if role == "system" {
            continue; // system 单独传
        }
        if role == "tool" {
            anth_messages.push(serde_json::json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": m["tool_call_id"],
                    "content": m["content"],
                }],
            }));
            continue;
        }
        anth_messages.push(m.clone());
    }
    let anth_tools: Vec<serde_json::Value> = tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "name": t["function"]["name"],
                "description": t["function"]["description"],
                "input_schema": t["function"]["parameters"],
            })
        })
        .collect();
    let system = messages
        .first()
        .filter(|m| m["role"] == "system")
        .map(|m| m["content"].clone())
        .unwrap_or(serde_json::Value::Null);

    let body = serde_json::json!({
        "model": llm.model,
        "max_tokens": 1024,
        "system": system,
        "messages": anth_messages,
        "tools": anth_tools,
    });
    let base = llm.base_url.trim_end_matches('/');
    let base = base.strip_suffix("/v1").unwrap_or(base);
    let resp = http
        .post(format!("{}/v1/messages", base))
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .send()
        .context("LLM 请求发送失败")?;
    if !resp.status().is_success() {
        anyhow::bail!("LLM HTTP {}: {}", resp.status(), resp.text().unwrap_or_default().chars().take(200).collect::<String>());
    }
    let v: serde_json::Value = resp.json()?;
    let mut text = String::new();
    let mut calls = Vec::new();
    for block in v["content"].as_array().cloned().unwrap_or_default() {
        match block["type"].as_str() {
            Some("text") => text.push_str(block["text"].as_str().unwrap_or("")),
            Some("tool_use") => calls.push((
                block["id"].as_str().unwrap_or("").to_string(),
                block["name"].as_str().unwrap_or("").to_string(),
                block["input"].clone(),
            )),
            _ => {}
        }
    }
    Ok((text, calls))
}

fn push_assistant(messages: &mut Vec<serde_json::Value>, llm: &LlmConfig, text: &str, calls: &[ToolCall]) {
    if llm.backend == "anthropic" {
        let mut content = vec![];
        if !text.is_empty() {
            content.push(serde_json::json!({"type": "text", "text": text}));
        }
        for (id, name, args) in calls {
            content.push(serde_json::json!({
                "type": "tool_use", "id": id, "name": name, "input": args,
            }));
        }
        messages.push(serde_json::json!({"role": "assistant", "content": content}));
    } else {
        let mut msg = serde_json::json!({"role": "assistant", "content": if text.is_empty() { serde_json::Value::Null } else { text.into() }});
        if !calls.is_empty() {
            msg["tool_calls"] = calls
                .iter()
                .map(|(id, name, args)| {
                    serde_json::json!({
                        "id": id,
                        "type": "function",
                        "function": {"name": name, "arguments": args.to_string()},
                    })
                })
                .collect();
        }
        messages.push(msg);
    }
}

fn push_tool_result(messages: &mut Vec<serde_json::Value>, llm: &LlmConfig, id: &str, result: &str) {
    if llm.backend == "anthropic" {
        messages.push(serde_json::json!({
            "role": "user",
            "content": [{"type": "tool_result", "tool_use_id": id, "content": result}],
        }));
    } else {
        messages.push(serde_json::json!({
            "role": "tool", "tool_call_id": id, "content": result,
        }));
    }
}

// ---------- 内置工具执行 ----------

fn exec_builtin(name: &str, args: &serde_json::Value, config: &AgentConfig, tx_main: &calloop::channel::Sender<AgentEvent>) -> String {
    match name {
        "get_system_status" => system_status(),
        "set_timer" => {
            let minutes = args["minutes"].as_f64().unwrap_or(5.0) as f32;
            let label = args["label"].as_str().unwrap_or("计时结束").to_string();
            let _ = tx_main.send(AgentEvent::Timer(minutes, label.clone()));
            format!("已设置 {} 分钟后的提醒：{}", minutes, label)
        }
        "get_clipboard" => match Command::new("wl-paste").arg("--no-newline").output() {
            Ok(o) if o.status.success() => {
                let s = String::from_utf8_lossy(&o.stdout);
                s.chars().take(500).collect()
            }
            _ => "（剪贴板为空或不是文本）".into(),
        },
        "run_command" => {
            let cmd = args["command"].as_str().unwrap_or("");
            run_whitelisted(cmd, config)
        }
        _ => format!("未知工具：{name}"),
    }
}

fn run_whitelisted(command: &str, config: &AgentConfig) -> String {
    let allowed = config.security.shell_allow.iter().any(|prefix| command.trim_start().starts_with(prefix.as_str()));
    if !allowed {
        return format!("命令不在白名单内，已拒绝：{command}");
    }
    let mut child = match Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return format!("启动失败: {e}"),
    };
    // 超时 10s
    let start = std::time::Instant::now();
    loop {
        if let Some(status) = child.try_wait().ok().flatten() {
            let mut out = String::new();
            child.stdout.take().map(|mut s| s.read_to_string(&mut out).ok());
            child.stderr.take().map(|mut s| s.read_to_string(&mut out).ok());
            return format!("exit={status}\n{}", out.chars().take(1000).collect::<String>());
        }
        if start.elapsed() > std::time::Duration::from_secs(10) {
            child.kill().ok();
            return "命令超时（10s），已终止".into();
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn system_status() -> String {
    use sysinfo::System;
    let mut sys = System::new();
    sys.refresh_memory();
    std::thread::sleep(std::time::Duration::from_millis(120));
    sys.refresh_cpu_usage();
    let cpu = sys.global_cpu_usage();
    let mem_used = sys.used_memory() as f64 / 1024.0 / 1024.0 / 1024.0;
    let mem_total = sys.total_memory() as f64 / 1024.0 / 1024.0 / 1024.0;
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    format!(
        "CPU {:.0}% | 内存 {:.1}/{:.1} GB ({:.0}%) | 负载 {}",
        cpu,
        mem_used,
        mem_total,
        mem_used / mem_total * 100.0,
        sysinfo::System::load_average().one
    )
}

// ---------- MCP ----------

fn init_mcp_server(
    rt: &tokio::runtime::Runtime,
    server: &McpServer,
) -> Result<(Vec<McpTool>, rmcp::service::RunningService<rmcp::RoleClient, ()>)> {
    let mut cmd = tokio::process::Command::new(&server.command);
    cmd.args(&server.args);
    let (tools, running) = rt.block_on(async move {
        use rmcp::ServiceExt;
        use rmcp::transport::TokioChildProcess;
        let transport = TokioChildProcess::new(cmd)?;
        let running = ().serve(transport).await?;
        let tools = running.peer().list_all_tools().await?;
        Ok::<_, anyhow::Error>((tools, running))
    })?;
    let mirrored = tools
        .into_iter()
        .map(|t| McpTool {
            server: server.name.clone(),
            name: t.name.to_string(),
            description: t.description.map(|d| d.into_owned()).unwrap_or_default(),
            schema: serde_json::Value::Object((*t.input_schema).clone()),
        })
        .collect();
    Ok((mirrored, running))
}

fn exec_mcp_tool(
    rt: Option<&tokio::runtime::Runtime>,
    peers: &[(String, rmcp::service::RunningService<rmcp::RoleClient, ()>)],
    tool: &McpTool,
    args: serde_json::Value,
) -> String {
    let Some(rt) = rt else { return "MCP 不可用".into() };
    let Some((_, running)) = peers.iter().find(|(n, _)| *n == tool.server) else {
        return format!("MCP server '{}' 未连接", tool.server);
    };
    let result = rt.block_on(async {
        use rmcp::model::CallToolRequestParam;
        let mut map = serde_json::Map::new();
        if let serde_json::Value::Object(o) = args {
            map = o;
        }
        running
            .peer()
            .call_tool(CallToolRequestParam {
                name: std::borrow::Cow::Owned(tool.name.clone()),
                arguments: Some(map),
            })
            .await
    });
    match result {
        Ok(r) => {
            let text: Vec<String> = r
                .content
                .iter()
                .filter_map(|c| {
                    use rmcp::model::RawContent;
                    match &c.raw {
                        RawContent::Text(t) => Some(t.text.clone()),
                        _ => None,
                    }
                })
                .collect();
            let joined = text.join("\n");
            if joined.chars().count() > 800 {
                joined.chars().take(800).collect::<String>() + "…"
            } else {
                joined
            }
        }
        Err(e) => format!("MCP 工具执行失败: {e}"),
    }
}

// 保持 HashMap 导入（工具参数用）
#[allow(dead_code)]
fn _unused(_: HashMap<String, String>) {}
