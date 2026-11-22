mod decoder;
mod memory;
pub(crate) mod metrics;
mod worker;

pub const STREAM_WIDTH: u32 = 1280;
pub const STREAM_HEIGHT: u32 = 720;
pub use metrics::video_performance_summary;
pub use worker::VideoDecodeWorker;

use anyhow::{Result, ensure};
use memory::DecoderBlock;
use std::sync::{Mutex, atomic::AtomicBool};

#[derive(Clone, Copy)]
pub(crate) struct YuvLayout {
    pub width: u32,
    pub height: u32,
    pub uv_offset: u32,
    pub bytes: u32,
}

impl YuvLayout {
    fn new(width: u32, height: u32) -> Result<Self> {
        ensure!(width > 0 && height > 0 && width <= STREAM_WIDTH && height <= STREAM_HEIGHT);
        let width = width.next_multiple_of(16);
        let height = height.next_multiple_of(16);
        let uv_offset = width * height;
        Ok(Self { width, height, uv_offset, bytes: uv_offset + uv_offset / 2 })
    }
}

static IDLE_OUTPUT_SLOTS: Mutex<Option<[DecoderBlock; 3]>> = Mutex::new(None);

pub(crate) fn reserve_yuv_output() -> Result<()> {
    let layout = YuvLayout::new(STREAM_WIDTH, STREAM_HEIGHT)?;
    *IDLE_OUTPUT_SLOTS.lock().unwrap() = Some([
        DecoderBlock::allocate(layout.bytes, false)?,
        DecoderBlock::allocate(layout.bytes, false)?,
        DecoderBlock::allocate(layout.bytes, false)?,
    ]);
    Ok(())
}

pub(crate) struct DirectVideoOutput {
    slots: Mutex<Option<[DecoderBlock; 3]>>,
    state: Mutex<SlotState>,
    pub(crate) decoder_uses_phycont: AtomicBool,
}

#[derive(Default)]
struct SlotState {
    next: u64,
    generations: [Option<u64>; 3],
}

impl DirectVideoOutput {
    pub(crate) fn new() -> Self {
        Self {
            slots: Mutex::new(None),
            state: Mutex::new(SlotState::default()),
            decoder_uses_phycont: AtomicBool::new(false),
        }
    }

    fn acquire_slots(&self, layout: YuvLayout) -> Result<()> {
        ensure!(layout.bytes <= YuvLayout::new(STREAM_WIDTH, STREAM_HEIGHT)?.bytes);
        let slots = IDLE_OUTPUT_SLOTS.lock().unwrap().take()
            .ok_or_else(|| anyhow::anyhow!("NV12 output buffers are still in use by the previous stream"))?;
        *self.slots.lock().unwrap() = Some(slots);
        Ok(())
    }

    fn reserve(&self) -> (usize, u64, *mut u8) {
        let mut state = self.state.lock().unwrap();
        let generation = state.next;
        let index = (generation % 3) as usize;
        state.next = generation.wrapping_add(1);
        state.generations[index] = None;
        let address = self.slots.lock().unwrap().as_ref().unwrap()[index].address;
        (index, generation, address as *mut u8)
    }

    fn publish(&self, index: usize, generation: u64) {
        self.state.lock().unwrap().generations[index] = Some(generation);
    }

    pub(crate) fn with_frame<T>(&self, frame: &DecodedFrame, upload: impl FnOnce(*const u8) -> T) -> Option<T> {
        let state = self.state.lock().unwrap();
        if state.generations[frame.index] != Some(frame.generation) {
            return None;
        }
        let slots = self.slots.lock().unwrap();
        Some(upload(slots.as_ref().unwrap()[frame.index].address as *const u8))
    }
}

impl Drop for DirectVideoOutput {
    fn drop(&mut self) {
        if let Some(slots) = self.slots.get_mut().unwrap().take() {
            *IDLE_OUTPUT_SLOTS.lock().unwrap() = Some(slots);
        }
    }
}

pub struct DecodedFrame {
    index: usize,
    generation: u64,
    pub(crate) layout: YuvLayout,
    pub(crate) visible: [u32; 4],
    pub(crate) decoded_at: std::time::Instant,
    pub(crate) handed_off_at: Option<std::time::Instant>,
    /// When the first RTP packet of this frame arrived on the wire.
    pub(crate) arrived_at: std::time::Instant,
}

#[derive(Clone, Copy)]
pub struct DecoderConfig {
    pub decode_width: u32,
    pub decode_height: u32,
}
