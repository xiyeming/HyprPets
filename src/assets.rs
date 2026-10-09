//! 角色资产加载：characters/ 目录规范
//!
//! ```text
//! ~/.config/hyprpet/characters/
//! └── <角色目录>/
//!     ├── meta.toml      # 名称、作者、默认缩放、帧尺寸（网格格式必填）
//!     ├── sprites.png    # 贴图（网格表或 Aseprite 整图）
//!     ├── sprites.json   # 可选，Aseprite 导出 JSON（存在则优先）
//!     └── actions.toml   # 动作状态机定义
//! ```

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::sprite::{FrameRect, Sheet};

// ---------- meta.toml ----------

#[derive(Debug, Deserialize, Default)]
pub struct Meta {
    pub name: Option<String>,
    pub author: Option<String>,
    pub scale: Option<u32>,
    pub frame_width: Option<u32>,
    pub frame_height: Option<u32>,
    pub preview: Option<[u32; 2]>,
}

// ---------- actions.toml ----------

#[derive(Debug, Clone, Deserialize)]
pub struct ActionDef {
    #[serde(default = "default_fps")]
    pub fps: f32,
    /// 帧索引（加载时已把网格 [col,row] / aseprite tag 展开为索引）
    #[serde(default)]
    pub frames: Vec<usize>,
    #[serde(default = "default_true")]
    pub r#loop: bool,
    /// loop 动作的持续时间（秒），超时后转移
    pub duration: Option<f32>,
    /// 移动速度（像素/秒）
    pub movement: Option<[f32; 2]>,
    /// 碰到布局边界自动转向（walk_left ↔ walk_right 按名字镜像）
    #[serde(default)]
    pub auto_turn: bool,
    pub next: Option<String>,
    #[serde(default)]
    pub next_random: Vec<String>,
    /// 进入动作时播放的音效（相对角色目录）
    pub sound: Option<String>,
}

fn default_fps() -> f32 {
    4.0
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize)]
struct ActionsFile {
    default: Option<String>,
    #[serde(default)]
    actions: HashMap<String, ActionToml>,
}

/// actions.toml 中的原始条目：`frames`（网格坐标）与 `tag`（aseprite）二选一
#[derive(Debug, Deserialize)]
struct ActionToml {
    fps: Option<f32>,
    #[serde(rename = "loop")]
    loop_: Option<bool>,
    duration: Option<f32>,
    movement: Option<[f32; 2]>,
    #[serde(default)]
    auto_turn: bool,
    next: Option<String>,
    #[serde(default)]
    next_random: Vec<String>,
    /// 进入动作时播放的音效（相对角色目录）
    sound: Option<String>,
    /// 网格 [col,row] 列表
    frames: Option<Vec<[u32; 2]>>,
    /// Aseprite 帧标签
    tag: Option<String>,
}

impl ActionToml {
    fn into_def(self, name: &str, sheet: &Sheet, tags: &HashMap<String, (usize, usize)>) -> Result<ActionDef> {
        let frames = if let Some(grid) = self.frames {
            // [col,row] → 行优先索引
            grid.iter()
                .map(|[c, r]| (*r * sheet.cols + *c) as usize)
                .collect()
        } else if let Some(tag) = self.tag {
            let (from, to) = tags
                .get(&tag)
                .with_context(|| format!("动作 [{name}] 引用的 aseprite 标签 '{tag}' 不存在"))?;
            (*from..=*to).collect()
        } else {
            Vec::new()
        };
        Ok(ActionDef {
            fps: self.fps.unwrap_or_else(default_fps),
            frames,
            r#loop: self.loop_.unwrap_or_else(default_true),
            duration: self.duration,
            movement: self.movement,
            auto_turn: self.auto_turn,
            next: self.next,
            next_random: self.next_random,
            sound: self.sound,
        })
    }
}

// ---------- Aseprite JSON ----------

#[derive(Debug, Deserialize)]
struct AsepriteFile {
    frames: serde_json::Value,
    meta: AsepriteMeta,
}

#[derive(Debug, Deserialize)]
struct AsepriteMeta {
    #[serde(rename = "frameTags", default)]
    frame_tags: Vec<FrameTag>,
}

#[derive(Debug, Deserialize)]
struct FrameTag {
    name: String,
    from: usize,
    to: usize,
}

#[derive(Debug, Deserialize)]
struct AsepriteFrameRect {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

#[derive(Debug, Deserialize)]
struct AsepriteFrameEntry {
    frame: AsepriteFrameRect,
}

// ---------- Character ----------

pub struct Character {
    pub dir: PathBuf,
    pub meta: Meta,
    pub sheet: Sheet,
    pub actions: HashMap<String, ActionDef>,
    pub default_action: String,
}

impl Character {
    pub fn name(&self) -> String {
        self.meta
            .name
            .clone()
            .unwrap_or_else(|| self.dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())
    }

