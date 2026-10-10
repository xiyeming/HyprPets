//! 内置像素少女：程序生成的 Q 版动漫女孩 6x4 帧动画表（48px 单元格，288x192）。
//!
//! 帧布局：
//! - Row0: [0]站姿 [1]眨眼 [2]呼吸 [3]开心^^ [4]惊讶 [5]害羞
//! - Row1: 向左走 4 帧 [0..3]
//! - Row2: 向右走 4 帧 [0..3]（镜像）
//! - Row3: [0]睡 [1]睡+Z [2]被抓 [3]下落 [4]落地压扁 [5]擦汗

use crate::sprite::Sheet;

const W: u32 = 48;
const H: u32 = 48;

const fn p(a: u8, r: u8, g: u8, b: u8) -> u32 {
    Sheet::premul(a, r, g, b)
}

const SKIN: u32 = p(255, 255, 226, 196);
const SKIN_D: u32 = p(255, 240, 200, 168);
const HAIR: u32 = p(255, 255, 148, 186);
const HAIR_D: u32 = p(255, 235, 108, 148);
const EYE: u32 = p(255, 96, 118, 214);
const EYE_D: u32 = p(255, 58, 74, 160);
const WHITE: u32 = p(255, 255, 255, 255);
const MOUTH: u32 = p(255, 196, 96, 110);
const BLUSH: u32 = p(255, 255, 158, 168);
const DRESS: u32 = p(255, 168, 150, 224);
const DRESS_D: u32 = p(255, 138, 116, 196);
const TRIM: u32 = p(255, 255, 255, 255);
const SHOE: u32 = p(255, 122, 84, 64);
const SWEAT: u32 = p(255, 130, 205, 255);
const ZZZ: u32 = p(255, 190, 205, 255);
const SPARK: u32 = p(255, 255, 236, 140);

#[derive(Clone, Copy, PartialEq)]
enum Eyes {
    Open,
    Blink,
    Happy,
    Surprised,
    Closed,
}

#[derive(Clone, Copy)]
struct Pose {
    flip: bool,
    eyes: Eyes,
    /// 手臂：0=放下 1=举起（被抓/下落）2=张开（开心）
    arms: u8,
    /// 敲键盘：0=无，bit0=左手抬起 bit1=右手抬起（工作中打字动画）
    typing: u8,
    /// 走路腿相位：0=A 1=收 2=B 3=收
    legs: u8,
    /// 垂直偏移（呼吸/跳跃）
    dy: i32,
    /// 落地压扁
    squash: i32,
    zzz: bool,
    sweat: bool,
    sparkle: bool,
    shy: bool,
}

impl Default for Pose {
    fn default() -> Self {
        Pose {
            flip: false,
            eyes: Eyes::Open,
            arms: 0,
            typing: 0,
            legs: 1,
            dy: 0,
            squash: 0,
            zzz: false,
            sweat: false,
            sparkle: false,
            shy: false,
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
            self.px[(y as u32 * 288 + x as u32) as usize] = color;
        }
    }
    fn rect(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, color: u32) {
        for y in y0..y1 {
            for x in x0..x1 {
                self.set(x, y, color);
            }
        }
    }
    /// 椭圆（cx,cy 中心，rx/ry 半径）
    fn ellipse(&mut self, cx: i32, cy: i32, rx: i32, ry: i32, color: u32) {
        for y in (cy - ry)..=(cy + ry) {
            for x in (cx - rx)..=(cx + rx) {
                let dx = (x - cx) as f32 / rx.max(1) as f32;
                let dy = (y - cy) as f32 / ry.max(1) as f32;
                if dx * dx + dy * dy <= 1.0 {
                    self.set(x, y, color);
                }
            }
        }
    }
}

