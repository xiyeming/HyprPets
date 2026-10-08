//! Sprite 图集（多帧动画表）与像素合成。
//!
//! 像素格式为预乘 ARGB（0xAARRGGBB），与 wl_shm Argb8888 小端布局一致。
//! 帧源支持两种：
//! - 网格表：`frames` 按行优先索引（col,row → idx = row*cols + col），帧尺寸一致
//! - Aseprite 导出：每帧独立矩形（`Sheet::frames` 为任意矩形列表）
//!
//! 合成时逐像素 nearest 放大，双向裁剪（宠物可部分在屏外/跨屏）。

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

/// 帧矩形（像素，整张图内）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameRect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

pub struct Sheet {
    pub width: u32,
    pub height: u32,
    /// 预乘 ARGB 像素
    pixels: Vec<u32>,
    /// 帧矩形列表（网格格式为所有单元格，Aseprite 为导出帧）
    pub frames: Vec<FrameRect>,
    /// 网格列数（Aseprite 格式为 0）
    pub cols: u32,
    pub frame_w: u32,
    pub frame_h: u32,
}

impl Sheet {
    /// 网格表：整图 + 帧尺寸，frames 按行优先生成
    pub fn grid(pixels: Vec<u32>, width: u32, height: u32, frame_w: u32, frame_h: u32) -> Sheet {
        let cols = width / frame_w;
        let rows = height / frame_h;
        let mut frames = Vec::with_capacity((cols * rows) as usize);
        for row in 0..rows {
            for col in 0..cols {
                frames.push(FrameRect { x: col * frame_w, y: row * frame_h, w: frame_w, h: frame_h });
            }
        }
        Sheet { width, height, pixels, frames, cols, frame_w, frame_h }
    }

    /// Aseprite 表：整图 + 显式帧矩形
    pub fn aseprite(pixels: Vec<u32>, width: u32, height: u32, frames: Vec<FrameRect>) -> Sheet {
        let frame_w = frames.first().map(|f| f.w).unwrap_or(width);
        let frame_h = frames.first().map(|f| f.h).unwrap_or(height);
        Sheet { width, height, pixels, frames, cols: 0, frame_w, frame_h }
    }

    /// 从 PNG 加载整图像素（预乘）
    pub fn load_pixels(path: &Path) -> Result<(Vec<u32>, u32, u32)> {
        let img = image::open(path)
            .with_context(|| format!("无法读取贴图 {}", path.display()))?
            .to_rgba8();
        let (w, h) = img.dimensions();
        let pixels = img
            .pixels()
            .map(|p| {
                let [r, g, b, a] = p.0;
                let r = ((r as u32 * a as u32) / 255) as u8;
                let g = ((g as u32 * a as u32) / 255) as u8;
                let b = ((b as u32 * a as u32) / 255) as u8;
                ((a as u32) << 24) | ((r as u32) << 16) | ((g as u32) << 8) | b as u32
            })
            .collect();
        Ok((pixels, w, h))
    }

    /// 调试访问（测试/诊断用）
    pub fn pixels_probe(&self) -> &[u32] {
        &self.pixels
    }

    pub fn frame(&self, idx: usize) -> FrameRect {
        self.frames.get(idx).copied().unwrap_or(FrameRect { x: 0, y: 0, w: self.frame_w, h: self.frame_h })
    }