    pub fn load(dir: &Path) -> Result<Character> {
        let meta: Meta = if dir.join("meta.toml").exists() {
            toml::from_str(&fs::read_to_string(dir.join("meta.toml"))?)
                .context("解析 meta.toml 失败")?
        } else {
            Meta::default()
        };

        let json_path = dir.join("sprites.json");
        // Aseprite 帧标签：name → (from, to) 索引范围
        let mut tags: HashMap<String, (usize, usize)> = HashMap::new();
        let sheet = if json_path.exists() {
            Self::load_aseprite(dir, &json_path, &mut tags)?
        } else {
            let fw = meta.frame_width.context("网格格式需要在 meta.toml 指定 frame_width")?;
            let fh = meta.frame_height.context("网格格式需要在 meta.toml 指定 frame_height")?;
            let (pixels, w, h) = Sheet::load_pixels(&dir.join("sprites.png"))?;
            Sheet::grid(pixels, w, h, fw, fh)
        };

        // 解析动作
        let actions_raw: ActionsFile = toml::from_str(
            &fs::read_to_string(dir.join("actions.toml")).context("缺少 actions.toml")?,
        )
        .context("解析 actions.toml 失败")?;

        let mut actions = HashMap::new();
        for (name, toml) in actions_raw.actions {
            actions.insert(name.clone(), toml.into_def(&name, &sheet, &tags)?);
        }

        let default_action = actions_raw
            .default
            .filter(|d| actions.contains_key(d))
            .unwrap_or_else(|| "idle".to_string());

        Ok(Character { dir: dir.to_path_buf(), meta, sheet, actions, default_action })
    }

    /// Aseprite 格式：sprites.json + sprites.png
    fn load_aseprite(
        dir: &Path,
        json_path: &Path,
        tags: &mut HashMap<String, (usize, usize)>,
    ) -> Result<Sheet> {
        let file: AsepriteFile =
            serde_json::from_str(&fs::read_to_string(json_path)?).context("解析 sprites.json 失败")?;

        for t in &file.meta.frame_tags {
            tags.insert(t.name.clone(), (t.from, t.to));
        }

        // frames 可能是数组或 {文件名: 条目} 哈希（哈希无序，按键名排序保证稳定）
        let rects: Vec<FrameRect> = match &file.frames {
            serde_json::Value::Array(arr) => arr
                .iter()
                .map(|v| {
                    let e: AsepriteFrameEntry = serde_json::from_value(v.clone())?;
                    Ok(FrameRect { x: e.frame.x, y: e.frame.y, w: e.frame.w, h: e.frame.h })
                })
                .collect::<Result<_>>(),
            serde_json::Value::Object(map) => {
                let mut keys: Vec<_> = map.keys().cloned().collect();
                keys.sort();
                keys.iter()
                    .map(|k| {
                        let e: AsepriteFrameEntry =
                            serde_json::from_value(map[k].clone()).context("解析帧失败")?;
                        Ok(FrameRect { x: e.frame.x, y: e.frame.y, w: e.frame.w, h: e.frame.h })
                    })
                    .collect::<Result<_>>()
            }
            _ => bail!("sprites.json 的 frames 格式不支持"),
        }?;

        let (pixels, w, h) = Sheet::load_pixels(&dir.join("sprites.png"))?;
        Ok(Sheet::aseprite(pixels, w, h, rects))
    }

    /// meta 或 actions 中声明的预览帧
    pub fn preview_frame(&self) -> usize {
        if let Some([c, r]) = self.meta.preview {
            return (r * self.sheet.cols + c) as usize;
        }
        0
    }
}

// ---------- 目录工具 ----------

pub fn characters_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("hyprpet").join("characters")
}

