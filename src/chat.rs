//! 像素聊天对话框：双击宠物唤起，键盘独占输入。
//!
//! 布局：历史消息气泡（用户右 / 宠物左）+ 底部输入行 + 光标闪烁。
//! 纯像素绘制，复用 ui 原语。

use crate::ui::{fill_rect, rgba, stroke_rect, FontKit};
use crate::ui::{self};

const CHAT_W: u32 = 380;
const CHAT_H: u32 = 240;
const PAD: i32 = 8;

const PANEL: u32 = rgba(22, 24, 34, 235);
const BORDER: u32 = rgba(140, 145, 175, 255);
const USER_BG: u32 = rgba(84, 110, 180, 230);
const PET_BG: u32 = rgba(248, 250, 255, 240);
const PET_TEXT: u32 = rgba(35, 38, 52, 255);
const INPUT_BG: u32 = rgba(12, 13, 20, 255);
const TEXT: u32 = rgba(235, 236, 248, 255);
const CURSOR: u32 = rgba(255, 210, 90, 255);

#[derive(Clone, Copy, PartialEq)]
pub enum Who {
    User,
    Pet,
}

pub struct Chat {
    pub rect: (i32, i32, u32, u32),
    pub history: Vec<(Who, String)>,
    pub input: String,
    /// IME 预编辑文本（未提交，仅展示）
    pub preedit: String,
    pub busy: bool,
    cursor_on: bool,
}

impl Chat {
    /// 计算对话框位置：优先宠物上方，空间不足则下方（钳制在显示器内）
    pub fn layout_rect(
        anchor: (i32, i32),
        pet_w: i32,
        pet_h: i32,
        bbox: (i32, i32, i32, i32),
    ) -> (i32, i32, u32, u32) {
        let mut x = anchor.0 + pet_w / 2 - CHAT_W as i32 / 2;
        x = x.clamp(bbox.0, (bbox.2 - CHAT_W as i32).max(bbox.0));
        let mut y = anchor.1 - CHAT_H as i32 - 12;
        if y < bbox.1 {
            y = anchor.1 + pet_h + 12;
        }
        y = y.clamp(bbox.1, (bbox.3 - CHAT_H as i32).max(bbox.1));
        (x, y, CHAT_W, CHAT_H)
    }

    pub fn new(anchor: (i32, i32), pet_w: i32, pet_h: i32, bbox: (i32, i32, i32, i32)) -> Chat {
        Chat {
            rect: Self::layout_rect(anchor, pet_w, pet_h, bbox),
            history: Vec::new(),
            input: String::new(),
            preedit: String::new(),
            busy: false,
            cursor_on: true,
        }
    }

    /// 输入光标的全局矩形（供 IME set_cursor_rectangle 定位候选框）
    pub fn cursor_global(&self, font: &FontKit) -> (i32, i32, i32, i32) {
        let (x, y, w, h) = self.rect;
        let input_h = 26;
        let iy = y + h as i32 - input_h - PAD;
        let text = if self.preedit.is_empty() {
            self.input.clone()
        } else {
            format!("{}{}", self.input, self.preedit)
        };
        let text_max = w as i32 - PAD * 2 - 14;
        let mut display: &str = &text;
        while font.text_width(display).round() as i32 > text_max && !display.is_empty() {
            display = ui::trim_first(display);
        }
        let tw = font.text_width(display).round() as i32;
        (x + PAD + 6 + tw, iy, 2, font.px() as i32)
    }

    /// 跟随宠物移动更新位置（保留内容与输入）
    pub fn follow(&mut self, anchor: (i32, i32), pet_w: i32, pet_h: i32, bbox: (i32, i32, i32, i32)) {
        self.rect = Self::layout_rect(anchor, pet_w, pet_h, bbox);
    }

    pub fn push(&mut self, who: Who, text: &str) {
        // 保留最近 30 条
        self.history.push((who, text.to_string()));
        if self.history.len() > 30 {
            self.history.drain(..self.history.len() - 30);
        }
    }

    pub fn toggle_cursor(&mut self) {
        self.cursor_on = !self.cursor_on;
    }

