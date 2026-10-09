//! 像素风 IMGUI：右键配置菜单 + 系统字体文字渲染。
//!
//! 菜单项：换肤 / 动作（可勾选）/ 分隔线。托盘与右键菜单共用 MenuCmd。
//! 所有颜色均为预乘 ARGB（渲染管道约定）。

use std::path::PathBuf;

use crate::sprite::Sheet;

/// 菜单/托盘共用的配置命令
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuCmd {
    ToggleMini,
    TogglePassthrough,
    CycleIdle,
    OpenChat,
    OpenSettings,
    HideEdge,
    Quit,
}

/// 预乘颜色构造
pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> u32 {
    Sheet::premul(a, r, g, b)
}

pub const PANEL: u32 = rgba(24, 26, 36, 225);
pub const PANEL_BORDER: u32 = rgba(150, 155, 180, 255);
pub const ROW_HOVER: u32 = rgba(70, 92, 150, 190);
pub const TEXT: u32 = rgba(238, 238, 248, 255);
pub const TEXT_DIM: u32 = rgba(160, 165, 185, 255);
pub const CHECK_ON: u32 = rgba(120, 230, 140, 255);
pub const CHECK_BOX: u32 = rgba(200, 205, 225, 255);

// ---------- 文字 ----------

pub struct FontKit {
    font: fontdue::Font,
    px: f32,
}

impl FontKit {
    pub fn load() -> Option<FontKit> {
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        let query = fontdb::Query {
            families: &[
                fontdb::Family::Name("Noto Sans CJK SC"),
                fontdb::Family::Name("Noto Sans SC"),
                fontdb::Family::Name("WenQuanYi Micro Hei"),
                fontdb::Family::Name("Microsoft YaHei"),
                fontdb::Family::SansSerif,
            ],
            ..Default::default()
        };
        let id = db.query(&query)?;
        db.with_face_data(id, |data, _| {
            fontdue::Font::from_bytes(data, fontdue::FontSettings::default())
                .ok()
                .map(|font| FontKit { font, px: 14.0 })
        })?
    }

    pub fn px(&self) -> f32 {
        self.px
    }

    pub fn text_width(&self, s: &str) -> f32 {
        s.chars()
            .map(|c| self.font.rasterize(c, self.px).0.advance_width)
            .sum()
    }

    /// 在 (x, y_top)（全局坐标）绘制文字
    pub fn draw_text(
        &self,
        buf: &mut [u32],
        buf_w: usize,
        viewport: (i32, i32),
        x: i32,
        y_top: i32,
        s: &str,
        color: u32,
    ) {
        let mut pen_x = x as f32;
        let buf_h = (buf.len() / buf_w) as i32;
        for ch in s.chars() {
            let (metrics, coverage) = self.font.rasterize(ch, self.px);
            if metrics.width == 0 {
                pen_x += metrics.advance_width;
                continue;
            }
            let gx = pen_x.round() as i32 + metrics.xmin;
            let gy_top = y_top + self.px as i32 - metrics.ymin - metrics.height as i32;
            for (j, row) in coverage.chunks(metrics.width).enumerate() {
                let by = gy_top + j as i32 - viewport.1;
                if by < 0 || by >= buf_h {
                    continue;
                }
                for (i, &cov) in row.iter().enumerate() {
                    if cov == 0 {
                        continue;
                    }
                    let bx = gx + i as i32 - viewport.0;
                    if bx < 0 || bx >= buf_w as i32 {
                        continue;
                    }
                    blend(buf, by as usize * buf_w + bx as usize, color, cov as u32);
                }
            }
            pen_x += metrics.advance_width;
        }
    }
}

/// 预乘 src over 混合：cov ∈ 0..=255
fn blend(buf: &mut [u32], idx: usize, src: u32, cov: u32) {
    let inv = 255 - cov;
    let dst = buf[idx];
    let ch = |shift: u32| -> u32 {
        let s = (src >> shift) & 0xff;
        let d = (dst >> shift) & 0xff;
        (s * cov + d * inv) / 255
    };
    buf[idx] = (ch(24) << 24) | (ch(16) << 16) | (ch(8) << 8) | ch(0);
}

// ---------- 菜单 ----------

pub enum MenuItem {
    /// 换肤项（带预览图）
    Skin {
        name: String,
        dir: PathBuf,
        preview: Sheet,
        preview_idx: usize,
    },
    /// 配置动作（checked = Some 时渲染勾选框）
    Action {
        label: String,
        checked: Option<bool>,
        cmd: MenuCmd,
    },
    Separator,
}