fn paint(px: &mut [u32], pose: Pose) {
    let mut c = Cell { px, flip: pose.flip };
    let d = pose.dy;
    let sq = pose.squash;

    // ===== 双马尾（后发，先画在头后）=====
    // 左尾（ sway 由 legs 相位驱动）
    let sway = match pose.legs {
        0 => -1,
        2 => 1,
        _ => 0,
    };
    c.ellipse(9, 16 + d, 5, 9, HAIR); // 左团
    c.rect(5, 16 + d, 13, 32 + d, HAIR);
    c.ellipse(8 + sway, 34 + d, 4, 7, HAIR);
    c.rect(6 + sway, 34 + d, 10 + sway, 42 + d, HAIR_D); // 尾尖加深
    c.ellipse(39, 16 + d, 5, 9, HAIR); // 右团
    c.rect(35, 16 + d, 43, 32 + d, HAIR);
    c.ellipse(40 - sway, 34 + d, 4, 7, HAIR);
    c.rect(38 - sway, 34 + d, 42 - sway, 42 + d, HAIR_D);
    // 蝴蝶结
    c.rect(4, 12 + d, 9, 15 + d, DRESS_D);
    c.rect(40, 12 + d, 45, 15 + d, DRESS_D);

    // ===== 腿（裙子下）=====
    let (la, lb) = match pose.legs {
        0 => (-1i32, 1i32),
        2 => (1, -1),
        _ => (0, 0),
    };
    c.rect(19 + la, 42 + d, 24 + la, 47, SKIN);
    c.rect(25 + lb, 42 + d, 30 + lb, 47, SKIN);
    c.rect(18 + la, 45, 25 + la, 47, SHOE); // 鞋
    c.rect(24 + lb, 45, 31 + lb, 47, SHOE);

    // ===== 裙子（梯形）=====
    c.rect(17, 33 + d, 31, 40 + d + sq, DRESS);
    c.rect(14, 38 + d + sq, 34, 43 + d + sq, DRESS); // 裙摆
    c.rect(14, 38 + d + sq, 34, 39 + d + sq, DRESS_D); // 裙褶皱线
    c.rect(14, 42 + d + sq, 34, 43 + d + sq, TRIM); // 裙白边

    // ===== 键盘桌（工作中，画在手前面）=====
    if pose.typing > 0 {
        c.rect(11, 40 + d, 37, 44 + d, p(255, 58, 64, 78)); // 键盘底座
        c.rect(12, 41 + d, 36, 42 + d, p(255, 96, 104, 126)); // 键帽排
        for kx in [15, 19, 23, 27, 31] {
            c.rect(kx, 41 + d, kx + 2, 42 + d, p(255, 140, 148, 170)); // 按键
        }
    }

    // ===== 手臂 =====
    if pose.typing > 0 {
        // 敲键盘：前臂前伸，双手在键盘上交替起落
        let up_l = (pose.typing & 1 != 0) as i32;
        let up_r = (pose.typing & 2 != 0) as i32;
        c.rect(15, 33 + d, 21, 36 + d, DRESS); // 左袖
        c.rect(27, 33 + d, 33, 36 + d, DRESS); // 右袖
        c.rect(20, 34 + d, 25, 37 + d, SKIN); // 左前臂
        c.rect(23, 34 + d, 28, 37 + d, SKIN); // 右前臂
        c.rect(20, 37 + d - up_l, 25, 40 + d - up_l, SKIN_D); // 左手（抬起时上移 1px）
        c.rect(23, 37 + d - up_r, 28, 40 + d - up_r, SKIN_D); // 右手
    } else {
    match pose.arms {
        1 => {
            // 举起（被抓/下落）
            c.rect(9, 16 + d, 13, 26 + d, SKIN);
            c.rect(35, 16 + d, 39, 26 + d, SKIN);
            c.rect(8, 14 + d, 14, 17 + d, DRESS); // 袖口
            c.rect(34, 14 + d, 40, 17 + d, DRESS);
        }
        2 => {
            // 张开（开心）
            c.rect(7, 24 + d, 12, 28 + d, SKIN);
            c.rect(36, 24 + d, 41, 28 + d, SKIN);
            c.rect(5, 21 + d, 11, 25 + d, DRESS);
            c.rect(37, 21 + d, 43, 25 + d, DRESS);
        }
        _ => {
            // 放下
            c.rect(13, 33 + d, 17, 41 + d, SKIN);
            c.rect(31, 33 + d, 35, 41 + d, SKIN);
            c.rect(13, 32 + d, 17, 35 + d, DRESS);
            c.rect(31, 32 + d, 35, 35 + d, DRESS);
        }
    }
    }

    // ===== 头（Q 版大头）=====
    c.ellipse(24, 18 + d, 14, 12, SKIN);
    // 耳朵位置的发遮
    // 头发天盖 + 刘海
    c.ellipse(24, 11 + d, 15, 8, HAIR);
    // 刘海锯齿（覆盖额头）
    for (lx, lw) in [(11, 5), (17, 6), (24, 6), (31, 5)] {
        c.rect(lx, 12 + d, lx + lw, 18 + d, HAIR);
    }
    c.rect(11, 11 + d, 37, 14 + d, HAIR_D); // 顶部阴影
    // 鬓角
    c.rect(9, 16 + d, 12, 24 + d, HAIR);
    c.rect(36, 16 + d, 39, 24 + d, HAIR);

    // ===== 眼睛 =====
    match pose.eyes {
        Eyes::Open | Eyes::Surprised => {
            let eh = if pose.eyes == Eyes::Surprised { 6 } else { 5 };
            c.rect(16, 20 + d, 21, 20 + eh + d, EYE);
            c.rect(27, 20 + d, 32, 20 + eh + d, EYE);
            c.rect(16, 20 + d, 21, 21 + d, EYE_D); // 上眼线
            c.rect(27, 20 + d, 32, 21 + d, EYE_D);
            c.set(19, 21 + d, WHITE); // 高光
            c.set(30, 21 + d, WHITE);
            c.set(17, 23 + d, WHITE);
            c.set(28, 23 + d, WHITE);
        }
        Eyes::Blink | Eyes::Closed => {
            c.rect(16, 22 + d, 21, 23 + d, EYE_D);
            c.rect(27, 22 + d, 32, 23 + d, EYE_D);
        }
        Eyes::Happy => {
            c.set(16, 23 + d, EYE_D);
            c.set(17, 22 + d, EYE_D);
            c.set(18, 21 + d, EYE_D);
            c.set(19, 22 + d, EYE_D);
            c.set(20, 23 + d, EYE_D);
            c.set(27, 23 + d, EYE_D);
            c.set(28, 22 + d, EYE_D);
            c.set(29, 21 + d, EYE_D);
            c.set(30, 22 + d, EYE_D);
            c.set(31, 23 + d, EYE_D);
        }
    }

    // 眉毛
    c.rect(16, 18 + d, 21, 18 + d, HAIR_D);
    c.rect(27, 18 + d, 32, 18 + d, HAIR_D);

    // 腮红
    c.rect(12, 24 + d, 14, 25 + d, BLUSH);
    c.rect(33, 24 + d, 35, 25 + d, BLUSH);
    if pose.shy {
        c.rect(12, 23 + d, 14, 25 + d, BLUSH);
        c.rect(33, 23 + d, 35, 25 + d, BLUSH);
    }

    // 嘴
    match pose.eyes {
        Eyes::Happy => {
            c.set(22, 27 + d, MOUTH);
            c.set(23, 28 + d, MOUTH);
            c.set(24, 28 + d, MOUTH);
            c.set(25, 27 + d, MOUTH);
        }
        Eyes::Surprised => {
            c.rect(22, 27 + d, 25, 29 + d, MOUTH);
        }
        _ => {
            c.set(23, 27 + d, MOUTH);
            c.set(24, 27 + d, MOUTH);
        }
    }

    // ===== 装饰 =====
    if pose.sparkle {
        c.set(39, 8 + d, SPARK);
        c.set(40, 8 + d, SPARK);
        c.set(39, 9 + d, SPARK);
        c.set(40, 9 + d, SPARK);
    }
    if pose.zzz {
        c.rect(36, 3 + d, 40, 4 + d, ZZZ);
        c.set(39, 5 + d, ZZZ);
        c.rect(36, 6 + d, 40, 7 + d, ZZZ);
        c.rect(32, 9 + d, 37, 10 + d, ZZZ);
        c.set(36, 11 + d, ZZZ);
        c.rect(32, 12 + d, 37, 13 + d, ZZZ);
    }
    if pose.sweat {
        c.set(13, 6 + d, SWEAT);
        c.set(14, 6 + d, SWEAT);
        c.set(13, 7 + d, SWEAT);
        c.set(14, 7 + d, SWEAT);
    }
}

