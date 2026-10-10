//! 像素史莱姆：程序生成的 8x4 帧动画表（32px 单元格，256x128）。
//!
//! 帧布局（与 builtin_cat 相同约定）：
//! - Row0: [0]待机 [1]弹跳 [2]眨眼 [3]开心^^
//! - Row1: 向左走 4 帧（压扁-拉伸循环）
//! - Row2: 向右走 4 帧（镜像）
//! - Row3: [0]睡1 [1]睡2+Z [2]被抓(拉长) [3]下落 [4]落地(扁) [5]疲惫+汗 [6]庆祝(跳) [7]空

use crate::sprite::Sheet;

const W: u32 = 32;
const H: u32 = 32;

const fn p(a: u8, r: u8, g: u8, b: u8) -> u32 {
    Sheet::premul(a, r, g, b)
}

const BODY: u32 = p(255, 120, 220, 130);
const BODY_HI: u32 = p(255, 180, 245, 175);
const BODY_DARK: u32 = p(255, 70, 175, 95);
const DARK: u32 = p(255, 45, 60, 70);
const WHITE: u32 = p(255, 255, 255, 255);
const BLUSH: u32 = p(255, 255, 150, 170);
const SWEAT: u32 = p(255, 120, 200, 255);
const ZZZ: u32 = p(255, 190, 205, 255);

#[derive(Clone, Copy, PartialEq)]
enum Eyes {
    Open,
    Blink,
    Happy,
    Closed,
}

#[derive(Clone, Copy)]
struct Pose {
    flip: bool,
    eyes: Eyes,
    /// 垂直压扁（0..8，底部锚定）
    squash: i32,
    /// 水平拉伸（0..6）
    stretch: i32,
    /// 眼睛水平偏移（走路重心）
    eye_shift: i32,
    zzz: bool,
    sweat: bool,
    /// 敲键盘：bit0=左手抬起 bit1=右手抬起
    typing: u8,
}

impl Default for Pose {
    fn default() -> Self {
        Pose {
            flip: false,
            eyes: Eyes::Open,
            squash: 0,
            stretch: 0,
            eye_shift: 0,
            zzz: false,
            sweat: false,
            typing: 0,
        }
    }
}

struct Cell<'a> {
    px: &'a mut [u32],
    flip: bool,
}

impl<'a> Cell<'a> {
    fn set(&mut self, x: i32, y: i32, color: u32) {
        let x = if self.flip { W as i32 - 1 - x } else { x };
        if (0..W as i32).contains(&x) && (0..H as i32).contains(&y) {
            self.px[(y as u32 * 256 + x as u32) as usize] = color;
        }
    }
}

/// 画一只史莱姆：椭圆身体（按行宽度），底部锚定压扁/拉伸
fn paint_slime(cell: &mut [u32], pose: Pose) {
    let mut c = Cell { px: cell, flip: pose.flip };

    let squash = pose.squash.clamp(0, 8);
    let stretch = pose.stretch.clamp(0, 6);
    let top = 6 + squash; // 顶部下移 = 变矮
    let bottom = 26; // 底部锚定
    let cx = 16i32;
    let base_half = 11 + stretch; // 最大半宽

    // 身体：抛物线轮廓
    let rows = bottom - top;
    for i in 0..rows {
        let t = i as f32 / rows as f32; // 0=顶 1=底
        let half = (base_half as f32 * (1.0 - (1.0 - t).powi(2) * 0.85)).round() as i32;
        let y = top + i;
        let color = if i < 3 { BODY_HI } else { BODY };
        for x in (cx - half)..(cx + half) {
            c.set(x, y, color);
        }
    }
    // 底部阴影线
    for x in (cx - base_half + 1)..(cx + base_half - 1) {
        c.set(x, bottom - 1, BODY_DARK);
    }

    // 高光（左上角）
    for (hx, hy) in [(cx - 6, top + 3), (cx - 5, top + 3), (cx - 6, top + 4), (cx - 4, top + 4)] {
        c.set(hx, hy, BODY_HI);
    }

    // 眼睛
    let ey = top + rows / 2 - 2;
    let ex1 = cx - 5 + pose.eye_shift;
    let ex2 = cx + 3 + pose.eye_shift;
    match pose.eyes {
        Eyes::Open => {
            c.rect(ex1, ey, ex1 + 3, ey + 3, DARK);
            c.rect(ex2, ey, ex2 + 3, ey + 3, DARK);
            c.set(ex1 + 2, ey, WHITE);
            c.set(ex2 + 2, ey, WHITE);
        }
        Eyes::Blink | Eyes::Closed => {
            c.rect(ex1, ey + 1, ex1 + 3, ey + 2, DARK);
            c.rect(ex2, ey + 1, ex2 + 3, ey + 2, DARK);
        }
        Eyes::Happy => {
            c.set(ex1, ey + 2, DARK);
            c.set(ex1 + 1, ey + 1, DARK);
            c.set(ex1 + 2, ey + 2, DARK);
            c.set(ex2, ey + 2, DARK);
            c.set(ex2 + 1, ey + 1, DARK);
            c.set(ex2 + 2, ey + 2, DARK);
        }
    }

    // 腮红
    c.rect(cx - 8 + pose.eye_shift, ey + 3, cx - 6 + pose.eye_shift, ey + 4, BLUSH);
    c.rect(cx + 6 + pose.eye_shift, ey + 3, cx + 8 + pose.eye_shift, ey + 4, BLUSH);

    // 嘴
    c.set(cx - 1, ey + 4, DARK);
    c.set(cx, ey + 4, DARK);

    // 键盘（工作中）：前置键盘 + 两只小手交替敲击
    if pose.typing > 0 {
        c.rect(6, 24, 26, 29, p(255, 58, 64, 78));
        c.rect(7, 25, 25, 26, p(255, 96, 104, 126));
        for kx in [9, 13, 17, 21] {
            c.rect(kx, 25, kx + 1, 26, p(255, 140, 148, 170));
        }
        let up_l = (pose.typing & 1 != 0) as i32;
        let up_r = (pose.typing & 2 != 0) as i32;
        c.rect(9, 20 + up_l, 13, 24 + up_l, BODY);
        c.rect(19, 20 + up_r, 23, 24 + up_r, BODY);
        c.rect(10, 23 + up_l, 12, 24 + up_l, BODY_HI);
        c.rect(20, 23 + up_r, 22, 24 + up_r, BODY_HI);
    }

    // Zzz
    if pose.zzz {
        c.rect(24, 3, 27, 4, ZZZ);
        c.set(26, 5, ZZZ);
        c.rect(24, 6, 27, 7, ZZZ);
        c.rect(22, 9, 26, 10, ZZZ);
        c.set(25, 11, ZZZ);
        c.rect(22, 12, 26, 13, ZZZ);
    }
    // 汗滴
    if pose.sweat {
        c.set(12, 2, SWEAT);
        c.set(13, 2, SWEAT);
        c.set(12, 3, SWEAT);
        c.set(13, 3, SWEAT);
    }
}