pub struct Menu {
    pub items: Vec<MenuItem>,
    pub hover: Option<usize>,
    /// (x, y, w, h) 全局坐标
    pub rect: (i32, i32, u32, u32),
    /// 每个条目的行高（layout 时计算，hit/draw 共用）
    row_heights: Vec<i32>,
    pub font: FontKit,
}

const ITEM_PAD: i32 = 6;
const PREVIEW_SCALE: u32 = 1;
const BORDER: i32 = 2;

impl Menu {
    pub fn layout(
        items: Vec<MenuItem>,
        anchor: (i32, i32),
        pet_h: i32,
        bbox: (i32, i32, i32, i32),
        font: FontKit,
    ) -> Menu {
        let text_h = font.px() as i32;
        // 行高按条目类型自适应：换肤行含预览，动作行仅文字，分隔线细条
        let row_heights: Vec<i32> = items
            .iter()
            .map(|it| match it {
                MenuItem::Skin { preview, .. } => {
                    (preview.frame_h as i32 * PREVIEW_SCALE as i32)
                        .max(text_h)
                        .max(10)
                        + ITEM_PAD * 2
                }
                MenuItem::Action { .. } => (text_h.max(14)) + ITEM_PAD * 2,
                MenuItem::Separator => 9,
            })
            .collect();
        let width = items
            .iter()
            .map(|it| match it {
                MenuItem::Skin { name, preview, .. } => {
                    font.text_width(name).round() as i32
                        + preview.frame_w as i32 * PREVIEW_SCALE as i32
                        + 16
                }
                MenuItem::Action { label, checked, .. } => {
                    font.text_width(label).round() as i32 + if checked.is_some() { 22 } else { 8 }
                }
                MenuItem::Separator => 40,
            })
            .max()
            .unwrap_or(60)
            + ITEM_PAD * 2
            + BORDER * 2;
        let height: i32 = row_heights.iter().sum::<i32>() + BORDER * 2;

        let mut y = anchor.1 - height;
        if y < bbox.1 {
            y = anchor.1 + pet_h;
        }
        y = y.clamp(bbox.1, (bbox.3 - height).max(bbox.1));
        let x = anchor.0.clamp(bbox.0, (bbox.2 - width).max(bbox.0));

        Menu { items, hover: None, rect: (x, y, width as u32, height as u32), row_heights, font }
    }

    /// 被点击的条目索引（分隔线不可点击）
    pub fn hit(&self, global: (f64, f64)) -> Option<usize> {
        let (x, y, w, h) = self.rect;
        let gx = global.0 as i32;
        let gy = global.1 as i32;
        if gx < x || gx >= x + w as i32 || gy < y || gy >= y + h as i32 {
            return None;
        }
        let mut y0 = y + BORDER;
        for (i, rh) in self.row_heights.iter().enumerate() {
            if gy >= y0 && gy < y0 + rh {
                return match self.items.get(i) {
                    Some(MenuItem::Separator) | None => None,
                    _ => Some(i),
                };
            }
            y0 += rh;
        }
        None
    }

    pub fn draw(&self, buf: &mut [u32], buf_w: usize, viewport: (i32, i32)) {
        let (mx, my, mw, mh) = self.rect;
        fill_rect(buf, buf_w, viewport, (mx, my, mw, mh), PANEL);
        stroke_rect(buf, buf_w, viewport, (mx, my, mw, mh), PANEL_BORDER);

        let mut ry = my + BORDER;
        for (i, item) in self.items.iter().enumerate() {
            let item_h = self.row_heights[i];
            match item {
                MenuItem::Separator => {
                    fill_rect(
                        buf,
                        buf_w,
                        viewport,
                        (mx + BORDER + 4, ry + item_h / 2, (mw as i32 - BORDER * 2 - 8) as u32, 1),
                        PANEL_BORDER,
                    );
                    ry += item_h;
                    continue;
                }
                _ => {}
            }
            if self.hover == Some(i) {
                fill_rect(
                    buf,
                    buf_w,
                    viewport,
                    (mx + BORDER, ry, (mw as i32 - BORDER * 2) as u32, item_h as u32),
                    ROW_HOVER,
                );
            }
            match item {
                MenuItem::Skin { name, preview, preview_idx, .. } => {
                    let frame = preview.frame(*preview_idx);
                    preview.blit_frame(
                        buf,
                        buf_w,
                        viewport,
                        frame,
                        (mx + BORDER + ITEM_PAD, ry + ITEM_PAD),
                        PREVIEW_SCALE,
                        255,
                        false,
                    );
                    let text_x =
                        mx + BORDER + ITEM_PAD + preview.frame_w as i32 * PREVIEW_SCALE as i32 + 8;
                    let text_y = ry + (item_h - self.font.px() as i32) / 2;
                    self.font.draw_text(buf, buf_w, viewport, text_x, text_y, name, TEXT);
                    ry += item_h;
                }
                MenuItem::Action { label, checked, .. } => {
                    let mut text_x = mx + BORDER + ITEM_PAD;
                    if let Some(on) = checked {
                        // 勾选框 10x10
                        let cy = ry + (item_h - 10) / 2;
                        stroke_rect(buf, buf_w, viewport, (text_x, cy, 10, 10), CHECK_BOX);
                        if *on {
                            fill_rect(buf, buf_w, viewport, (text_x + 2, cy + 2, 6, 6), CHECK_ON);
                        }
                        text_x += 18;
                    }
                    let text_y = ry + (item_h - self.font.px() as i32) / 2;
                    self.font.draw_text(buf, buf_w, viewport, text_x, text_y, label, TEXT);
                    ry += item_h;
                }
                MenuItem::Separator => {
                    ry += item_h;
                }
            }
        }
    }
}

