//! `hyprpet mcp` CLI：MCP server 快速接入与主流 agent 配置导入。
//!
//! ```text
//! hyprpet mcp add <name> <command> [args...]   # 手动添加 stdio MCP server
//! hyprpet mcp remove <name>                    # 移除
//! hyprpet mcp list                             # 列出
//! hyprpet mcp import [all|claude-code|claude-desktop|codex|pi|opencode|chatgpt|gemini|cursor|vscode]
//! ```
//!
//! 导入源（文件存在才导入，格式自动识别）：
//! - claude-code    ~/.claude.json（+ 当前目录 .mcp.json）的 mcpServers
//! - claude-desktop ~/.claude/claude_desktop_config.json 的 mcpServers
//! - codex          ~/.codex/config.toml 的 [mcp_servers.*]
//! - pi             ~/.pi/agent/mcp.json 的 mcpServers
//! - opencode       ~/.config/opencode/opencode.json 的 mcp（local 类型）
//! - chatgpt        ~/.chatgpt/mcp_settings.json 的 mcpServers（ChatGPT 桌面端）
//! - gemini         ~/.gemini/settings.json 的 mcpServers
//! - cursor         ~/.cursor/mcp.json 的 mcpServers
//! - vscode         ./.vscode/mcp.json 的 servers

use std::path::PathBuf;

use crate::agent::{AgentConfig, McpServer};

pub fn mcp_main(args: &[String]) -> i32 {
    let Some(sub) = args.first().map(|s| s.as_str()) else {
        usage();
        return 2;
    };
    match sub {
        "add" => cmd_add(args.get(1..).unwrap_or(&[])),
        "remove" | "rm" => cmd_remove(args.get(1..).unwrap_or(&[])),
        "list" | "ls" => cmd_list(),
        "import" => cmd_import(args.get(1).map(|s| s.as_str()).unwrap_or("all")),
        "serve" => crate::mcp_serve::serve(),
        "setup" => cmd_setup(args.get(1).map(|s| s.as_str()).unwrap_or("")),
        "export" => cmd_export(args.get(1).map(|s| s.as_str()).unwrap_or("all")),
        _ => {
            usage();
            2
        }
    }
}

/// 可注册的目标 agent
const SETUP_TARGETS: &[(&str, &str)] = &[
    ("claude-code", "~/.claude.json"),
    ("claude-desktop", "~/.claude/claude_desktop_config.json"),
    ("pi", "~/.pi/agent/mcp.json"),
    ("chatgpt", "~/.chatgpt/mcp_settings.json"),
    ("gemini", "~/.gemini/settings.json"),
    ("cursor", "~/.cursor/mcp.json"),
    ("opencode", "~/.config/opencode/opencode.json"),
];

/// hyprpet mcp setup <agent>：把「hyprpet mcp serve」注册为指定 agent 的 MCP server
fn cmd_setup(target: &str) -> i32 {
    if target.is_empty() || target == "all" {
        println!("目标: {}", SETUP_TARGETS.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(" / "));
        println!("codex 与 vscode 请用: hyprpet mcp export codex|vscode");
        for (name, _) in SETUP_TARGETS {
            if cmd_setup(name) != 0 {
                return 1;
            }
        }
        let _ = cmd_setup_codex();
        return 0;
    }
    let exe = std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "hyprpet".into());
    let args = vec!["mcp".to_string(), "serve".to_string()];

    match target {
        "codex" => return cmd_setup_codex(),
        "vscode" => {
            let path = std::path::PathBuf::from(".vscode/mcp.json");
            let r = merge_json(&path, &["servers", "hyprpet"], &serde_json::json!({
                "command": exe, "args": args }));
            return report(r, &format!(".vscode/mcp.json（{:?}）", std::env::current_dir().ok()));
        }
        _ => {}
    }
    let Some((_, path)) = SETUP_TARGETS.iter().find(|(n, _)| *n == target) else {
        eprintln!("未知目标 {target}，可选: all / {}", SETUP_TARGETS.iter().map(|(n,_)|*n).collect::<Vec<_>>().join(" / "));
        return 2;
    };
    let path = expand_home(path);
    if target == "opencode" {
        let r = merge_json(&path, &["mcp", "hyprpet"], &serde_json::json!({
            "type": "local", "command": [exe, "mcp", "serve"] }));
        report(r, &format!("{}", path.display()))
    } else {
        let r = merge_json(&path, &["mcpServers", "hyprpet"], &serde_json::json!({
            "command": exe, "args": args }));
        report(r, &format!("{}", path.display()))
    }
}

