use crate::app::StreamingSession;
use crate::shell::egui_painter::SdlEguiPainter;
use crate::streaming::video::{DirectVideoOutput};
use anyhow::{Context, Result};
use sdl2::pixels::PixelFormatEnum;
use sdl2::render::{Canvas, Texture};
use sdl2::video::Window;
use std::cell::Cell;
use std::sync::Arc;
use std::time::Instant;
use vitasdk_sys::{SceGxmContext, SceGxmNotification};
use std::sync::atomic::Ordering;

pub const WIDTH: u32 = 960;
pub const HEIGHT: u32 = 544;

thread_local! {
    static SCENE_CAPTURE: Cell<Option<*mut SceGxmContext>> = const { Cell::new(None) };
}

unsafe extern "C" {
    fn sceGxmFinish(context: *mut SceGxmContext);
    fn __real_sceGxmEndScene(context: *mut SceGxmContext, vertex: *const SceGxmNotification, fragment: *const SceGxmNotification) -> i32;
}

#[unsafe(no_mangle)]
unsafe extern "C" fn __wrap_sceGxmEndScene(context: *mut SceGxmContext, vertex: *const SceGxmNotification, fragment: *const SceGxmNotification) -> i32 {
    let result = unsafe { __real_sceGxmEndScene(context, vertex, fragment) };
    if result >= 0 {
        SCENE_CAPTURE.with(|capture| {
            if capture.get().is_some() {
                capture.set(Some(context));
            }
        });
    }
    result
}

pub struct VitaSurface {
    pub(crate) canvas: Canvas<Window>,
    video_texture: Option<Texture>,
    source: Option<sdl2::rect::Rect>,
    scene: Option<*mut SceGxmContext>,
    direct_video_output: Option<Arc<DirectVideoOutput>>,
    video_width: u32,
    video_height: u32,
    last_frame_id: u64,
    egui_painter: SdlEguiPainter,
}

impl VitaSurface {
    pub fn new(video: &sdl2::VideoSubsystem) -> Result<Self> {
        let window = video
            .window("Turbo GreenVita", WIDTH, HEIGHT)
            .position_centered()
            .build()
            .context("failed to create SDL Vita window")?;
        let mut canvas = window
            .into_canvas()
            .accelerated()
            .build()
            .map_err(anyhow::Error::msg)
            .context("failed to create SDL Vita renderer")?;
        canvas
            .set_logical_size(WIDTH, HEIGHT)
            .map_err(anyhow::Error::msg)
            .context("failed to set Vita logical render size")?;

        Ok(Self {
            canvas,
            video_texture: None,
            source: None,
            scene: None,
            direct_video_output: None,
            video_width: 0,
            video_height: 0,
            last_frame_id: 0,
            egui_painter: SdlEguiPainter::default(),
        })
    }

    /// Where the video quad lands on screen - accounts for letterboxing, see `fit_rect`.
    pub fn video_rect(&self) -> sdl2::rect::Rect {
        Self::fit_rect(self.video_width, self.video_height, WIDTH, HEIGHT)
    }