    /// 把某一帧以 nearest 采样绘制到某个显示器的 buffer 上。
    ///
    /// `viewport` 是该 surface 在全局布局中的原点，`dst` 是帧左上角全局坐标。
    /// `alpha` 为整体不透明度（255 = 不透明；穿透 102、全屏避让 64）。
    #[allow(clippy::too_many_arguments)]
    pub fn blit_frame(
        &self,
        buf: &mut [u32],
        buf_w: usize,
        viewport: (i32, i32),
        frame: FrameRect,
        dst: (i32, i32),
        scale: u32,
        alpha: u8,
    ) {
        let scale = scale as i32;
        if scale <= 0 {
            return;
        }
        let fw = frame.w as i32;
        let fh = frame.h as i32;
        let buf_h = (buf.len() / buf_w) as i32;

        // 帧在 buffer 局部坐标中的位置
        let px = dst.0 - viewport.0;
        let py = dst.1 - viewport.1;

        // 与 buffer 的可见交集
        let vx0 = px.max(0);
        let vy0 = py.max(0);
        let vx1 = (px + fw * scale).min(buf_w as i32);
        let vy1 = (py + fh * scale).min(buf_h);
        if vx0 >= vx1 || vy0 >= vy1 {
            return;
        }

        // 映射回帧内像素坐标（起点向上对齐 scale 边界，终点向下对齐）
        let sx0 = (vx0 - px + scale - 1) / scale;
        let sy0 = (vy0 - py + scale - 1) / scale;
        let sx1 = ((vx1 - px) / scale).min(fw);
        let sy1 = ((vy1 - py) / scale).min(fh);

        for sy in sy0..sy1 {
            for sx in sx0..sx1 {
                let mut c = self.pixels[(frame.y as i32 + sy) as usize * self.width as usize
                    + (frame.x as i32 + sx) as usize];
                let a = (c >> 24) & 0xff;
                if a == 0 {
                    continue;
                }
                if alpha < 255 {
                    let a = alpha as u32;
                    c = (((c >> 24) & 0xff) * a / 255) << 24
                        | ((((c >> 16) & 0xff) * a / 255) << 16)
                        | ((((c >> 8) & 0xff) * a / 255) << 8)
                        | (((c & 0xff) * a / 255));
                }
                let bx = (px + sx * scale) as usize;
                let by = (py + sy * scale) as usize;
                for dy in 0..scale as usize {
                    let row = (by + dy) * buf_w;
                    for dx in 0..scale as usize {
                        buf[row + bx + dx] = c;
                    }
                }
            }
        }
    }

    /// 保存为 PNG（un-premultiply）。
    pub fn save_png(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).ok();
        }
        let mut img = image::RgbaImage::new(self.width, self.height);
        for (i, p) in self.pixels.iter().enumerate() {
            // 必须先提升到 u32 再做乘法，u8*255 会溢出回绕
            let a = (p >> 24) as u8;
            let unpremul = |v: u32| -> u8 {
                if a > 0 { ((v * 255) / a as u32) as u8 } else { 0 }
            };
            let r = unpremul((p >> 16) & 0xff);
            let g = unpremul((p >> 8) & 0xff);
            let b = unpremul(p & 0xff);
            img.put_pixel(i as u32 % self.width, i as u32 / self.width, image::Rgba([r, g, b, a]));
        }
        img.save(path).with_context(|| format!("无法写入 {}", path.display()))?;
        Ok(())
    }

    /// 预乘 ARGB 颜色（const 构造用）
    pub const fn premul(a: u8, r: u8, g: u8, b: u8) -> u32 {
        ((a as u32) << 24) | (((r as u32 * a as u32 / 255) as u32) << 16)
            | (((g as u32 * a as u32 / 255) as u32) << 8)
            | ((b as u32 * a as u32 / 255) as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 4x4 图，2x2 帧 → 4 帧
    #[test]
    fn grid_frames_layout() {
        let sheet = Sheet::grid(vec![0; 16], 4, 4, 2, 2);
        assert_eq!(sheet.frames.len(), 4);
        assert_eq!(sheet.frame(3), FrameRect { x: 2, y: 2, w: 2, h: 2 });
    }

    /// PNG 往返必须保持颜色（回归：u8*255 溢出导致全黑）
    #[test]
    fn png_roundtrip_preserves_color() {
        let dir = std::env::temp_dir().join("hyprpet_sheet_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sheet.png");
        let px = vec![Sheet::premul(255, 245, 164, 75); 8 * 8];
        let sheet = Sheet::grid(px, 8, 8, 4, 4);
        sheet.save_png(&path).unwrap();
        let (loaded, w, h) = Sheet::load_pixels(&path).unwrap();
        assert_eq!((w, h), (8, 8));
        assert_eq!(loaded[0], 0xFFF5A44B);
    }
}
