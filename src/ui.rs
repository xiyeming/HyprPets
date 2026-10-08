//! 像素风 IMGUI：系统字体文字渲染（fontdb + fontdue）+ 右键换肤菜单。
//!
//! 所有颜色均为预乘 ARGB（渲染管道约定）。

use std::path::PathBuf;

use crate::sprite::Sheet;

/// 预乘颜色构造
pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> u32 {
    Sheet::premul(a, r, g, b)
}

pub const PANEL: u32 = rgba(24, 26, 36, 225);
pub const PANEL_BORDER: u32 = rgba(150, 155, 180, 255);
pub const ROW_HOVER: u32 = rgba(70, 92, 150, 190);
pub const TEXT: u32 = rgba(238, 238, 248, 255);
pub const TEXT_DIM: u32 = rgba(160, 165, 185, 255);

// ---------- 文字 ----------

pub struct FontKit {
    font: fontdue::Font,
    px: f32,
}

impl FontKit {
    /// 扫描系统字体，优先中文无衬线
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
            // 空格等零宽字形：只前进画笔
            if metrics.width == 0 {
                pen_x += metrics.advance_width;
                continue;
            }
            let gx = pen_x.round() as i32 + metrics.xmin;
            // fontdue bitmap 首行是字形最上行；ymin 是基线到下缘距离
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

pub struct MenuEntry {
    pub name: String,
    pub dir: PathBuf,
    pub preview: Sheet,
    pub preview_idx: usize,
}

pub struct Menu {
    pub entries: Vec<MenuEntry>,
    pub hover: Option<usize>,
    /// (x, y, w, h) 全局坐标
    pub rect: (i32, i32, u32, u32),
    pub font: FontKit,
}

const ITEM_PAD: i32 = 6;
const PREVIEW_SCALE: u32 = 2;
const BORDER: i32 = 2;

impl Menu {
    /// 布局菜单：anchor = 宠物左上角（全局），bbox = 所在布局边界
    pub fn layout(entries: Vec<MenuEntry>, anchor: (i32, i32), pet_h: i32, bbox: (i32, i32, i32, i32), font: FontKit) -> Menu {
        let preview_h = entries
            .first()
            .map(|e| e.preview.frame_h as i32 * PREVIEW_SCALE as i32)
            .unwrap_or(0);
        let text_h = font.px as i32;
        let item_h = preview_h.max(text_h).max(8) + ITEM_PAD * 2;
        let width = entries
            .iter()
            .map(|e| {
                font.text_width(&e.name).round() as i32
                    + e.preview.frame_w as i32 * PREVIEW_SCALE as i32
                    + 16
            })
            .max()
            .unwrap_or(60)
            + ITEM_PAD * 2
            + BORDER * 2;
        let height = item_h * entries.len().max(1) as i32 + BORDER * 2;

        // 优先在宠物上方弹出，放不下则下方；水平方向钳制在布局内
        let mut y = anchor.1 - height;
        if y < bbox.1 {
            y = anchor.1 + pet_h;
        }
        y = y.clamp(bbox.1, (bbox.3 - height).max(bbox.1));
        let x = anchor.0.clamp(bbox.0, (bbox.2 - width).max(bbox.0));

        Menu { entries, hover: None, rect: (x, y, width as u32, height as u32), font }
    }

    pub fn hit(&self, global: (f64, f64)) -> Option<usize> {
        let (x, y, w, h) = self.rect;
        let gx = global.0 as i32;
        let gy = global.1 as i32;
        if gx < x || gx >= x + w as i32 || gy < y || gy >= y + h as i32 {
            return None;
        }
        let item_h = (h as i32 - BORDER * 2) / self.entries.len().max(1) as i32;
        let idx = ((gy - y - BORDER) / item_h) as usize;
        (idx < self.entries.len()).then_some(idx)
    }

    pub fn draw(&self, buf: &mut [u32], buf_w: usize, viewport: (i32, i32)) {
        let (mx, my, mw, mh) = self.rect;
        // 面板 + 边框
        fill_rect(buf, buf_w, viewport, (mx, my, mw, mh), PANEL);
        stroke_rect(buf, buf_w, viewport, (mx, my, mw, mh), PANEL_BORDER);

        let item_h = (mh as i32 - BORDER * 2) / self.entries.len().max(1) as i32;
        for (i, entry) in self.entries.iter().enumerate() {
            let ry = my + BORDER + item_h * i as i32;
            if self.hover == Some(i) {
                fill_rect(
                    buf,
                    buf_w,
                    viewport,
                    (mx + BORDER, ry, (mw as i32 - BORDER * 2) as u32, item_h as u32),
                    ROW_HOVER,
                );
            }
            // 预览图
            let frame = entry.preview.frame(entry.preview_idx);
            entry.preview.blit_frame(
                buf,
                buf_w,
                viewport,
                frame,
                (mx + BORDER + ITEM_PAD, ry + ITEM_PAD),
                PREVIEW_SCALE,
                255,
            );
            // 名称
            let text_x = mx + BORDER + ITEM_PAD + entry.preview.frame_w as i32 * PREVIEW_SCALE as i32 + 8;
            let text_y = ry + (item_h - self.font.px as i32) / 2;
            self.font
                .draw_text(buf, buf_w, viewport, text_x, text_y, &entry.name, TEXT);
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
    /// 剩余时间 < 0.5s 时不透明度渐出
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
    pet: (i32, i32, i32, i32), // x, y, w, h（全局）
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

    // 面板 + 边框 + 尾巴
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

    font.draw_text(
        buf,
        buf_w,
        viewport,
        bx + pad + 1,
        by + (bh - font.px as i32) / 2,
        &text,
        BUBBLE_TEXT,
    );
}
