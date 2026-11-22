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

/// Number of display-side NV12 textures we rotate between.
///
/// The decoder already stages frames in 3 CDRAM slots (see
/// `streaming/video/mod.rs`). The old code uploads into a *single* SDL
/// streaming texture, so `sync_video_frame` had to `sceGxmFinish()` the
/// previous scene before overwriting data the GPU might still be sampling
/// — serialising CPU tessellation behind the GPU and pinning us near
/// 30 presented fps at 720p. With 3 display textures the upload targets
/// texture N while the GPU is still reading texture N-1; we only wait for
/// the scene that last sampled the texture we are about to reuse (N-3).
const DISPLAY_TEXTURE_COUNT: usize = 3;

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
    /// Rotating display textures. `display_textures.len()` is
    /// `DISPLAY_TEXTURE_COUNT` once the layout is known; `display_textures[0]`
    /// is the texture the GPU is most recently sampling.
    display_textures: Vec<Texture>,
    /// Which display texture the next upload targets (round-robin).
    next_display_index: usize,
    /// Which display texture the next `draw_scene` copy samples from: the
    /// texture targeted by the most recent successful upload.
    display_index: usize,
    /// GXM scene that last sampled each display texture, in display-index
    /// order. Waiting on `scene_for[next_display_index]` before reusing a
    /// texture is what keeps the GPU safe while letting the other
    /// `DISPLAY_TEXTURE_COUNT - 1` scenes stay in flight.
    scene_for: Vec<Option<*mut SceGxmContext>>,
    source: Option<sdl2::rect::Rect>,
    direct_video_output: Option<Arc<DirectVideoOutput>>,
    video_width: u32,
    video_height: u32,
    last_frame_id: u64,
    egui_painter: SdlEguiPainter,
    /// Cached overlay geometry from the last full egui pass. Replayed on
    /// video-only frames so the debug chip doesn't strobe at 10Hz.
    cached_primitives: Vec<egui::ClippedPrimitive>,
    cached_pixels_per_point: f32,
    cached_textures_delta: egui::TexturesDelta,
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
            display_textures: Vec::new(),
            next_display_index: 0,
            display_index: 0,
            scene_for: Vec::new(),
            source: None,
            direct_video_output: None,
            video_width: 0,
            video_height: 0,
            last_frame_id: 0,
            egui_painter: SdlEguiPainter::default(),
            cached_primitives: Vec::new(),
            cached_pixels_per_point: 1.0,
            cached_textures_delta: egui::TexturesDelta::default(),
        })
    }

    /// Where the video quad lands on screen - accounts for letterboxing, see `fit_rect`.
    pub fn video_rect(&self) -> sdl2::rect::Rect {
        Self::fit_rect(self.video_width, self.video_height, WIDTH, HEIGHT)
    }

    fn alloc_display_textures(&mut self, layout: crate::streaming::video::YuvLayout) -> Result<()> {
        let info = self.canvas.info();
        anyhow::ensure!(info.name == "VITA gxm" && info.texture_formats.contains(&PixelFormatEnum::NV12), "native Vita GXM NV12 support required");
        // Set once: every per-frame `SDL_SetYUVConversionMode` call was
        // redundant work on the upload hot path.
        unsafe {
            use sdl2::sys::*;
            SDL_SetYUVConversionMode(SDL_YUV_CONVERSION_MODE::SDL_YUV_CONVERSION_BT709);
        }
        let mut textures = Vec::with_capacity(DISPLAY_TEXTURE_COUNT);
        for _ in 0..DISPLAY_TEXTURE_COUNT {
            textures.push(
                self.canvas
                    .create_texture_streaming(PixelFormatEnum::NV12, layout.width, layout.height)
                    .map_err(anyhow::Error::msg)
                    .context("failed to create SDL NV12 display texture")?,
            );
        }
        self.display_textures = textures;
        self.scene_for = vec![None; DISPLAY_TEXTURE_COUNT];
        self.next_display_index = 0;
        self.display_index = 0;
        Ok(())
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
        if self.display_textures.is_empty() {
            self.alloc_display_textures(layout)?;
        }
        let metrics = &crate::streaming::video::metrics::METRICS;
        let target = self.next_display_index;
        // Only wait for the scene that sampled the texture we are about to
        // reuse. With 3 rotating textures that scene is 3 presented frames
        // old, so GPU work overlaps CPU tessellation instead of blocking it.
        let start = Instant::now();
        if let Some(scene) = self.scene_for[target].take() {
            unsafe { sceGxmFinish(scene) };
        }
        metrics.gpu_wait_us.store(start.elapsed().as_micros() as u64, Ordering::Relaxed);
        let texture = &self.display_textures[target];
        // DMA upload: the staging slots live in uncached CDRAM (the HW
        // decoder needs NC memory). A CPU memcpy from NC memory runs at
        // ~69 MB/s (~20ms for 720p NV12, measured). sceDmacMemcpy runs on
        // the DMA controller and bypasses the CPU caches entirely.
        // Y plane: width*height; UV plane: width*height/2 at uv_offset.
        let layout_copy = layout;
        let start = Instant::now();
        let upload = output.with_frame(frame, |base| unsafe {
            use sdl2::sys::*;
            use std::os::raw::c_void;
            // Lock the streaming texture to get its Y pointer (UV at +w*h).
            let mut pixels: *mut c_void = std::ptr::null_mut();
            let mut pitch: i32 = 0;
            let lock_ret = SDL_LockTexture(texture.raw(), std::ptr::null(), &mut pixels, &mut pitch);
            if lock_ret != 0 {
                return lock_ret;
            }
            if pixels.is_null() {
                let _ = SDL_UnlockTexture(texture.raw());
                return -1;
            }
            let y_size = layout_copy.width as u32 * layout_copy.height as u32;
            let uv_size = y_size / 2;
            let tex_y = pixels as *mut u8;
            let tex_uv = tex_y.add(y_size as usize);
            let dmac_ret_y = vitasdk_sys::sceDmacMemcpy(
                tex_y as *mut c_void,
                base as *const c_void,
                y_size,
            );
            if dmac_ret_y < 0 {
                let _ = SDL_UnlockTexture(texture.raw());
                return dmac_ret_y;
            }
            let dmac_ret_uv = vitasdk_sys::sceDmacMemcpy(
                tex_uv as *mut c_void,
                base.add(layout_copy.uv_offset as usize) as *const c_void,
                uv_size,
            );
            let _ = SDL_UnlockTexture(texture.raw());
            if dmac_ret_uv < 0 {
                return dmac_ret_uv;
            }
            0
        });
        if let Some(result) = upload {
            anyhow::ensure!(result == 0, "NV12 upload failed: {}", sdl2::get_error());
            let now = Instant::now();
            metrics.upload_us.store(now.duration_since(start).as_micros() as u64, Ordering::Relaxed);
            if let Some(handed_off_at) = frame.handed_off_at {
                metrics.handoff_to_upload_max_us.fetch_max(
                    now.duration_since(handed_off_at).as_micros() as u64,
                    Ordering::Relaxed,
                );
            }
            let [x, y, w, h] = frame.visible;
            self.source = Some(sdl2::rect::Rect::new(x as i32, y as i32, w, h));
            self.video_width = w;
            self.video_height = h;
            self.display_index = target;
            // Advance on success only: if the staging slot was recycled
            // underneath us (`with_frame` returned None) we retry the same
            // frame_id next loop instead of silently dropping it. Count the
            // recycled-slot case so a silent drop can't masquerade as slow
            // rendering.
            self.last_frame_id = frame_id;
            self.next_display_index = (target + 1) % DISPLAY_TEXTURE_COUNT;
            metrics.presented.fetch_add(1, Ordering::Relaxed);
        } else {
            metrics.slot_dropped.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    fn finish_all_scenes(&mut self) {
        for scene in self.scene_for.iter_mut() {
            if let Some(context) = scene.take() {
                unsafe { sceGxmFinish(context) };
            }
        }
    }

    fn detach_video(&mut self) {
        self.finish_all_scenes();
        for texture in self.display_textures.drain(..) {
            unsafe { texture.destroy() };
        }
        self.scene_for.clear();
        self.next_display_index = 0;
        self.display_index = 0;
        self.direct_video_output = None;
        self.source = None;
        self.video_width = 0;
        self.video_height = 0;
        self.last_frame_id = 0;
        self.cached_primitives.clear();
    }

    pub fn draw_scene(&mut self, show_video: bool) -> Result<()> {
        SCENE_CAPTURE.with(|capture| capture.set(Some(std::ptr::null_mut())));
        self.canvas.set_draw_color(sdl2::pixels::Color::BLACK);
        self.canvas.clear();

        if show_video
            && let Some(source) = self.source
            && let Some(texture) = self.display_textures.get(self.display_index)
        {
            let destination = self.video_rect();
            self.canvas
                .copy(texture, source, destination)
                .map_err(anyhow::Error::msg)
                .context("failed to draw SDL YUV video frame")?;
        }

        Ok(())
    }

    /// Video-only present used while streaming with minimal overlays.
    /// `draw_scene` arms the GXM scene capture; `present` swaps the
    /// framebuffer and records which display texture the GPU last sampled,
    /// so the next reuse of that texture waits on the right scene.
    /// The cached overlay geometry is repainted on top (no egui rebuild),
    /// so the debug chip stays visible instead of strobing at 10Hz.
    pub fn present_video_only(&mut self, show_video: bool) -> Result<()> {
        self.draw_scene(show_video)?;
        if !self.cached_primitives.is_empty() {
            // Clone so the painter borrow doesn't alias the cache.
            let primitives = self.cached_primitives.clone();
            let ppp = self.cached_pixels_per_point;
            // Textures are already uploaded; pass an empty delta.
            let delta = egui::TexturesDelta::default();
            self.egui_painter.paint(
                &mut self.canvas,
                [WIDTH, HEIGHT],
                ppp,
                &primitives,
                &delta,
            )?;
        }
        let start = Instant::now();
        self.canvas.present();
        crate::streaming::video::metrics::METRICS
            .present_us
            .store(start.elapsed().as_micros() as u64, Ordering::Relaxed);
        let context = SCENE_CAPTURE.with(|capture| capture.take()).filter(|context| !context.is_null());
        if self.source.is_some() {
            if let Some(context) = context {
                self.scene_for[self.display_index] = Some(context);
            }
            anyhow::ensure!(
                self.scene_for.iter().any(|scene| scene.is_some()),
                "missing GXM completion context after NV12 draw"
            );
        }
        Ok(())
    }

    pub fn paint_egui(
        &mut self,
        pixels_per_point: f32,
        primitives: &[egui::ClippedPrimitive],
        textures_delta: &egui::TexturesDelta,
    ) -> Result<()> {
        // Cache for the video-only replay path.
        self.cached_primitives.clear();
        self.cached_primitives.extend(primitives.iter().cloned());
        self.cached_pixels_per_point = pixels_per_point;
        // Textures were already uploaded by the painter below; keep an
        // empty delta so replays don't re-upload (and don't free).
        self.cached_textures_delta = egui::TexturesDelta::default();
        self.egui_painter.paint(
            &mut self.canvas,
            [WIDTH, HEIGHT],
            pixels_per_point,
            primitives,
            textures_delta,
        )?;
        let start = Instant::now();
        self.canvas.present();
        crate::streaming::video::metrics::METRICS
            .present_us
            .store(start.elapsed().as_micros() as u64, Ordering::Relaxed);
        // SDL may end more than one GXM scene per present on Vita; the wrap
        // hook captures the last one. Record which display texture that
        // scene sampled so the next reuse of *that* texture waits on it —
        // the other in-flight scenes are left alone.
        let context = SCENE_CAPTURE.with(|capture| capture.take()).filter(|context| !context.is_null());
        if self.source.is_some() {
            if let Some(context) = context {
                self.scene_for[self.display_index] = Some(context);
            }
            anyhow::ensure!(
                self.scene_for.iter().any(|scene| scene.is_some()),
                "missing GXM completion context after NV12 draw"
            );
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
