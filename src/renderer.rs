//! wl_shm 渲染封装（基于 SCTK SlotPool）。
//!
//! 相比 softbuffer 的优势：
//! - 复用同一条 Wayland 连接与事件队列，不引入第二个 libwayland 连接
//! - buffer release 由 SCTK 自动跟踪，slot 自动回收
//! - 像素格式 0xAARRGGBB 与 wl_shm Argb8888 小端布局一致，可零拷贝直写
//!
//! 策略：每次绘制申请一个新 buffer（池自动复用已释放的 slot），
//! 绘制 → attach → damage → commit。

use anyhow::{Context, Result};
use smithay_client_toolkit::shm::slot::{Buffer, SlotPool};
use smithay_client_toolkit::shm::Shm;
use wayland_client::protocol::wl_shm::Format;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::Proxy;

pub struct Renderer {
    pool: SlotPool,
    size: (u32, u32),
}

impl Renderer {
    pub fn new(shm: &Shm, width: u32, height: u32) -> Result<Self> {
        let mut renderer = Renderer { pool: SlotPool::new(1, shm).context("创建 shm pool 失败")?, size: (0, 0) };
        renderer.ensure_pool(width, height);
        Ok(renderer)
    }

    /// 保证 pool 至少能装下 3 帧（双缓冲 + 余量）
    fn ensure_pool(&mut self, width: u32, height: u32) {
        let need = (width * height * 4) as usize * 3;
        if self.pool.len() < need {
            if let Err(e) = self.pool.resize(need) {
                eprintln!("[hyprpet] pool resize 失败: {e}");
            }
        }
        self.size = (width, height);
    }

    /// 绘制一帧并提交到 surface。
    ///
    /// `viewport` 是该 surface 在全局布局中的原点（多显示器坐标换算用），
    /// `draw` 收到 u32 像素视图（0xAARRGGBB，长度 = width*height）、行宽与 viewport。
    pub fn present(
        &mut self,
        surface: &WlSurface,
        width: u32,
        height: u32,
        viewport: (i32, i32),
        draw: impl FnOnce(&mut [u32], usize, (i32, i32)),
    ) -> Result<()> {
        if self.size != (width, height) {
            self.ensure_pool(width, height);
        }
        let stride = width * 4;
        let (buffer, canvas) = match create_buffer(&mut self.pool, width, height, stride) {
            Ok(pair) => pair,
            Err(_) => {
                self.pool
                    .resize((width * height * 4) as usize * 3)
                    .context("shm pool 扩容失败")?;
                create_buffer(&mut self.pool, width, height, stride).context("创建 shm buffer 失败")?
            }
        };

        let pixels = u32_view(canvas);
        pixels.fill(0); // 全透明背景
        draw(pixels, width as usize, viewport);

        buffer
            .attach_to(surface)
            .context("attach buffer 失败")?;
        if surface.version() >= 4 {
            surface.damage_buffer(0, 0, width as i32, height as i32);
        } else {
            surface.damage(0, 0, width as i32, height as i32);
        }
        surface.commit();
        Ok(())
    }
}

/// 把 shm canvas 视为 u32 像素切片（小端 ARGB）。
///
/// SlotPool 的 slot 偏移保证按 8 字节对齐，且我们申请的 buffer 尺寸均为
/// stride(=w*4) 的整数倍，因此 4 字节对齐必然成立；这里仍做运行时断言兜底。
fn u32_view(canvas: &mut [u8]) -> &mut [u32] {
    assert_eq!(
        canvas.as_ptr() as usize % std::mem::align_of::<u32>(),
        0,
        "shm canvas 未按 4 字节对齐"
    );
    assert_eq!(canvas.len() % 4, 0);
    unsafe { std::slice::from_raw_parts_mut(canvas.as_mut_ptr().cast::<u32>(), canvas.len() / 4) }
}

// 类型别名，方便 main.rs 引用

fn create_buffer(
    pool: &mut SlotPool,
    width: u32,
    height: u32,
    stride: u32,
) -> std::result::Result<(Buffer, &mut [u8]), smithay_client_toolkit::shm::slot::CreateBufferError> {
    pool.create_buffer(width as i32, height as i32, stride as i32, Format::Argb8888)
}
