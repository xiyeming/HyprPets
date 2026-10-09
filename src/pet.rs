//! 宠物状态：位置、尺寸、缩放与交互标志。

/// 运行模式：正常 / mini（缩小、停漫游、可固定可收边）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PetMode {
    Normal,
    Mini,
}

/// 宠物在屏幕（surface 局部坐标）中的状态。
pub struct Pet {
    /// 左上角 x（像素，已含缩放）
    pub x: i32,
    /// 左上角 y
    pub y: i32,
    /// 原始贴图宽度
    pub w: u32,
    /// 原始贴图高度
    pub h: u32,
    /// 像素放大倍率（nearest 采样）
    pub scale: u32,
    /// 穿透模式：input region 置空，宠物不接收任何指针事件
    pub passthrough: bool,
    /// 指针悬停中
    pub hovered: bool,
    /// 正被拖拽
    pub dragged: bool,
    /// 运行模式
    pub mode: PetMode,
}

impl Pet {
    pub fn new(w: u32, h: u32, scale: u32) -> Self {
        Pet {
            x: 48,
            y: 48,
            w,
            h,
            scale: scale.max(1),
            passthrough: false,
            hovered: false,
            dragged: false,
            mode: PetMode::Normal,
        }
    }

    pub fn is_mini(&self) -> bool {
        self.mode == PetMode::Mini
    }

    /// 屏幕上的实际占位尺寸
    pub fn width_px(&self) -> i32 {
        self.w as i32 * self.scale as i32
    }

    pub fn height_px(&self) -> i32 {
        self.h as i32 * self.scale as i32
    }

    /// 把宠物位置钳制在联合包围盒内（多显示器布局）
    pub fn clamp_to(&mut self, min_x: i32, min_y: i32, max_x: i32, max_y: i32) {
        let x = self.x.clamp(min_x, (max_x - self.width_px()).max(min_x));
        let y = self.y.clamp(min_y, (max_y - self.height_px()).max(min_y));
        self.x = x;
        self.y = y;
    }
}