fn frame(sheet: &mut [u32], col: u32, row: u32, pose: Pose) {
    let start = (row * H * 288 + col * W) as usize;
    paint(&mut sheet[start..], pose);
}

/// 生成完整动画表
pub fn build_sheet() -> Sheet {
    let mut px = vec![0u32; (288 * 192) as usize];

    // Row0: 表情
    frame(&mut px, 0, 0, Pose::default());
    frame(&mut px, 1, 0, Pose { eyes: Eyes::Blink, ..Default::default() });
    frame(&mut px, 2, 0, Pose { dy: 1, ..Default::default() });
    frame(&mut px, 3, 0, Pose { eyes: Eyes::Happy, arms: 2, sparkle: true, ..Default::default() });
    frame(&mut px, 4, 0, Pose { eyes: Eyes::Surprised, arms: 1, ..Default::default() });
    frame(&mut px, 5, 0, Pose { eyes: Eyes::Happy, shy: true, ..Default::default() });

    // Row1: 向左走
    let walk = [
        Pose { legs: 0, ..Default::default() },
        Pose { legs: 1, dy: 1, ..Default::default() },
        Pose { legs: 2, ..Default::default() },
        Pose { legs: 3, dy: 1, ..Default::default() },
    ];
    for (i, p) in walk.iter().enumerate() {
        frame(&mut px, i as u32, 1, *p);
    }
    // Row2: 向右走（镜像）
    for (i, p) in walk.iter().enumerate() {
        frame(&mut px, i as u32, 2, Pose { flip: true, ..*p });
    }

    // Row1 剩余位：敲键盘相位（左起/右起/双起/右起）
    frame(&mut px, 4, 1, Pose { typing: 1, ..Default::default() });
    frame(&mut px, 5, 1, Pose { typing: 2, ..Default::default() });
    // Row2 剩余位：继续敲键相位
    frame(&mut px, 4, 2, Pose { typing: 3, ..Default::default() });
    frame(&mut px, 5, 2, Pose { typing: 2, ..Default::default() });

    // Row3: 睡/被抓/下落/落地/擦汗
    frame(&mut px, 0, 3, Pose { eyes: Eyes::Closed, dy: 1, ..Default::default() });
    frame(&mut px, 1, 3, Pose { eyes: Eyes::Closed, dy: 2, zzz: true, ..Default::default() });
    frame(&mut px, 2, 3, Pose { eyes: Eyes::Surprised, arms: 1, dy: -1, ..Default::default() });
    frame(&mut px, 3, 3, Pose { eyes: Eyes::Surprised, arms: 1, dy: -2, ..Default::default() });
    frame(&mut px, 4, 3, Pose { eyes: Eyes::Blink, squash: 2, ..Default::default() });
    frame(&mut px, 5, 3, Pose { eyes: Eyes::Closed, sweat: true, ..Default::default() });

    Sheet::grid(px, 288, 192, W, H)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sheet_generated() {
        let sheet = build_sheet();
        assert_eq!(sheet.frames.len(), 24);
        assert!(sheet.pixels_probe().iter().any(|&c| c >> 24 == 0xff));
    }
}
