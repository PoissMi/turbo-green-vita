mod decoder;
mod memory;
pub(crate) mod metrics;
mod worker;

pub const STREAM_WIDTH: u32 = 1280;
pub const STREAM_HEIGHT: u32 = 720;
pub const HW_OUTPUT_WIDTH: u32 = 960;
pub const HW_OUTPUT_HEIGHT: u32 = 544;

pub use memory::reserve_decoder_cdram;
pub use metrics::video_performance_summary;
pub use worker::VideoDecodeWorker;

use std::sync::atomic::AtomicBool;
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::Duration;

// With three textures a free slot normally exists (displayed and pending are always distinct),
// so this wait only fires if that invariant is ever broken; it caps how long decode can stall.
const MAX_PENDING_TEXTURE_WAIT: Duration = Duration::from_millis(67);

#[derive(Clone, Copy)]
pub(crate) struct VideoTextureTarget {
    pub(crate) ptr: usize,
    pub(crate) pitch: u32,
    pub(crate) capacity: u32,
}

struct DirectVideoOutputState {
    targets: Option<[VideoTextureTarget; 3]>,
    displayed: Option<usize>,
    // SDL present is async, so the previously displayed texture may still be scanned out;
    // keep it out of rotation until one more frame has been picked up.
    prev_displayed: Option<usize>,
    pending: Option<(usize, u64)>,
    next_generation: u64,
}

/// Synchronizes the decoder thread with the three SDL/GXM textures owned by the render thread.
/// Pointers are stored as integers so the platform-specific unsafe boundary stays in the code
/// that registers and consumes the textures.
pub(crate) struct DirectVideoOutput {
    state: Mutex<DirectVideoOutputState>,
    frame_displayed: Condvar,
    pub(crate) decoder_ready: AtomicBool,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

impl DirectVideoOutput {
    pub(crate) fn new(width: u32, height: u32) -> Self {
        Self {
            state: Mutex::new(DirectVideoOutputState {
                targets: None,
                displayed: None,
                prev_displayed: None,
                pending: None,
                next_generation: 0,
            }),
            frame_displayed: Condvar::new(),
            decoder_ready: AtomicBool::new(false),
            width,
            height,
        }
    }

    pub(crate) fn set_targets(&self, targets: [VideoTextureTarget; 3]) {
        if let Ok(mut state) = self.state.lock() {
            state.targets = Some(targets);
            state.displayed = None;
            state.prev_displayed = None;
            state.pending = None;
        }
    }

    pub(crate) fn clear_targets(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.targets = None;
            state.displayed = None;
            state.prev_displayed = None;
            state.pending = None;
        }
        self.frame_displayed.notify_all();
    }

    pub(crate) fn mark_displayed(&self, index: usize, generation: u64) {
        let mut cleared_pending = false;
        if let Ok(mut state) = self.state.lock() {
            if state.displayed != Some(index) {
                state.prev_displayed = state.displayed;
            }
            state.displayed = Some(index);
            if state.pending == Some((index, generation)) {
                state.pending = None;
                cleared_pending = true;
            }
        }
        if cleared_pending {
            self.frame_displayed.notify_one();
        }
    }

    pub(super) fn lock_decode_target(&self) -> Option<DirectVideoTargetGuard<'_>> {
        let mut state = self.state.lock().ok()?;
        if state.targets.is_some() && Self::free_slot(&state).is_none() {
            let (waited_state, _) = self
                .frame_displayed
                .wait_timeout_while(state, MAX_PENDING_TEXTURE_WAIT, |state| {
                    state.targets.is_some() && Self::free_slot(state).is_none()
                })
                .ok()?;
            state = waited_state;
        }
        let targets = state.targets?;
        // Never touch the texture being scanned out; if every slot is busy past the wait,
        // overwrite the pending (unshown) frame rather than stalling the decoder.
        let index = Self::free_slot(&state)
            .or(state.pending.map(|(index, _)| index))
            .unwrap_or(0);
        Some(DirectVideoTargetGuard {
            state,
            target: targets[index],
            index,
        })
    }

    fn free_slot(state: &DirectVideoOutputState) -> Option<usize> {
        let targets = state.targets.as_ref()?;
        let pending_index = state.pending.map(|(index, _)| index);
        (0..targets.len()).find(|&index| {
            state.displayed != Some(index)
                && state.prev_displayed != Some(index)
                && pending_index != Some(index)
        })
    }
}

pub(super) struct DirectVideoTargetGuard<'a> {
    state: MutexGuard<'a, DirectVideoOutputState>,
    target: VideoTextureTarget,
    index: usize,
}

impl DirectVideoTargetGuard<'_> {
    pub(super) fn publish(mut self) -> (usize, u64) {
        self.state.next_generation = self.state.next_generation.wrapping_add(1);
        let generation = self.state.next_generation;
        self.state.pending = Some((self.index, generation));
        (self.index, generation)
    }
}

pub struct DecodedFrame {
    pub texture_index: usize,
    pub generation: u64,
}

#[derive(Clone, Copy)]
pub struct DecoderConfig {
    pub decode_width: u32,
    pub decode_height: u32,
    pub output_width: u32,
    pub output_height: u32,
}