/// 扫描所有角色目录 → (名称, 路径)
pub fn scan_characters() -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let dir = characters_dir();
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            // 至少要能认出角色：有 meta.toml 或 actions.toml
            if path.join("actions.toml").exists() || path.join("meta.toml").exists() {
                let name = fs::read_to_string(path.join("meta.toml"))
                    .ok()
                    .and_then(|s| toml::from_str::<Meta>(&s).ok())
                    .and_then(|m| m.name)
                    .unwrap_or_else(|| entry.file_name().to_string_lossy().into_owned());
                out.push((name, path));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// 首次运行：写入内置史莱姆角色
pub fn ensure_builtin_slime() -> Result<PathBuf> {
    let dir = characters_dir().join("builtin_slime");
    if !dir.join("actions.toml").exists() {
        fs::create_dir_all(&dir)?;
        crate::builtin_slime::build_sheet().save_png(&dir.join("sprites.png"))?;
        fs::write(dir.join("meta.toml"), SLIME_META)?;
        fs::write(dir.join("actions.toml"), SLIME_ACTIONS)?;
    }
    Ok(dir)
}

/// 首次运行：写入内置小猫角色
pub fn ensure_builtin_character() -> Result<PathBuf> {
    let dir = characters_dir().join("builtin_cat");
    if !dir.join("actions.toml").exists() {
        fs::create_dir_all(&dir)?;
        crate::builtin_cat::build_sheet().save_png(&dir.join("sprites.png"))?;
        fs::write(dir.join("meta.toml"), BUILTIN_META)?;
        fs::write(dir.join("actions.toml"), BUILTIN_ACTIONS)?;
    }
    Ok(dir)
}

const BUILTIN_META: &str = r#"
name = "像素小猫"
author = "hyprpet"
scale = 4
frame_width = 32
frame_height = 32
preview = [0, 0]
"#;

const BUILTIN_ACTIONS: &str = r#"
default = "idle"

# 坐姿发呆，偶尔眨眼；6 秒后随机转移
[actions.idle]
fps = 2
loop = true
duration = 6
frames = [[0, 0], [0, 0], [1, 0], [0, 0]]
next_random = ["walk_left", "walk_right", "walk_left", "sleep"]

# 向左走
[actions.walk_left]
fps = 8
loop = true
duration = 4
movement = [-70, 0]
auto_turn = true
frames = [[0, 1], [1, 1], [2, 1], [3, 1]]
next_random = ["idle", "idle", "sleep"]

# 向右走
[actions.walk_right]
fps = 8
loop = true
duration = 4
movement = [70, 0]
auto_turn = true
frames = [[0, 2], [1, 2], [2, 2], [3, 2]]
next_random = ["idle", "idle", "sleep"]

# 睡觉（带 Zzz）
[actions.sleep]
fps = 1.5
loop = true
duration = 8
frames = [[0, 3], [1, 3], [2, 3], [1, 3]]
next = "idle"

# 被抓住
[actions.grabbed]
fps = 1
loop = true
frames = [[3, 3]]

# 下落
[actions.fall]
fps = 1
loop = true
frames = [[4, 3]]

# 落地（短促）→ idle
[actions.land]
fps = 8
loop = false
frames = [[5, 3]]
next = "idle"

# 系统通知提醒 → idle
[actions.alert]
fps = 6
loop = false
frames = [[2, 0], [3, 0]]
next = "idle"

# 系统高负载擦汗 → idle
[actions.tired]
fps = 2
loop = true
duration = 4
frames = [[6, 3]]
next = "idle"

# 庆祝（预留：任务成功时触发）→ idle
[actions.celebrate]
fps = 6
loop = false
frames = [[7, 3], [7, 3], [3, 0]]
next = "idle"

# 陪读/打字（检测到编码进程）→ idle
[actions.study]
fps = 4
loop = true
duration = 12
frames = [[3, 3], [4, 3]]
next = "idle"

# 被点击（开心）→ idle
[actions.petted]
fps = 6
loop = false
frames = [[3, 0], [3, 0]]
next = "idle"
"#;

const SLIME_META: &str = r#"
name = "像素史莱姆"
author = "hyprpet"
scale = 4
frame_width = 32
frame_height = 32
preview = [0, 0]
"#;

const SLIME_ACTIONS: &str = r#"
default = "idle"

[actions.idle]
fps = 2
loop = true
duration = 6
frames = [[0, 0], [1, 0], [0, 0], [0, 0]]
next_random = ["walk_left", "walk_right", "walk_left", "sleep"]

[actions.walk_left]
fps = 8
loop = true
duration = 4
movement = [-70, 0]
auto_turn = true
frames = [[0, 1], [1, 1], [2, 1], [3, 1]]
next_random = ["idle", "idle", "sleep"]

[actions.walk_right]
fps = 8
loop = true
duration = 4
movement = [70, 0]
auto_turn = true
frames = [[0, 2], [1, 2], [2, 2], [3, 2]]
next_random = ["idle", "idle", "sleep"]

[actions.sleep]
fps = 1.5
loop = true
duration = 8
frames = [[0, 3], [1, 3], [0, 3]]
next = "idle"

[actions.grabbed]
fps = 1
loop = true
frames = [[2, 3]]

[actions.fall]
fps = 1
loop = true
frames = [[3, 3]]

[actions.land]
fps = 8
loop = false
frames = [[4, 3]]
next = "idle"

[actions.alert]
fps = 6
loop = false
frames = [[3, 0], [3, 0]]
next = "idle"

[actions.tired]
fps = 2
loop = true
duration = 4
frames = [[5, 3]]
next = "idle"

[actions.celebrate]
fps = 6
loop = false
frames = [[6, 3], [6, 3], [3, 0]]
next = "idle"

[actions.study]
fps = 4
loop = true
duration = 12
frames = [[2, 3], [3, 3]]
next = "idle"

[actions.petted]
fps = 6
loop = false
frames = [[3, 0], [3, 0]]
next = "idle"
"#;