fn cmd_setup_codex() -> i32 {
    let exe = std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "hyprpet".into());
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let path = home.join(".codex/config.toml");
    let mut v: toml::Value = std::fs::read_to_string(&path).ok().and_then(|s| s.parse().ok()).unwrap_or(toml::Value::Table(toml::map::Map::new()));
    let entry = toml::Value::Table(toml::map::Map::from_iter([
        ("command".to_string(), toml::Value::String(exe)),
        ("args".to_string(), toml::Value::Array(vec!["mcp".into(), "serve".into()])),
    ]));
    toml_set(&mut v, &["mcp_servers", "hyprpet"], entry);
    let out = match toml::to_string_pretty(&v) {
        Ok(s) => s,
        Err(e) => { eprintln!("序列化失败: {e}"); return 1; }
    };
    backup(&path);
    match std::fs::write(&path, out) {
        Ok(_) => { println!("已注册到 {}", path.display()); 0 }
        Err(e) => { eprintln!("写入 {} 失败: {e}", path.display()); 1 }
    }
}

/// hyprpet mcp export <agent|all>：把本机 agent.toml 的 [[mcp]] 列表推送到其他 agent 配置
fn cmd_export(target: &str) -> i32 {
    let cfg = AgentConfig::load();
    if cfg.mcp.is_empty() {
        println!("hyprpet 未配置 MCP server（先 hyprpet mcp add）");
        return 0;
    }
    let targets: Vec<(&str, &str)> = if target == "all" {
        SETUP_TARGETS.to_vec()
    } else {
        match SETUP_TARGETS.iter().find(|(n, _)| *n == target) {
            Some(t) => vec![*t],
            None => {
                eprintln!("未知目标 {target}");
                return 2;
            }
        }
    };
    for (name, path) in targets {
        let path = expand_home(path);
        for m in &cfg.mcp {
            let r = merge_json(&path, &["mcpServers", &m.name], &serde_json::json!({
                "command": m.command, "args": m.args }));
            if r.is_err() {
                eprintln!("[{name}] 写入 {} 失败: {}", path.display(), r.err().unwrap());
            }
        }
        println!("[{name}] 已写入 {}（{} 个 server）", path.display(), cfg.mcp.len());
    }
    if target == "all" || target == "codex" {
        // codex 用 TOML
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        let path = home.join(".codex/config.toml");
        let mut v: toml::Value = std::fs::read_to_string(&path).ok().and_then(|s| s.parse().ok()).unwrap_or(toml::Value::Table(toml::map::Map::new()));
        for m in &cfg.mcp {
            let entry = toml::Value::Table(toml::map::Map::from_iter([
                ("command".to_string(), toml::Value::String(m.command.clone())),
                ("args".to_string(), toml::Value::Array(m.args.iter().map(|a| a.as_str().into()).collect())),
            ]));
            toml_set(&mut v, &["mcp_servers", &m.name], entry);
        }
        match toml::to_string_pretty(&v) {
            Ok(out) => {
                backup(&path);
                let _ = std::fs::write(&path, out);
            }
            Err(e) => eprintln!("[codex] 序列化失败: {e}"),
        }
        println!("[codex] 已写入 {}", path.display());
    }
    0
}

/// 修改配置文件前的强制备份（保留原文件与权限）
fn backup(path: &std::path::Path) {
    if !path.exists() {
        return;
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let bak = path.with_file_name(format!(
        "{}.hyprpet-bak-{}",
        path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        ts
    ));
    if let Err(e) = std::fs::copy(path, &bak) {
        eprintln!("警告: 备份 {} 失败: {e}（中止写入）", path.display());
        std::process::exit(1);
    }
    eprintln!("已备份 → {}", bak.display());
}

fn expand_home(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join(rest)
    } else {
        PathBuf::from(p)
    }
}

