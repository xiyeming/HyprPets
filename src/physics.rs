//! 轻量抛掷物理：重力 + 初速度，落地由 App 根据布局边界判定。

#[derive(Debug, Default)]
pub struct Physics {
    pub vx: f32,
    pub vy: f32,
    pub active: bool,
}

impl Physics {
    /// 像素/秒²
    pub const GRAVITY: f32 = 2400.0;
    /// 终端速度
    pub const TERMINAL: f32 = 3200.0;
    /// 低于此速度不再触发抛掷（直接落地）
    pub const TOSS_THRESHOLD: f32 = 200.0;

    pub fn toss(&mut self, vx: f32, vy: f32) {
        self.vx = vx.clamp(-Self::TERMINAL, Self::TERMINAL);
        self.vy = vy.clamp(-Self::TERMINAL, Self::TERMINAL);
        self.active = true;
    }

    pub fn stop(&mut self) {
        self.vx = 0.0;
        self.vy = 0.0;
        self.active = false;
    }
}