// ---------- 绘制原语 ----------

pub fn fill_rect(buf: &mut [u32], buf_w: usize, viewport: (i32, i32), rect: (i32, i32, u32, u32), color: u32) {
    let (x, y, w, h) = rect;
    let buf_h = (buf.len() / buf_w) as i32;
    let x0 = (x - viewport.0).max(0);
    let y0 = (y - viewport.1).max(0);
    let x1 = (x - viewport.0 + w as i32).min(buf_w as i32);
    let y1 = (y - viewport.1 + h as i32).min(buf_h);
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    for row in y0..y1 {
        let start = row as usize * buf_w + x0 as usize;
        buf[start..(x1 - x0) as usize + start].fill(color);
    }
}

pub fn stroke_rect(buf: &mut [u32], buf_w: usize, viewport: (i32, i32), rect: (i32, i32, u32, u32), color: u32) {
    let (x, y, w, h) = rect;
    fill_rect(buf, buf_w, viewport, (x, y, w, 1), color);
    fill_rect(buf, buf_w, viewport, (x, y + h as i32 - 1, w, 1), color);
    fill_rect(buf, buf_w, viewport, (x, y, 1, h), color);
    fill_rect(buf, buf_w, viewport, (x + w as i32 - 1, y, 1, h), color);
}

// ---------- 通知气泡 ----------

pub struct Bubble {
    pub text: String,
    pub until: std::time::Instant,
}

impl Bubble {
    pub fn alpha(&self) -> u8 {
        let remain = self.until.saturating_duration_since(std::time::Instant::now());
        let ms = remain.as_millis() as u64;
        if ms > 500 {
            255
        } else {
            (ms * 255 / 500) as u8
        }
    }
}

const BUBBLE_BG: u32 = rgba(250, 252, 255, 242);
const BUBBLE_BORDER: u32 = rgba(90, 95, 120, 255);
const BUBBLE_TEXT: u32 = rgba(35, 38, 52, 255);

/// 在宠物头顶绘制气泡（空间不足时放脚下）
pub fn draw_bubble(
    bubble: &Bubble,
    font: &FontKit,
    buf: &mut [u32],
    buf_w: usize,
    viewport: (i32, i32),
    pet: (i32, i32, i32, i32),
    monitor: (i32, i32, i32, i32),
) {
    let text: String = bubble.text.chars().take(24).collect();
    let text_w = font.text_width(&text).round() as i32;
    let pad = 8;
    let bw = text_w + pad * 2 + 2;
    let bh = font.px as i32 + pad + 4;

    let (px, py, pw, ph) = pet;
    let bx = (px + pw / 2 - bw / 2).clamp(monitor.0, monitor.2 - bw);
    let above = py - bh - 10 >= monitor.1;
    let by = if above { py - bh - 10 } else { py + ph + 10 };

    fill_rect(buf, buf_w, viewport, (bx, by, bw as u32, bh as u32), BUBBLE_BG);
    stroke_rect(buf, buf_w, viewport, (bx, by, bw as u32, bh as u32), BUBBLE_BORDER);
    let tail_x = (px + pw / 2).clamp(bx + 6, bx + bw - 10);
    if above {
        fill_rect(buf, buf_w, viewport, (tail_x, by + bh, 4, 3), BUBBLE_BG);
        fill_rect(buf, buf_w, viewport, (tail_x + 1, by + bh + 3, 2, 2), BUBBLE_BG);
    } else {
        fill_rect(buf, buf_w, viewport, (tail_x, by - 3, 4, 3), BUBBLE_BG);
        fill_rect(buf, buf_w, viewport, (tail_x + 1, by - 5, 2, 2), BUBBLE_BG);
    }

    font.draw_text(buf, buf_w, viewport, bx + pad + 1, by + (bh - font.px as i32) / 2, &text, BUBBLE_TEXT);
}