/// JSON 合并：在指定路径（逐层对象）写入 entry
fn merge_json(path: &std::path::Path, keys: &[&str], entry: &serde_json::Value) -> Result<(), String> {
    let mut v: serde_json::Value = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(serde_json::json!({}));
    if !v.is_object() {
        v = serde_json::json!({});
    }
    let mut cur = &mut v;
    for k in keys.iter().take(keys.len().saturating_sub(1)) {
        if !cur.get(*k).map(|x| x.is_object()).unwrap_or(false) {
            cur.as_object_mut().unwrap().insert((*k).to_string(), serde_json::json!({}));
        }
        cur = cur.get_mut(*k).unwrap();
    }
    let last = keys.last().ok_or("空路径")?;
    cur.as_object_mut().unwrap().insert((*last).to_string(), entry.clone());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    backup(path);
    std::fs::write(path, serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?).map_err(|e| e.to_string())
}

fn toml_set(v: &mut toml::Value, keys: &[&str], val: toml::Value) {
    if keys.is_empty() {
        *v = val;
        return;
    }
    if !v.is_table() {
        *v = toml::Value::Table(toml::map::Map::new());
    }
    let e = v.as_table_mut().unwrap().entry(keys[0]).or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    toml_set(e, &keys[1..], val);
}

fn report(r: Result<(), String>, where_: &str) -> i32 {
    match r {
        Ok(_) => {
            println!("已注册到 {where_}（重启该 agent 生效）");
            0
        }
        Err(e) => {
            eprintln!("写入 {where_} 失败: {e}");
            1
        }
    }
}

fn usage() {
    eprintln!(
        "用法:\n  hyprpet mcp add <name> <command> [args...]\n  hyprpet mcp remove <name>\n  hyprpet mcp list\n  hyprpet mcp import [all|claude-code|claude-desktop|codex|pi|opencode|chatgpt|gemini|cursor|vscode]"
    );
}

/// 统一的 server 描述（导入中间格式）
#[derive(Debug, Clone)]
struct ServerDef {
    name: String,
    command: String,
    args: Vec<String>,
}

fn cmd_add(args: &[String]) -> i32 {
    let (Some(name), Some(command)) = (args.first(), args.get(1)) else {
        eprintln!("用法: hyprpet mcp add <name> <command> [args...]");
        return 2;
    };
    let rest = args.get(2..).unwrap_or(&[]);
    let mut cfg = AgentConfig::load();
    cfg.mcp.retain(|m| m.name != *name);
    cfg.mcp.push(McpServer {
        name: name.clone(),
        command: command.clone(),
        args: rest.to_vec(),
    });
    crate::agent::save_config(&cfg);
    println!("已添加 [{name}] {command} {}（对话中即可调用）", rest.join(" "));
    0
}

fn cmd_remove(args: &[String]) -> i32 {
    let Some(name) = args.first() else {
        eprintln!("用法: hyprpet mcp remove <name>");
        return 2;
    };
    let mut cfg = AgentConfig::load();
    let before = cfg.mcp.len();
    cfg.mcp.retain(|m| m.name != *name);
    if cfg.mcp.len() == before {
        eprintln!("未找到 [{name}]");
        return 1;
    }
    crate::agent::save_config(&cfg);
    println!("已移除 [{name}]");
    0
}

fn cmd_list() -> i32 {
    let cfg = AgentConfig::load();
    if cfg.mcp.is_empty() {
        println!("（未配置 MCP server，用 `hyprpet mcp add` 或 `hyprpet mcp import all` 添加）");
    }
    for m in &cfg.mcp {
        println!("{}: {} {}", m.name, m.command, m.args.join(" "));
    }
    0
}

