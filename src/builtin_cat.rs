//! 内置像素小猫：程序生成的 8x4 帧动画表（32px 单元格，256x128）。
//!
//! 帧布局：
//! - Row0: [0]坐姿 [1]眨眼 [2]警觉 [3]开心^^
//! - Row1: 向左走 4 帧
//! - Row2: 向右走 4 帧（Row1 镜像）
//! - Row3: [0]睡1 [1]睡2 [2]睡3+Zzz [3]被抓 [4]下落 [5]落地压扁

use crate::sprite::Sheet;

const W: u32 = 32;
const H: u32 = 32;

const fn p(a: u8, r: u8, g: u8, b: u8) -> u32 {
    Sheet::premul(a, r, g, b)
}

const FUR: u32 = p(255, 245, 164, 75);
const FUR_DARK: u32 = p(255, 222, 130, 48);
const BELLY: u32 = p(255, 255, 232, 190);
const PINK: u32 = p(255, 255, 138, 158);
const DARK: u32 = p(255, 58, 44, 52);
const WHITE: u32 = p(255, 255, 255, 255);
const BLUSH: u32 = p(255, 255, 150, 150);
const ZZZ: u32 = p(255, 190, 205, 255);

#[derive(Clone, Copy, PartialEq)]
enum Eyes {
    Open,
    Blink,
    Happy, // ^^
    Closed, // 睡（下弯）
}

#[derive(Clone, Copy)]
struct Pose {
    flip: bool,
    eyes: Eyes,
    /// 身体垂直偏移（走路上下颠）
    body_dy: i32,
    /// 压扁（落地）：>0 时身体下压、横向变宽
    squash: i32,
    /// 走路腿相位：0=A 1=并腿 2=B 3=并腿
    legs: u8,
    tail_up: bool,
    zzz: bool,
}

impl Default for Pose {
    fn default() -> Self {
        Pose {
            flip: false,
            eyes: Eyes::Open,
            body_dy: 0,
            squash: 0,
            legs: 1,
            tail_up: false,
            zzz: false,
        }
    }
}

struct Cell<'a> {
    px: &'a mut [u32],
    /// 整表行宽（cell 是表内的一块连续内存，行索引必须用整表宽度）
    stride: u32,
    flip: bool,
}