trait RectExt {
    fn rect(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, color: u32);
}
impl<'a> RectExt for Cell<'a> {
    fn rect(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, color: u32) {
        for y in y0..y1 {
            for x in x0..x1 {
                self.set(x, y, color);
            }
        }
    }
}

fn paint_frame(sheet: &mut [u32], col: u32, row: u32, pose: Pose) {
    const SHEET_W: u32 = 256;
    let start = (row * H * SHEET_W + col * W) as usize;
    paint_slime(&mut sheet[start..], pose);
}

/// 生成完整动画表
pub fn build_sheet() -> Sheet {
    let mut px = vec![0u32; (256 * 128) as usize];

    // Row0: 待机 / 弹跳 / 眨眼 / 开心
    paint_frame(&mut px, 0, 0, Pose::default());
    paint_frame(&mut px, 1, 0, Pose { squash: 2, ..Default::default() });
    paint_frame(&mut px, 2, 0, Pose { eyes: Eyes::Blink, ..Default::default() });
    paint_frame(&mut px, 3, 0, Pose { eyes: Eyes::Happy, stretch: 2, ..Default::default() });

    // Row1: 向左走（压扁-拉伸 + 重心偏移）
    let walk = [
        Pose { squash: 3, stretch: 2, eye_shift: -1, ..Default::default() },
        Pose { squash: 0, stretch: 0, eye_shift: 0, ..Default::default() },
        Pose { squash: 3, stretch: 2, eye_shift: 1, ..Default::default() },
        Pose { squash: 1, stretch: 1, eye_shift: 0, ..Default::default() },
    ];
    for (i, pose) in walk.iter().enumerate() {
        paint_frame(&mut px, i as u32, 1, *pose);
    }
    // Row1 空位：敲键盘相位
    paint_frame(&mut px, 4, 1, Pose { typing: 1, ..Default::default() });
    paint_frame(&mut px, 5, 1, Pose { typing: 2, ..Default::default() });
    paint_frame(&mut px, 6, 1, Pose { typing: 3, ..Default::default() });
    paint_frame(&mut px, 7, 1, Pose { typing: 2, ..Default::default() });

    // Row2: 向右走（镜像）
    for (i, pose) in walk.iter().enumerate() {
        paint_frame(&mut px, i as u32, 2, Pose { flip: true, ..*pose });
    }

    // Row3: 睡 / 被抓 / 下落 / 落地 / 疲惫 / 庆祝
    paint_frame(&mut px, 0, 3, Pose { eyes: Eyes::Closed, squash: 4, ..Default::default() });
    paint_frame(&mut px, 1, 3, Pose { eyes: Eyes::Closed, squash: 5, zzz: true, ..Default::default() });
    paint_frame(&mut px, 2, 3, Pose { stretch: 3, eye_shift: 0, ..Default::default() });
    paint_frame(&mut px, 3, 3, Pose { stretch: 3, eye_shift: 0, ..Default::default() });
    paint_frame(&mut px, 4, 3, Pose { squash: 6, stretch: 4, eyes: Eyes::Blink, ..Default::default() });
    paint_frame(&mut px, 5, 3, Pose { squash: 5, eyes: Eyes::Closed, sweat: true, ..Default::default() });
    paint_frame(&mut px, 6, 3, Pose { eyes: Eyes::Happy, squash: 1, stretch: 2, ..Default::default() });

    Sheet::grid(px, 256, 128, W, H)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sheet_has_32_frames() {
        let sheet = build_sheet();
        assert_eq!(sheet.frames.len(), 32);
        assert!(sheet.pixels_probe().iter().any(|&c| c >> 24 == 0xff));
    }
}