fn cmd_import(tool: &str) -> i32 {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let mut found: Vec<ServerDef> = Vec::new();

    let mut try_source = |label: &str, defs: Vec<ServerDef>| {
        if !defs.is_empty() {
            println!("[{label}] 发现 {} 个:", defs.len());
            for d in &defs {
                println!("  {}: {} {}", d.name, d.command, d.args.join(" "));
            }
            found.extend(defs);
        }
    };

    match tool {
        "all" => {
            try_source("claude-code", from_claude(&home.join(".claude.json")));
            try_source("claude-code(local)", from_claude(&PathBuf::from(".mcp.json")));
            try_source("claude-desktop", from_claude(&home.join(".claude/claude_desktop_config.json")));
            try_source("codex", from_codex(&home.join(".codex/config.toml")));
            try_source("pi", from_claude(&home.join(".pi/agent/mcp.json")));
            try_source("opencode", from_opencode(&home.join(".config/opencode/opencode.json")));
            try_source("chatgpt", from_claude(&home.join(".chatgpt/mcp_settings.json")));
            try_source("gemini", from_claude(&home.join(".gemini/settings.json")));
            try_source("cursor", from_claude(&home.join(".cursor/mcp.json")));
            try_source("vscode", from_vscode(&PathBuf::from(".vscode/mcp.json")));
        }
        "claude-code" => try_source("claude-code", from_claude(&home.join(".claude.json"))),
        "claude-desktop" => try_source("claude-desktop", from_claude(&home.join(".claude/claude_desktop_config.json"))),
        "codex" => try_source("codex", from_codex(&home.join(".codex/config.toml"))),
        "pi" => try_source("pi", from_claude(&home.join(".pi/agent/mcp.json"))),
        "opencode" => try_source("opencode", from_opencode(&home.join(".config/opencode/opencode.json"))),
        "chatgpt" => try_source("chatgpt", from_claude(&home.join(".chatgpt/mcp_settings.json"))),
        "gemini" => try_source("gemini", from_claude(&home.join(".gemini/settings.json"))),
        "cursor" => try_source("cursor", from_claude(&home.join(".cursor/mcp.json"))),
        "vscode" => try_source("vscode", from_vscode(&PathBuf::from(".vscode/mcp.json"))),
        other => {
            eprintln!("未知来源: {other}");
            return 2;
        }
    }

    if found.is_empty() {
        println!("未发现可导入的 MCP server（检查对应 agent 是否已配置 MCP）");
        return 0;
    }

    let mut cfg = AgentConfig::load();
    let mut added = 0;
    for d in found {
        if cfg.mcp.iter().any(|m| m.name == d.name) {
            println!("[{}] 已存在，跳过", d.name);
            continue;
        }
        cfg.mcp.push(McpServer { name: d.name, command: d.command, args: d.args });
        added += 1;
    }
    crate::agent::save_config(&cfg);
    println!("已导入 {added} 个 MCP server 到 ~/.config/hyprpet/agent.toml（热生效，新对话即可调用）");
    0
}

// ---------- 各来源解析 ----------

/// claude 系 JSON：{"mcpServers": {"name": {"command": "...", "args": [...]}}}
fn from_claude(path: &std::path::Path) -> Vec<ServerDef> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(
        &std::fs::read_to_string(path).unwrap_or_default(),
    ) else {
        return Vec::new();
    };
    let Some(map) = v.get("mcpServers").and_then(|m| m.as_object()) else {
        return Vec::new();
    };
    map.iter()
        .filter_map(|(name, cfg)| {
            let command = cfg.get("command")?.as_str()?.to_string();
            let args = cfg
                .get("args")
                .and_then(|a| a.as_array())
                .map(|arr| arr.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                .unwrap_or_default();
            Some(ServerDef { name: name.clone(), command, args })
        })
        .collect()
}

/// codex TOML：[mcp_servers.name] command / args
fn from_codex(path: &std::path::Path) -> Vec<ServerDef> {
    #[derive(serde::Deserialize, Default)]
    struct CodexConfig {
        mcp_servers: std::collections::HashMap<String, CodexServer>,
    }
    #[derive(serde::Deserialize)]
    struct CodexServer {
        command: Option<String>,
        args: Option<Vec<String>>,
    }
    let Ok(cfg) = toml::from_str::<CodexConfig>(&std::fs::read_to_string(path).unwrap_or_default())
    else {
        return Vec::new();
    };
    cfg.mcp_servers
        .into_iter()
        .filter_map(|(name, s)| {
            let command = s.command?;
            Some(ServerDef { name, command, args: s.args.unwrap_or_default() })
        })
        .collect()
}

/// opencode JSON：{"mcp": {"name": {"type": "local", "command": ["..."]}}}
fn from_opencode(path: &std::path::Path) -> Vec<ServerDef> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(
        &std::fs::read_to_string(path).unwrap_or_default(),
    ) else {
        return Vec::new();
    };
    let Some(map) = v.get("mcp").and_then(|m| m.as_object()) else {
        return Vec::new();
    };
    map.iter()
        .filter_map(|(name, cfg)| {
            // 仅支持 local（stdio）类型
            if cfg.get("type").and_then(|t| t.as_str()) == Some("remote") {
                eprintln!("  (跳过远程 MCP [{name}]，hyprpet 仅支持 stdio)");
                return None;
            }
            let command = cfg.get("command")?;
            let (cmd, mut args) = match command {
                serde_json::Value::String(s) => (s.clone(), Vec::new()),
                serde_json::Value::Array(arr) => {
                    let mut it = arr.iter().filter_map(|x| x.as_str().map(String::from));
                    let cmd = it.next()?;
                    (cmd, it.collect())
                }
                _ => return None,
            };
            if let Some(extra) = cfg.get("args").and_then(|a| a.as_array()) {
                args.extend(extra.iter().filter_map(|x| x.as_str().map(String::from)));
            }
            Some(ServerDef { name: name.clone(), command: cmd, args })
        })
        .collect()
}