impl<'a> Cell<'a> {
    fn set(&mut self, x: i32, y: i32, color: u32) {
        let x = if self.flip { W as i32 - 1 - x } else { x };
        if (0..W as i32).contains(&x) && (0..H as i32).contains(&y) {
            self.px[(y as u32 * self.stride + x as u32) as usize] = color;
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

/// 画一只猫到单元格（cell 以整表行宽 stride 索引）
fn paint_cat(cell: &mut [u32], stride: u32, pose: Pose) {
    let flip = pose.flip;
    let d = pose.body_dy;
    let sq = pose.squash;
    let mut c = Cell { px: cell, stride, flip };

    // 耳朵（警觉时更高挺）
    let ear_lift = if pose.eyes == Eyes::Open && pose.tail_up { 0 } else { 0 };
    let _ = ear_lift;
    for i in 0..6 {
        let inset = i / 2;
        c.rect(5 + i, 3 + inset + d, 12 - inset, 5 + i + d, FUR);
        c.rect(20 + inset, 3 + inset + d, 27 - i, 5 + i + d, FUR);
    }
    c.rect(8, 4 + d, 10, 7 + d, PINK);
    c.rect(22, 4 + d, 24, 7 + d, PINK);

    // 头 + 身体（squash：压扁时下移、变矮变宽）
    let body_top = 6 + d + sq;
    let body_bot = 24 + d;
    c.rect(4 - sq, body_top, 28 + sq, body_bot, FUR);
    c.rect(4 - sq, body_top, 28 + sq, (body_top + 3).min(body_bot), FUR_DARK);
    c.rect(2 - sq, 14 + d, 30 + sq, body_bot, FUR);

    // 肚皮
    c.rect(11, (17 + d + sq).min(24), 21, (25 + d).min(27), BELLY);

    // 脚（走路交替；squash 时贴地张开）
    let (leg_a, leg_b) = match pose.legs {
        0 => (-2i32, 2i32),
        2 => (2, -2),
        _ => (0, 0),
    };
    let leg_y = 23 + d;
    if sq > 0 {
        c.rect(4, leg_y, 10, 27, FUR_DARK);
        c.rect(22, leg_y, 28, 27, FUR_DARK);
        c.rect(6, 26, 9, 27, PINK);
        c.rect(23, 26, 26, 27, PINK);
    } else {
        c.rect(6 + leg_a, leg_y, 11 + leg_a, 27, FUR_DARK);
        c.rect(21 + leg_b, leg_y, 26 + leg_b, 27, FUR_DARK);
        c.rect(7 + leg_a, 25, 10 + leg_a, 27, PINK);
        c.rect(22 + leg_b, 25, 25 + leg_b, 27, PINK);
    }

    // 眼睛
    match pose.eyes {
        Eyes::Open => {
            c.rect(9, 12 + d, 12, 15 + d, DARK);
            c.rect(20, 12 + d, 23, 15 + d, DARK);
            c.set(11, 12 + d, WHITE);
            c.set(22, 12 + d, WHITE);
        }
        Eyes::Blink => {
            c.rect(9, 14 + d, 12, 15 + d, DARK);
            c.rect(20, 14 + d, 23, 15 + d, DARK);
        }
        Eyes::Happy => {
            // ^^ 形：两段斜像素
            c.set(9, 15 + d, DARK);
            c.set(10, 14 + d, DARK);
            c.set(11, 15 + d, DARK);
            c.set(20, 15 + d, DARK);
            c.set(21, 14 + d, DARK);
            c.set(22, 15 + d, DARK);
        }
        Eyes::Closed => {
            c.rect(9, 14 + d, 12, 15 + d, DARK);
            c.rect(20, 14 + d, 23, 15 + d, DARK);
            c.set(12, 15 + d, DARK);
            c.set(23, 15 + d, DARK);
        }
    }

    // 腮红（开心时加深）
    let blush = if pose.eyes == Eyes::Happy { p(255, 255, 120, 130) } else { BLUSH };
    c.rect(6, 15 + d, 8, 16 + d, blush);
    c.rect(24, 15 + d, 26, 16 + d, blush);

    // 鼻子 + 嘴
    c.set(15, 16 + d, PINK);
    c.set(16, 16 + d, PINK);
    c.set(14, 17 + d, DARK);
    c.set(17, 17 + d, DARK);

    // 尾巴：默认垂下在右侧，tail_up 时上翘；被抓/下落时张开
    match pose.legs {
        0 | 2 if !pose.tail_up => {
            // 走路摆动
            let tw = if pose.legs == 0 { -1 } else { 1 };
            c.rect(28, 16 + d + tw, 30, 22 + d + tw, FUR);
            c.rect(28, 16 + d + tw, 30, 18 + d + tw, FUR_DARK);
            c.rect(28, 20 + d + tw, 30, 22 + d + tw, FUR_DARK);
        }
        _ if pose.tail_up => {
            c.rect(28, 8 + d, 30, 14 + d, FUR);
            c.rect(28, 8 + d, 30, 10 + d, FUR_DARK);
        }
        _ => {
            c.rect(28, 16 + d, 30, 22 + d, FUR);
            c.rect(28, 16 + d, 30, 18 + d, FUR_DARK);
            c.rect(28, 20 + d, 30, 22 + d, FUR_DARK);
        }
    }

    // Zzz 气泡
    if pose.zzz {
        c.set(26, 2, ZZZ);
        c.set(27, 2, ZZZ);
        c.set(26, 3, ZZZ);
        c.set(26, 4, ZZZ);
        c.set(27, 4, ZZZ);
        c.set(26, 5, ZZZ);
        c.set(27, 5, ZZZ);
    }
}

fn paint_frame(sheet: &mut [u32], col: u32, row: u32, pose: Pose) {
    const SHEET_W: u32 = 256; // 8 格 x 32px
    let start = (row * H * SHEET_W + col * W) as usize;
    let cell = &mut sheet[start..];
    paint_cat(cell, SHEET_W, pose);
}

/// 生成完整动画表
pub fn build_sheet() -> Sheet {
    let mut px = vec![0u32; (256 * 128) as usize];

    // Row0: 表情
    paint_frame(&mut px, 0, 0, Pose::default());
    paint_frame(&mut px, 1, 0, Pose { eyes: Eyes::Blink, ..Default::default() });
    paint_frame(&mut px, 2, 0, Pose { eyes: Eyes::Open, tail_up: true, ..Default::default() });
    paint_frame(&mut px, 3, 0, Pose { eyes: Eyes::Happy, tail_up: true, ..Default::default() });

    // Row1: 向左走（上下颠 + 腿交替 + 尾摆）
    let walk = [
        Pose { legs: 0, body_dy: 0, ..Default::default() },
        Pose { legs: 1, body_dy: 1, ..Default::default() },
        Pose { legs: 2, body_dy: 0, ..Default::default() },
        Pose { legs: 3, body_dy: 1, ..Default::default() },
    ];
    for (i, pose) in walk.iter().enumerate() {
        paint_frame(&mut px, i as u32, 1, *pose);
    }

    // Row2: 向右走 = Row1 镜像
    for (i, pose) in walk.iter().enumerate() {
        paint_frame(&mut px, i as u32, 2, Pose { flip: true, ..*pose });
    }

    // Row3: 睡 / 被抓 / 下落 / 落地
    paint_frame(&mut px, 0, 3, Pose { eyes: Eyes::Closed, ..Default::default() });
    paint_frame(&mut px, 1, 3, Pose { eyes: Eyes::Closed, body_dy: 1, ..Default::default() });
    paint_frame(&mut px, 2, 3, Pose { eyes: Eyes::Closed, body_dy: 1, zzz: true, ..Default::default() });
    paint_frame(&mut px, 3, 3, Pose { eyes: Eyes::Open, tail_up: true, legs: 0, ..Default::default() });
    paint_frame(&mut px, 4, 3, Pose { eyes: Eyes::Open, tail_up: true, legs: 2, body_dy: 1, ..Default::default() });
    paint_frame(&mut px, 5, 3, Pose { eyes: Eyes::Blink, squash: 3, legs: 1, ..Default::default() });

    Sheet::grid(px, 256, 128, W, H)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sheet_has_32_frames() {
        let sheet = build_sheet();
        assert_eq!(sheet.frames.len(), 32);
        assert!(sheet.pixels.iter().any(|&c| c >> 24 == 0xff));
    }
}
