//! 数据驱动动作状态机（FSM）。
//!
//! 由 `actions.toml` 定义（见 assets::ActionDef）：
//! - `fps` / `frames` / `loop`：动画播放
//! - `duration`：loop 动作的持续秒数，超时转移
//! - 非 loop 动作播完最后一帧转移
//! - 转移目标：`next_random`（均匀随机）→ `next` → `idle`（若存在）

use std::collections::HashMap;

use crate::assets::ActionDef;

pub struct Fsm {
    pub actions: HashMap<String, ActionDef>,
    pub current: String,
    frame_idx: usize,
    /// 距离上次帧推进的累计秒数
    frame_acc: f32,
    /// 当前动作已持续的秒数
    elapsed: f32,
}

impl Fsm {
    pub fn new(actions: HashMap<String, ActionDef>, start: &str) -> Fsm {
        let current = if actions.contains_key(start) { start.to_string() } else { "idle".to_string() };
        Fsm { actions, current, frame_idx: 0, frame_acc: 0.0, elapsed: 0.0 }
    }

    /// 切换到指定动作（不存在则保持现状），重置播放进度
    pub fn set(&mut self, name: &str) -> bool {
        if self.actions.contains_key(name) {
            if self.current != name {
                self.current = name.to_string();
                self.frame_idx = 0;
                self.frame_acc = 0.0;
                self.elapsed = 0.0;
            }
            true
        } else {
            false
        }
    }

    pub fn def(&self) -> Option<&ActionDef> {
        self.actions.get(&self.current)
    }

    /// 当前应显示的帧在整个图集里的索引
    pub fn frame(&self) -> usize {
        let def = self.def();
        match def {
            Some(d) if !d.frames.is_empty() => d.frames[self.frame_idx % d.frames.len()],
            _ => 0,
        }
    }

    /// 推进 dt 秒，返回视觉是否发生变化。
    pub fn tick(&mut self, dt: f32) -> bool {
        let Some(def) = self.actions.get(&self.current).cloned() else {
            return false;
        };
        if def.frames.is_empty() || def.fps <= 0.0 {
            return false;
        }

        let mut changed = false;
        self.frame_acc += dt;
        self.elapsed += dt;
        let frame_time = 1.0 / def.fps;

        while self.frame_acc >= frame_time {
            self.frame_acc -= frame_time;
            self.frame_idx += 1;
            changed = true;
            if self.frame_idx >= def.frames.len() {
                if def.r#loop {
                    self.frame_idx = 0;
                } else {
                    // 播完 → 转移
                    self.transition(&def);
                    return true;
                }
            }
        }

        // loop 动作 duration 超时转移
        if def.r#loop && changed {
            if let Some(duration) = def.duration {
                if self.elapsed >= duration {
                    self.transition(&def);
                    return true;
                }
            }
        }

        changed
    }

    fn transition(&mut self, def: &ActionDef) {
        let target = if !def.next_random.is_empty() {
            // 简单均匀随机：用时间 + 当前帧作种子，避免引入 rand 依赖
            let seed = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos() as usize + self.frame_idx)
                .unwrap_or(0);
            Some(def.next_random[seed % def.next_random.len()].clone())
        } else {
            def.next.clone().or_else(|| {
                if def.next_random.is_empty() && self.actions.contains_key("idle") && self.current != "idle" {
                    Some("idle".to_string())
                } else {
                    None
                }
            })
        };
        if let Some(t) = target {
            // 目标不存在时回落 idle
            if !self.set(&t) {
                self.set("idle");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn actions() -> HashMap<String, ActionDef> {
        let mut m = HashMap::new();
        m.insert(
            "idle".into(),
            ActionDef {
                fps: 2.0,
                frames: vec![0, 1],
                r#loop: true,
                duration: Some(1.0),
                movement: None,
                auto_turn: false,
                next: None,
                next_random: vec!["blink".into()],
            },
        );
        m.insert(
            "blink".into(),
            ActionDef { fps: 2.0, frames: vec![2], r#loop: false, movement: None, auto_turn: false, next: Some("idle".into()), next_random: vec![], duration: None },
        );
        m
    }

    #[test]
    fn loop_wraps_and_duration_transitions() {
        let mut fsm = Fsm::new(actions(), "idle");
        assert_eq!(fsm.frame(), 0);
        // 0.5s → 帧 1
        assert!(fsm.tick(0.5));
        assert_eq!(fsm.frame(), 1);
        // 0.5s → 回绕到 0；累计 1.0s 达到 duration → 转移
        assert!(fsm.tick(0.5));
        assert_eq!(fsm.current, "blink");
    }

    #[test]
    fn non_loop_transitions_at_end() {
        let mut fsm = Fsm::new(actions(), "blink");
        assert!(fsm.tick(0.6)); // 播完单帧
        assert_eq!(fsm.current, "idle");
    }
}
