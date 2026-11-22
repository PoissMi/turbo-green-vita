use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) struct VideoMetrics {
    pub(crate) rtp_assembly_sum_us: AtomicU64,
    pub(crate) rtp_assembly_count: AtomicU64,
    pub(crate) rtp_assembly_max_us: AtomicU64,
    pub(crate) upload_us: AtomicU64,
    pub(crate) gpu_wait_us: AtomicU64,
    /// Wall-clock time of `canvas.present()` — the vsync-blocking swap.
    pub(crate) present_us: AtomicU64,
    /// Time from loop start to `present()` returning (CPU work + blocking present).
    pub(crate) loop_us: AtomicU64,
    pub(crate) decode_us: AtomicU64,
    pub(crate) pipeline_age_us: AtomicU64,
    pub(crate) decoded: AtomicU64,
    pub(crate) skipped: AtomicU64,
    pub(crate) presented: AtomicU64,
    pub(crate) replaced: AtomicU64,
    pub(crate) handoffs: AtomicU64,
    pub(crate) ui_overwrites: AtomicU64,
    pub(crate) decode_to_handoff_max_us: AtomicU64,
    pub(crate) handoff_to_upload_max_us: AtomicU64,
    /// End-to-end age of the frame being displayed: first RTP packet arrival
    /// to upload. This is the number that describes perceived latency.
    pub(crate) frame_age_max_us: AtomicU64,
    pub(crate) frame_age_sum_us: AtomicU64,
    pub(crate) frame_age_count: AtomicU64,
    /// How long a sampled gamepad state waited before it was put on the wire.
    pub(crate) input_lag_max_us: AtomicU64,
    pub(crate) input_lag_sum_us: AtomicU64,
    pub(crate) input_lag_count: AtomicU64,
    /// Keyframes requested from the server.
    pub(crate) keyframes_requested: AtomicU64,
    pub(crate) queue_full: AtomicU64,
    /// Frames that never made it to `sync_video_frame`'s upload because the
    /// staging slot was recycled underneath us (`with_frame` returned None).
    pub(crate) slot_dropped: AtomicU64,
    pub(crate) resyncs: AtomicU64,
    pub(crate) resets: AtomicU64,
}

pub(crate) static METRICS: VideoMetrics = VideoMetrics {
    rtp_assembly_sum_us: AtomicU64::new(0),
    rtp_assembly_count: AtomicU64::new(0),
    rtp_assembly_max_us: AtomicU64::new(0),
    upload_us: AtomicU64::new(0),
    gpu_wait_us: AtomicU64::new(0),
    present_us: AtomicU64::new(0),
    loop_us: AtomicU64::new(0),
    decode_us: AtomicU64::new(0),
    pipeline_age_us: AtomicU64::new(0),
    decoded: AtomicU64::new(0),
    skipped: AtomicU64::new(0),
    presented: AtomicU64::new(0),
    replaced: AtomicU64::new(0),
    handoffs: AtomicU64::new(0),
    ui_overwrites: AtomicU64::new(0),
    decode_to_handoff_max_us: AtomicU64::new(0),
    handoff_to_upload_max_us: AtomicU64::new(0),
    frame_age_max_us: AtomicU64::new(0),
    frame_age_sum_us: AtomicU64::new(0),
    frame_age_count: AtomicU64::new(0),
    input_lag_max_us: AtomicU64::new(0),
    input_lag_sum_us: AtomicU64::new(0),
    input_lag_count: AtomicU64::new(0),
    keyframes_requested: AtomicU64::new(0),
    queue_full: AtomicU64::new(0),
    slot_dropped: AtomicU64::new(0),
    resyncs: AtomicU64::new(0),
    resets: AtomicU64::new(0),
};

pub fn video_performance_summary() -> String {
    let rtp_sum = METRICS.rtp_assembly_sum_us.swap(0, Ordering::Relaxed);
    let rtp_count = METRICS.rtp_assembly_count.swap(0, Ordering::Relaxed);
    let rtp_average = rtp_sum.checked_div(rtp_count).unwrap_or(0);
    let rtp_max = METRICS.rtp_assembly_max_us.swap(0, Ordering::Relaxed);
    let age_average = METRICS
        .frame_age_sum_us
        .swap(0, Ordering::Relaxed)
        .checked_div(METRICS.frame_age_count.swap(0, Ordering::Relaxed))
        .unwrap_or(0);
    let input_lag_average = METRICS
        .input_lag_sum_us
        .swap(0, Ordering::Relaxed)
        .checked_div(METRICS.input_lag_count.swap(0, Ordering::Relaxed))
        .unwrap_or(0);
    format!(
        "fps d/p:{}/{} us r:{rtp_average}/{rtp_max} d/a:{}/{} up/gpu:{}/{} pr/loop:{}/{} skip:{} repl:{} q:{} slot:{} rs:{} rst:{}\nhandoff:{} ui-ovr:{} max d>h/h>u:{}/{}us age:{age_average}/{}us in-lag:{input_lag_average}/{}us kf:{}",
        METRICS.decoded.swap(0, Ordering::Relaxed),
        METRICS.presented.swap(0, Ordering::Relaxed),
        METRICS.decode_us.load(Ordering::Relaxed),
        METRICS.pipeline_age_us.load(Ordering::Relaxed),
        METRICS.upload_us.load(Ordering::Relaxed),
        METRICS.gpu_wait_us.load(Ordering::Relaxed),
        METRICS.present_us.load(Ordering::Relaxed),
        METRICS.loop_us.load(Ordering::Relaxed),
        METRICS.skipped.swap(0, Ordering::Relaxed),
        METRICS.replaced.swap(0, Ordering::Relaxed),
        METRICS.queue_full.swap(0, Ordering::Relaxed),
        METRICS.slot_dropped.swap(0, Ordering::Relaxed),
        METRICS.resyncs.load(Ordering::Relaxed),
        METRICS.resets.load(Ordering::Relaxed),
        METRICS.handoffs.swap(0, Ordering::Relaxed),
        METRICS.ui_overwrites.swap(0, Ordering::Relaxed),
        METRICS.decode_to_handoff_max_us.swap(0, Ordering::Relaxed),
        METRICS.handoff_to_upload_max_us.swap(0, Ordering::Relaxed),
        METRICS.frame_age_max_us.swap(0, Ordering::Relaxed),
        METRICS.input_lag_max_us.swap(0, Ordering::Relaxed),
        METRICS.keyframes_requested.swap(0, Ordering::Relaxed),
    )
}