    pub fn sync_video_frame(&mut self, streaming: Option<&StreamingSession>) -> Result<()> {
        let Some(streaming) = streaming else {
            self.detach_video();
            return Ok(());
        };
        let output = streaming.direct_video_output();
        if !self.direct_video_output.as_ref().is_some_and(|old| Arc::ptr_eq(old, &output)) {
            self.detach_video();
            self.direct_video_output = Some(Arc::clone(&output));
        }
        let Some((frame_id, frame)) = streaming.video_frame() else { return Ok(()); };
        if frame_id == self.last_frame_id { return Ok(()); }
        let layout = frame.layout;
        if self.video_texture.is_none() {
            let info = self.canvas.info();
            anyhow::ensure!(info.name == "VITA gxm" && info.texture_formats.contains(&PixelFormatEnum::NV12), "native Vita GXM NV12 support required");
            self.video_texture = Some(self.canvas.create_texture_streaming(PixelFormatEnum::NV12, layout.width, layout.height).map_err(anyhow::Error::msg)?);
        }
        let metrics = &crate::streaming::video::metrics::METRICS;
        let start = Instant::now();
        if self.source.is_some() {
            let scene = self.scene.take().context("missing GXM completion context before NV12 upload")?;
            unsafe { sceGxmFinish(scene) };
        }
        metrics.gpu_wait_us.store(start.elapsed().as_micros() as u64, Ordering::Relaxed);
        let texture = self.video_texture.as_ref().unwrap();
        let start = Instant::now();
        let upload = output.with_frame(frame, |base| unsafe {
            use sdl2::sys::*;
            SDL_SetYUVConversionMode(SDL_YUV_CONVERSION_MODE::SDL_YUV_CONVERSION_BT709);
            SDL_UpdateNVTexture(texture.raw(), std::ptr::null(), base, layout.width as i32, base.add(layout.uv_offset as usize), layout.width as i32)
        });
        self.last_frame_id = frame_id;
        if let Some(result) = upload {
            anyhow::ensure!(result == 0, "NV12 upload failed: {}", sdl2::get_error());
            metrics.upload_us.store(start.elapsed().as_micros() as u64, Ordering::Relaxed);
            let [x, y, w, h] = frame.visible;
            self.source = Some(sdl2::rect::Rect::new(x as i32, y as i32, w, h));
            self.video_width = w;
            self.video_height = h;
            metrics.presented.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    fn detach_video(&mut self) {
        if let Some(scene) = self.scene.take() {
            unsafe { sceGxmFinish(scene) };
        }
        if let Some(texture) = self.video_texture.take() {
            unsafe { texture.destroy() };
        }
        self.direct_video_output = None;
        self.source = None;
        self.video_width = 0;
        self.video_height = 0;
        self.last_frame_id = 0;
    }

    pub fn draw_scene(&mut self, show_video: bool) -> Result<()> {
        SCENE_CAPTURE.with(|capture| capture.set(Some(std::ptr::null_mut())));
        self.canvas.set_draw_color(sdl2::pixels::Color::BLACK);
        self.canvas.clear();

        if show_video
            && let Some(source) = self.source
            && let Some(texture) = &self.video_texture
        {
            let destination = self.video_rect();
            self.canvas
                .copy(texture, source, destination)
                .map_err(anyhow::Error::msg)
                .context("failed to draw SDL YUV video frame")?;
        }

        Ok(())
    }

    pub fn paint_egui(
        &mut self,
        pixels_per_point: f32,
        primitives: &[egui::ClippedPrimitive],
        textures_delta: &egui::TexturesDelta,
    ) -> Result<()> {
        self.egui_painter.paint(
            &mut self.canvas,
            [WIDTH, HEIGHT],
            pixels_per_point,
            primitives,
            textures_delta,
        )?;
        self.canvas.present();
        let context = SCENE_CAPTURE.with(|capture| capture.take()).filter(|context| !context.is_null());
        if self.source.is_some() {
            self.scene = context.or(self.scene);
            anyhow::ensure!(self.scene.is_some(), "missing GXM completion context after NV12 draw");
        }
        Ok(())
    }

    fn fit_rect(src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> sdl2::rect::Rect {
        if src_w == 0 || src_h == 0 {
            return sdl2::rect::Rect::new(0, 0, dst_w, dst_h);
        }
        let src_aspect = src_w as f32 / src_h as f32;
        let dst_aspect = dst_w as f32 / dst_h as f32;
        if src_aspect > dst_aspect {
            let height = (dst_w as f32 / src_aspect).round() as u32;
            let y = ((dst_h - height) / 2) as i32;
            sdl2::rect::Rect::new(0, y, dst_w, height)
        } else {
            let width = (dst_h as f32 * src_aspect).round() as u32;
            let x = ((dst_w - width) / 2) as i32;
            sdl2::rect::Rect::new(x, 0, width, dst_h)
        }
    }
}
