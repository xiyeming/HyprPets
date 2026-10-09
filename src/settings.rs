//! LLM 设置对话框：菜单/托盘唤起的像素表单，键盘独占输入。
//!
//! 四个字段：服务商 / 接口地址 / API Key / 模型。
//! ↑↓ 或 Tab 切换字段，直接输入，Enter 保存（写 agent.toml），Esc 取消。

use crate::agent::{AgentConfig, LlmConfig};
use crate::ui::{fill_rect, rgba, stroke_rect, FontKit, TEXT, TEXT_DIM};

const W: u32 = 420;
const PAD: i32 = 10;

const PANEL: u32 = rgba(22, 24, 34, 240);
const BORDER: u32 = rgba(140, 145, 175, 255);
const INPUT_BG: u32 = rgba(12, 13, 20, 255);
const FOCUS_BG: u32 = rgba(40, 46, 66, 255);
const CURSOR: u32 = rgba(255, 210, 90, 255);
const LABEL: u32 = TEXT_DIM;

pub struct Settings {
    pub rect: (i32, i32, u32, u32),
    /// [服务商, base_url, api_key, model]
    fields: [String; 4],
    labels: [&'static str; 4],
    pub focus: usize,
    cursor_on: bool,
}

impl Settings {
    pub fn load(anchor: (i32, i32), pet_w: i32, pet_h: i32, bbox: (i32, i32, i32, i32)) -> Settings {
        let cfg = AgentConfig::load();
        let llm = cfg.llm.clone().unwrap_or_else(|| LlmConfig {
            backend: "openai".into(),
            base_url: "https://api.openai.com/v1".into(),
            api_key: None,
            model: "gpt-4o-mini".into(),
            system_prompt: String::new(),
        });
        let row_h = 26 + 18;
        let height = 30 + row_h * 4 + 34 + PAD * 2;
        let mut x = anchor.0 + pet_w / 2 - W as i32 / 2;
        x = x.clamp(bbox.0, (bbox.2 - W as i32).max(bbox.0));
        let mut y = anchor.1 - height - 12;
        if y < bbox.1 {
            y = anchor.1 + pet_h + 12;
        }
        y = y.clamp(bbox.1, (bbox.3 - height).max(bbox.1));
        Settings {
            rect: (x, y, W, height as u32),
            fields: [
                llm.backend,
                llm.base_url,
                llm.api_key.unwrap_or_default(),
                llm.model,
            ],
            labels: ["服务商(openai/anthropic)", "接口地址 base_url", "API Key", "模型 model"],
            focus: 0,
            cursor_on: true,
        }
    }

    pub fn toggle_cursor(&mut self) {
        self.cursor_on = !self.cursor_on;
    }

    /// 当前聚焦字段的可变引用
    pub fn focused_mut(&mut self) -> &mut String {
        &mut self.fields[self.focus.min(3)]
    }

    pub fn focus_next(&mut self) {
        self.focus = (self.focus + 1) % 4;
    }

    pub fn focus_prev(&mut self) {
        self.focus = (self.focus + 3) % 4;
    }

    /// 保存到 agent.toml（保留现有 mcp 与 security）
    pub fn save(&self) {
        let mut cfg = AgentConfig::load();
        let backend = if self.fields[0].trim().is_empty() {
            "openai"
        } else {
            self.fields[0].trim()
        };
        cfg.llm = Some(LlmConfig {
            backend: backend.to_string(),
            base_url: if self.fields[1].trim().is_empty() {
                "https://api.openai.com/v1".into()
            } else {
                self.fields[1].trim().to_string()
            },
            api_key: {
                let k = self.fields[2].trim().to_string();
                if k.is_empty() {
                    None
                } else {
                    Some(k)
                }
            },
            model: if self.fields[3].trim().is_empty() {
                "gpt-4o-mini".into()
            } else {
                self.fields[3].trim().to_string()
            },
            system_prompt: cfg
                .llm
                .map(|l| l.system_prompt)
                .filter(|s| !s.is_empty())
                .unwrap_or_default(),
        });
        crate::agent::save_config(&cfg);
    }

    pub fn draw(&self, font: &FontKit, buf: &mut [u32], buf_w: usize, viewport: (i32, i32)) {
        let (x, y, w, h) = self.rect;
        fill_rect(buf, buf_w, viewport, (x, y, w, h), PANEL);
        stroke_rect(buf, buf_w, viewport, (x, y, w, h), BORDER);

        font.draw_text(buf, buf_w, viewport, x + PAD, y + 6, "LLM 设置（保存后新对话生效）", TEXT);

        let row_h = 26 + 18;
        for i in 0..4 {
            let ry = y + 30 + row_h * i as i32;
            font.draw_text(buf, buf_w, viewport, x + PAD, ry, self.labels[i], LABEL);
            let iy = ry + 16;
            let focused = self.focus == i;
            fill_rect(
                buf,
                buf_w,
                viewport,
                (x + PAD, iy, (w as i32 - PAD * 2) as u32, 26),
                if focused { FOCUS_BG } else { INPUT_BG },
            );
            stroke_rect(
                buf,
                buf_w,
                viewport,
                (x + PAD, iy, (w as i32 - PAD * 2) as u32, 26),
                if focused { CURSOR } else { BORDER },
            );
            // 显示（api_key 掩码，首尾各留 4 位）
            let raw = &self.fields[i];
            let chars: Vec<char> = raw.chars().collect();
            let display: String = if i == 2 && chars.len() > 12 {
                let head: String = chars[..4].iter().collect();
                let tail: String = chars[chars.len() - 4..].iter().collect();
                format!("{head}****{tail}")
            } else {
                raw.clone()
            };
            let max_w = w as i32 - PAD * 2 - 12;
            let mut d: &str = &display;
            while font.text_width(d).round() as i32 > max_w && !d.is_empty() {
                d = crate::ui::trim_first(d);
            }
            font.draw_text(buf, buf_w, viewport, x + PAD + 5, iy + 6, d, TEXT);
            if focused && self.cursor_on {
                let cw = font.text_width(d).round() as i32;
                fill_rect(buf, buf_w, viewport, (x + PAD + 5 + cw, iy + 4, 2, 18), CURSOR);
            }
        }

        font.draw_text(
            buf,
            buf_w,
            viewport,
            x + PAD,
            y + h as i32 - 20,
            "↑↓/Tab 切换 · Enter 保存 · Esc 取消",
            LABEL,
        );
    }
}