/// vscode：{"servers": {"name": {"command": ..., "args": [...]}}}
fn from_vscode(path: &std::path::Path) -> Vec<ServerDef> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(
        &std::fs::read_to_string(path).unwrap_or_default(),
    ) else {
        return Vec::new();
    };
    let Some(map) = v.get("servers").and_then(|m| m.as_object()) else {
        return Vec::new();
    };
    map.iter()
        .filter_map(|(name, cfg)| {
            let command = cfg.get("command")?.as_str()?.to_string();
            let args = cfg
                .get("args")
                .and_then(|a| a.as_array())
                .map(|arr| arr.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                .unwrap_or_default();
            Some(ServerDef { name: name.clone(), command, args })
        })
        .collect()
}

// ===================== hyprpet char：快捷添加宠物 =====================

/// `hyprpet char add <名字> <图片.png> [--frame 宽x高] [--force]`
/// 任意 PNG 一键生成角色目录：拷贝贴图 + meta.toml + 配套 actions.toml。
/// 单帧图片 → 所有动作用同一帧；--frame 指定帧尺寸 → 按行智能分配动作。
pub fn char_main(args: &[String]) -> i32 {
    let Some(sub) = args.first().map(|s| s.as_str()) else {
        eprintln!("用法:\n  hyprpet char add <名字> <图片.png> [--frame 宽x高] [--force]\n  hyprpet char list\n  hyprpet char remove <名字>");
        return 2;
    };
    match sub {
        "add" => char_add(args.get(1..).unwrap_or(&[])),
        "list" | "ls" => char_list(),
        "remove" | "rm" => char_remove(args.get(1).map(|s| s.as_str()).unwrap_or("")),
        _ => {
            eprintln!("未知子命令 {sub}（add/list/remove）");
            2
        }
    }
}

fn char_add(args: &[String]) -> i32 {
    let name = args.first().cloned().unwrap_or_default();
    let image = args.get(1).cloned().unwrap_or_default();
    if name.is_empty() || image.is_empty() {
        eprintln!("用法: hyprpet char add <名字> <图片.png> [--frame 宽x高] [--force]");
        return 2;
    }
    if name.contains(['/', '\\']) || name.contains("..") {
        eprintln!("名字不能含路径分隔符");
        return 2;
    }
    let mut frame: Option<(u32, u32)> = None;
    let mut force = false;
    let mut it = args.get(2..).unwrap_or(&[]).iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--frame" => {
                let v = it.next().map(|s| s.as_str()).unwrap_or("");
                let mut p = v.split(['x', 'X', '*']);
                if let (Some(Ok(w)), Some(Ok(h))) = (p.next().map(str::parse::<u32>), p.next().map(str::parse::<u32>)) {
                    frame = Some((w, h));
                } else {
                    eprintln!("--frame 格式: 宽x高，如 32x32");
                    return 2;
                }
            }
            "--force" => force = true,
            other => eprintln!("忽略未知参数: {other}"),
        }
    }

    let img_path = std::path::PathBuf::from(&image);
    if !img_path.is_file() {
        eprintln!("图片不存在: {image}");
        return 1;
    }
    let (w, h) = match image::image_dimensions(&img_path) {
        Ok(d) => d,
        Err(e) => { eprintln!("读取图片失败: {e}"); return 1; }
    };

    // 帧尺寸：未指定 → 整张图为单帧；指定 → 校验整除
    let (fw, fh) = frame.unwrap_or((w, h));
    if fw == 0 || fh == 0 || w % fw != 0 || h % fh != 0 {
        eprintln!("帧尺寸 {fw}x{fh} 不能整除图片 {w}x{h}");
        return 2;
    }
    let cols = w / fw;
    let rows = h / fh;

    let dir = crate::assets::characters_dir().join(&name);
    if dir.exists() && !force {
        eprintln!("角色 [{name}] 已存在（用 --force 覆盖）");
        return 1;
    }

    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("创建目录失败: {e}");
        return 1;
    }
    if let Err(e) = std::fs::copy(&img_path, dir.join("sprites.png")) {
        eprintln!("拷贝贴图失败: {e}");
        return 1;
    }

    let meta = format!(
        "name = \"{name}\"\nauthor = \"hyprpet char add\"\nscale = 4\nframe_width = {fw}\nframe_height = {fh}\npreview = [0, 0]\n"
    );
    if let Err(e) = std::fs::write(dir.join("meta.toml"), meta) {
        eprintln!("写 meta.toml 失败: {e}");
        return 1;
    }
    if let Err(e) = std::fs::write(dir.join("actions.toml"), gen_actions(cols, rows)) {
        eprintln!("写 actions.toml 失败: {e}");
        return 1;
    }
    println!("✅ 宠物 [{name}] 已创建: {}", dir.display());
    println!("   {w}x{h}，帧 {fw}x{fh}（{cols} 列 × {rows} 行）");
    println!("   右键菜单已可换肤（热重载即时生效）");
    if rows < 2 {
        println!("   提示：单行动画表，动作均为 [0,0] 占位，按需编辑 actions.toml");
    }
    0
}

