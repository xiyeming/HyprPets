//! Sprite 加载与像素合成。
//!
//! 像素格式为预乘 ARGB（0xAARRGGBB），与 softbuffer / wl_shm ARGB8888 一致。
//! 加载时做预乘，合成时逐像素 nearest 放大。

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::pet::Pet;

pub struct Sprite {
    pub width: u32,
    pub height: u32,
    /// 预乘 ARGB 像素
    pixels: Vec<u32>,
}

impl Sprite {
    /// 从 PNG 加载；文件不存在时生成一只默认像素小猫并落盘。
    pub fn load_or_default(path: &Path) -> Result<Sprite> {
        if path.exists() {
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
            return Ok(Sprite { width: w, height: h, pixels });
        }

        let sprite = default_cat();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).ok();
        }
        let mut img = image::RgbaImage::new(sprite.width, sprite.height);
        for (i, p) in sprite.pixels.iter().enumerate() {
            // 注意：必须先提升到 u32 再做乘法，u8*255 会溢出回绕
            let a = (p >> 24) as u8;
            let unpremul = |v: u32| -> u8 {
                if a > 0 { ((v * 255) / a as u32) as u8 } else { 0 }
            };
            let r = unpremul((p >> 16) & 0xff);
            let g = unpremul((p >> 8) & 0xff);
            let b = unpremul(p & 0xff);
            img.put_pixel(i as u32 % sprite.width, i as u32 / sprite.width, image::Rgba([r, g, b, a]));
        }
        img.save(path).with_context(|| format!("无法写入默认贴图 {}", path.display()))?;
        Ok(sprite)
    }

    /// 把 sprite 以 nearest 采样绘制到某个显示器的 buffer 上。
    ///
    /// `viewport` 是该 surface 在全局布局中的原点，宠物坐标为全局坐标，
    /// 可能整体或部分位于本 surface 之外（双向裁剪）。
    /// 穿透模式下整体降到 40% 不透明度作为视觉反馈。
    pub fn blit(&self, buf: &mut [u32], buf_w: usize, viewport: (i32, i32), pet: &Pet) {
        let scale = pet.scale as i32;
        let pw = self.width as i32 * scale;
        let ph = self.height as i32 * scale;
        let buf_h = (buf.len() / buf_w) as i32;

        // 宠物在 buffer 局部坐标中的位置
        let px = pet.x - viewport.0;
        let py = pet.y - viewport.1;

        // 与 buffer 的可见交集
        let vx0 = px.max(0);
        let vy0 = py.max(0);
        let vx1 = (px + pw).min(buf_w as i32);
        let vy1 = (py + ph).min(buf_h);
        if vx0 >= vx1 || vy0 >= vy1 {
            return;
        }

        // 映射回 sprite 像素坐标（起点向上对齐到 scale 边界，终点向下对齐）
        let sx0 = (vx0 - px + scale - 1) / scale;
        let sy0 = (vy0 - py + scale - 1) / scale;
        let sx1 = (vx1 - px) / scale;
        let sy1 = (vy1 - py) / scale;

        let dim = pet.passthrough;
        for sy in sy0..sy1 {
            for sx in sx0..sx1 {
                let mut c = self.pixels[sy as usize * self.width as usize + sx as usize];
                let a = (c >> 24) & 0xff;
                if a == 0 {
                    continue;
                }
                if dim {
                    // 预乘像素整体缩放即可降不透明度
                    c = (((c >> 24) & 0xff) * 2 / 5) << 24
                        | ((((c >> 16) & 0xff) * 2 / 5) << 16)
                        | ((((c >> 8) & 0xff) * 2 / 5) << 8)
                        | ((c & 0xff) * 2 / 5);
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
}

/// 默认贴图路径：~/.config/hyprpet/pet.png
pub fn default_sprite_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("hyprpet").join("pet.png")
}

/// 32x32 像素小猫（程序生成，作为开箱即用的测试贴图）。
fn default_cat() -> Sprite {
    const W: u32 = 32;
    const H: u32 = 32;

    struct Painter {
        px: Vec<u32>,
    }
    impl Painter {
        fn set(&mut self, x: i32, y: i32, color: u32) {
            if (0..W as i32).contains(&x) && (0..H as i32).contains(&y) {
                self.px[(y as u32 * W + x as u32) as usize] = color;
            }
        }
        fn rect(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, color: u32) {
            for y in y0..y1 {
                for x in x0..x1 {
                    self.set(x, y, color);
                }
            }
        }
    }

    // 预乘 ARGB 颜色
    const fn c(a: u8, r: u8, g: u8, b: u8) -> u32 {
        ((a as u32) << 24) | (((r as u32 * a as u32 / 255) as u32) << 16)
            | (((g as u32 * a as u32 / 255) as u32) << 8)
            | ((b as u32 * a as u32 / 255) as u32)
    }
    let fur = c(255, 245, 164, 75);
    let fur_dark = c(255, 222, 130, 48);
    let belly = c(255, 255, 232, 190);
    let pink = c(255, 255, 138, 158);
    let dark = c(255, 58, 44, 52);
    let white = c(255, 255, 255, 255);
    let blush = c(255, 255, 150, 150);

    let mut p = Painter { px: vec![0u32; (W * H) as usize] };

    // 耳朵
    for i in 0..6 {
        let inset = i / 2;
        p.rect(5 + i, 3 + inset, 12 - inset, 5 + i, fur);
        p.rect(20 + inset, 3 + inset, 27 - i, 5 + i, fur);
    }
    p.rect(8, 4, 10, 7, pink);
    p.rect(22, 4, 24, 7, pink);
    // 头 + 身体
    p.rect(4, 6, 28, 24, fur);
    p.rect(4, 6, 28, 9, fur_dark); // 头顶花纹
    p.rect(2, 14, 30, 24, fur); // 身体外扩
    // 肚皮
    p.rect(11, 17, 21, 25, belly);
    // 脚
    p.rect(6, 23, 11, 27, fur_dark);
    p.rect(21, 23, 26, 27, fur_dark);
    p.rect(7, 25, 10, 27, pink);
    p.rect(22, 25, 25, 27, pink);
    // 眼睛（含高光）
    p.rect(9, 12, 12, 15, dark);
    p.rect(20, 12, 23, 15, dark);
    p.set(11, 12, white);
    p.set(22, 12, white);
    // 腮红
    p.rect(6, 15, 8, 16, blush);
    p.rect(24, 15, 26, 16, blush);
    // 鼻子 + 嘴
    p.set(15, 16, pink);
    p.set(16, 16, pink);
    p.set(14, 17, dark);
    p.set(17, 17, dark);
    // 尾巴（右侧条纹）
    p.rect(28, 16, 30, 22, fur);
    p.rect(28, 16, 30, 18, fur_dark);
    p.rect(28, 20, 30, 22, fur_dark);

    let px = p.px;
    Sprite { width: W, height: H, pixels: px }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_cat_is_opaque_in_center() {
        let cat = default_cat();
        assert!(cat.pixels[(15 * 32 + 15) as usize] >> 24 == 0xff);
    }

    /// PNG 往返必须保持颜色（回归：u8*255 溢出导致全黑）
    #[test]
    fn png_roundtrip_preserves_color() {
        let dir = std::env::temp_dir().join("hyprpet_test_roundtrip");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pet.png");
        std::fs::remove_file(&path).ok();
        let sprite = Sprite::load_or_default(&path).unwrap();
        assert_eq!(sprite.pixels[(16 * 32 + 16) as usize], 0xFFFF8A9E); // 鼻子粉色
        std::fs::remove_file(&path).ok();
    }
}