    fn wrap(font: &FontKit, text: &str, max_w: f32) -> Vec<String> {
        let mut lines = Vec::new();
        let mut cur = String::new();
        for ch in text.chars() {
            if ch == '\n' {
                lines.push(std::mem::take(&mut cur));
                continue;
            }
            let w = font.text_width(&(cur.clone() + ch.to_string().as_str()));
            if w > max_w && !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
            }
            cur.push(ch);
        }
        if !cur.is_empty() {
            lines.push(cur);
        }
        lines
    }

    pub fn draw(&self, font: &FontKit, buf: &mut [u32], buf_w: usize, viewport: (i32, i32)) {
        let (x, y, w, h) = self.rect;
        fill_rect(buf, buf_w, viewport, (x, y, w, h), PANEL);
        stroke_rect(buf, buf_w, viewport, (x, y, w, h), BORDER);

        let input_h = 26;
        let inner_w = w as i32 - PAD * 2;

        // ---- 历史区（自下而上布局）----
        let hist_bottom = y + h as i32 - input_h - PAD;
        let mut pen_y = hist_bottom;
        let line_h = font.px() as i32 + 6;
        for (who, text) in self.history.iter().rev() {
            let max_text_w = inner_w as f32 - 24.0;
            let lines = Self::wrap(font, text, max_text_w);
            let block_h = line_h * lines.len() as i32 + 8;
            pen_y -= block_h + 4;
            if pen_y < y + PAD {
                break;
            }
            let bubble_w = lines
                .iter()
                .map(|l| font.text_width(l).round() as i32 + 14)
                .max()
                .unwrap_or(40)
                .min(inner_w);
            let bx = match who {
                Who::User => x + w as i32 - PAD - bubble_w,
                Who::Pet => x + PAD,
            };
            let bg = match who {
                Who::User => USER_BG,
                Who::Pet => PET_BG,
            };
            let fg = match who {
                Who::User => TEXT,
                Who::Pet => PET_TEXT,
            };
            fill_rect(buf, buf_w, viewport, (bx, pen_y, bubble_w as u32, block_h as u32), bg);
            for (i, line) in lines.iter().enumerate() {
                font.draw_text(buf, buf_w, viewport, bx + 7, pen_y + 4 + i as i32 * line_h, line, fg);
            }
        }

        // busy 指示
        if self.busy && self.history.last().map(|h| h.0) != Some(Who::Pet) {
            let dots = ["…", "… …", "… … …"][(std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| (d.as_millis() / 400 % 3) as usize)
                .unwrap_or(0))];
            font.draw_text(buf, buf_w, viewport, x + PAD, pen_y - line_h - 2, dots, TEXT);
        }

        // ---- 输入行 ----
        let iy = y + h as i32 - input_h - PAD;
        fill_rect(buf, buf_w, viewport, (x + PAD, iy, inner_w as u32, input_h as u32), INPUT_BG);
        stroke_rect(buf, buf_w, viewport, (x + PAD, iy, inner_w as u32, input_h as u32), BORDER);

        // 输入 + IME 预编辑（预编辑用暗色表示未提交）
        let shown = if self.preedit.is_empty() {
            self.input.clone()
        } else {
            format!("{}{}", self.input, self.preedit)
        };
        let text_max = inner_w - 14;
        let mut display: &str = &shown;
        while font.text_width(display).round() as i32 > text_max && !display.is_empty() {
            display = ui::trim_first(display);
        }
        let input_len = self.input.chars().count().min(display.chars().count());
        let (disp_input, disp_pre) = display.split_at(display.char_indices().nth(input_len).map(|(i, _)| i).unwrap_or(display.len()));
        font.draw_text(buf, buf_w, viewport, x + PAD + 6, iy + 6, disp_input, TEXT);
        let iw = font.text_width(disp_input).round() as i32;
        font.draw_text(buf, buf_w, viewport, x + PAD + 6 + iw, iy + 6, disp_pre, ui::TEXT_DIM);
        if self.cursor_on {
            let cw = font.text_width(display).round() as i32;
            fill_rect(buf, buf_w, viewport, (x + PAD + 6 + cw, iy + 5, 2, (input_h - 10) as u32), CURSOR);
        }

        // 提示
        font.draw_text(
            buf,
            buf_w,
            viewport,
            x + PAD,
            y + 4,
            "Enter 发送 · Esc 关闭",
            ui::TEXT_DIM,
        );
    }
}