/// 按网格形状生成默认动作表
fn gen_actions(cols: u32, rows: u32) -> String {
    let join = |v: Vec<String>| v.join(", ");
    let row0 = join((0..cols.min(4)).map(|c| format!("[{c}, 0]")).collect());
    let r1 = if rows > 1 { 1 } else { 0 };
    let r2 = if rows > 2 { 2 } else { r1 };
    let last = rows - 1;
    let walk = join((0..cols.min(4)).map(|c| format!("[{c}, {r1}]")).collect());
    let walk_r = join((0..cols.min(4)).map(|c| format!("[{c}, {r2}]")).collect());
    let sleep = join((0..cols.min(3)).map(|c| format!("[{c}, {last}]")).collect());
    format!(
        r#"default = "idle"

[actions.idle]
fps = 2
loop = true
duration = 6
frames = [{row0}]
next_random = ["walk_left", "walk_right", "sleep"]

[actions.walk_left]
fps = 8
loop = true
duration = 4
movement = [-70, 0]
auto_turn = true
frames = [{walk}]
next_random = ["idle", "idle", "sleep"]

[actions.walk_right]
fps = 8
loop = true
duration = 4
movement = [70, 0]
auto_turn = true
frames = [{walk_r}]
next_random = ["idle", "idle", "sleep"]

[actions.sleep]
fps = 1.5
loop = true
duration = 8
frames = [{sleep}]
next = "idle"

# 以下动作统一占位 [0,0]，可改为任意帧
[actions.grabbed]
fps = 1
loop = true
frames = [[0, 0]]

[actions.fall]
fps = 1
loop = true
frames = [[0, 0]]

[actions.land]
fps = 8
loop = false
frames = [[0, 0]]
next = "idle"

[actions.alert]
fps = 6
loop = false
frames = [[0, 0], [0, 0]]
next = "idle"

[actions.tired]
fps = 2
loop = true
duration = 4
frames = [[0, 0]]
next = "idle"

[actions.celebrate]
fps = 6
loop = false
frames = [[0, 0], [0, 0]]
next = "idle"

[actions.study]
fps = 4
loop = true
duration = 12
frames = [[0, 0], [0, 0]]
next = "idle"

[actions.petted]
fps = 3
loop = false
frames = [[0, 0], [0, 0], [0, 0], [0, 0], [0, 0], [0, 0]]
next = "idle"
"#
    )
}

fn char_list() -> i32 {
    let list = crate::assets::scan_characters();
    if list.is_empty() {
        println!("（暂无角色）");
    }
    for (name, _) in list {
        println!("{name}");
    }
    0
}

fn char_remove(name: &str) -> i32 {
    if name.is_empty() {
        eprintln!("用法: hyprpet char remove <名字>");
        return 2;
    }
    let dir = crate::assets::characters_dir().join(name);
    if !dir.is_dir() {
        eprintln!("角色 [{name}] 不存在");
        return 1;
    }
    match std::fs::remove_dir_all(&dir) {
        Ok(_) => { println!("已删除 [{name}]"); 0 }
        Err(e) => { eprintln!("删除失败: {e}"); 1 }
    }
}
