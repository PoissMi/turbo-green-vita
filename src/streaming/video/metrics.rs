use std::collections::VecDeque;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

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
    /// Actual display flips per window. `presented` counts uploads; this
    /// counts what the screen really showed.
    pub(crate) presents: AtomicU64,
    /// Worst gap between two display flips: the freeze detector.
    pub(crate) present_gap_max_us: AtomicU64,
    pub(crate) present_gap_sum_us: AtomicU64,
    pub(crate) present_gap_count: AtomicU64,
    /// Audio currently queued in the SDL audio device, in milliseconds.
    /// This is the audio side of the A/V desync.
    pub(crate) audio_buffer_ms: AtomicU64,
    pub(crate) audio_buffer_sum_ms: AtomicU64,
    pub(crate) audio_buffer_count: AtomicU64,
    /// Times the audio queue overflowed and had to be dropped and refilled.
    pub(crate) audio_resyncs: AtomicU64,
    /// Worst gap between two data-channel flushes. The input waits for the
    /// next flush, so this bounds the input's delay on the wire.
    pub(crate) flush_gap_max_us: AtomicU64,
    pub(crate) queue_full: AtomicU64,
    /// Frames that never made it to `sync_video_frame`'s upload because the
    /// staging slot was recycled underneath us (`with_frame` returned None).
    pub(crate) slot_dropped: AtomicU64,
    pub(crate) resyncs: AtomicU64,
    pub(crate) resets: AtomicU64,
    /// How far the server's video timeline lags our wall clock, in
    /// microseconds. This is the delay the client cannot see on its own: the
    /// frame age is measured from the moment the packet leaves the socket
    /// buffer, so a queue that grows in front of the client (the server's
    /// send path or the network) makes every frame *older* while all the
    /// client-side numbers stay perfect.
    pub(crate) server_offset_us: AtomicI64,
    /// How much that offset grew over the last minute. A steady positive
    /// value means the video is arriving later and later: the picture is
    /// lagging behind reality more and more.
    pub(crate) server_drift_us: AtomicI64,
    /// The bitrate we currently ask the server for, in kbps. The user's
    /// setting is the ceiling; this is what we believe the path can carry.
    pub(crate) bandwidth_estimate_kbps: AtomicU64,
    /// Packets drained from the socket per receive call. A rising batch size
    /// means packets are piling up in the client's own receive buffer.
    pub(crate) rx_batch_sum: AtomicU64,
    pub(crate) rx_batch_max: AtomicU64,
    pub(crate) rx_batch_count: AtomicU64,
    /// When the last input report was handed to the data channel. The next
    /// outgoing packet carries it, so the gap between the two is how long
    /// the report waited for the wire.
    pub(crate) input_queued_at: Mutex<Option<Instant>>,
    pub(crate) input_wire_sum_us: AtomicU64,
    pub(crate) input_wire_max_us: AtomicU64,
    pub(crate) input_wire_count: AtomicU64,
    /// Input reports handed to the data channel.
    pub(crate) input_sent: AtomicU64,
    /// How far the video's game tick has fallen behind the audio's, in
    /// microseconds. The audio is mixed from the game clock and the video is
    /// captured from it, so comparing the two streams at the same moment on
    /// our clock shows the video arriving from an older point in the game.
    pub(crate) av_lag_us: AtomicI64,
    /// How much that grew over the last minute.
    pub(crate) av_lag_drift_us: AtomicI64,
    /// Pairs of contemporaneous video and audio packets compared so far.
    pub(crate) av_pairs: AtomicU64,
}

/// The latest tick of each stream, and the history used to measure how far
/// the video falls behind the audio.
struct AvSync {
    video: Option<(f64, Instant)>,
    audio: Option<(f64, Instant)>,
    /// The tick difference at the start of the session. The two streams have
    /// their own timestamp origins, so only a change from this point is
    /// meaningful.
    baseline: Option<f64>,
    samples: VecDeque<(Instant, f64)>,
}

static AV_SYNC: Mutex<AvSync> = Mutex::new(AvSync {
    video: None,
    audio: None,
    baseline: None,
    samples: VecDeque::new(),
});

/// Records the game tick of a video frame and compares it with the audio.
pub(crate) fn observe_video_tick(timestamp_seconds: f64, arrived_at: Instant) {
    let mut sync = match AV_SYNC.lock() {
        Ok(sync) => sync,
        Err(_) => return,
    };
    sync.video = Some((timestamp_seconds, arrived_at));
    update_av_lag(&mut sync);
}

/// Records the game tick of an audio packet.
pub(crate) fn observe_audio_tick(timestamp_seconds: f64, arrived_at: Instant) {
    let mut sync = match AV_SYNC.lock() {
        Ok(sync) => sync,
        Err(_) => return,
    };
    sync.audio = Some((timestamp_seconds, arrived_at));
}

fn update_av_lag(sync: &mut AvSync) {
    let (Some((video_ts, video_at)), Some((audio_ts, audio_at))) = (sync.video, sync.audio) else {
        return;
    };
    // The two samples have to be contemporaneous: the video frame and the
    // audio packet that reached us at the same moment were produced by the
    // same game tick.
    if video_at.duration_since(audio_at) > Duration::from_millis(120) {
        return;
    }
    let skew = video_ts - audio_ts;
    let Some(baseline) = sync.baseline else {
        sync.baseline = Some(skew);
        return;
    };
    // A video that arrives from an older point in the game has a smaller
    // tick than the audio that arrived beside it.
    METRICS.av_lag_us.store(((baseline - skew) * 1_000_000.0) as i64, Ordering::Relaxed);
    METRICS.av_pairs.fetch_add(1, Ordering::Relaxed);

    let due = sync
        .samples
        .back()
        .is_none_or(|(last_at, _)| video_at.duration_since(*last_at) >= Duration::from_secs(1));
    if !due {
        return;
    }
    sync.samples.push_back((video_at, skew));
    while sync.samples.len() > 130 {
        sync.samples.pop_front();
    }
    // How much the lag grew over the last minute.
    if let Some((_, old_skew)) = sync
        .samples
        .iter()
        .find(|(at, _)| video_at.duration_since(*at) >= Duration::from_secs(58))
    {
        METRICS
            .av_lag_drift_us
            .store(((old_skew - skew) * 1_000_000.0) as i64, Ordering::Relaxed);
    }
}

/// Records that an input report was queued for the data channel.
pub(crate) fn mark_input_queued(at: Instant) {
    if let Ok(mut slot) = METRICS.input_queued_at.lock() {
        *slot = Some(at);
    }
}

/// Takes the timestamp of the queued input report, if one is waiting.
pub(crate) fn take_input_queued() -> Option<Instant> {
    METRICS
        .input_queued_at
        .lock()
        .ok()
        .and_then(|mut slot| slot.take())
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
    presents: AtomicU64::new(0),
    present_gap_max_us: AtomicU64::new(0),
    present_gap_sum_us: AtomicU64::new(0),
    present_gap_count: AtomicU64::new(0),
    audio_buffer_ms: AtomicU64::new(0),
    audio_buffer_sum_ms: AtomicU64::new(0),
    audio_buffer_count: AtomicU64::new(0),
    audio_resyncs: AtomicU64::new(0),
    flush_gap_max_us: AtomicU64::new(0),
    queue_full: AtomicU64::new(0),
    slot_dropped: AtomicU64::new(0),
    resyncs: AtomicU64::new(0),
    resets: AtomicU64::new(0),
    server_offset_us: AtomicI64::new(0),
    server_drift_us: AtomicI64::new(0),
    bandwidth_estimate_kbps: AtomicU64::new(0),
    rx_batch_sum: AtomicU64::new(0),
    rx_batch_max: AtomicU64::new(0),
    rx_batch_count: AtomicU64::new(0),
    input_queued_at: Mutex::new(None),
    input_wire_sum_us: AtomicU64::new(0),
    input_wire_max_us: AtomicU64::new(0),
    input_wire_count: AtomicU64::new(0),
    input_sent: AtomicU64::new(0),
    av_lag_us: AtomicI64::new(0),
    av_lag_drift_us: AtomicI64::new(0),
    av_pairs: AtomicU64::new(0),
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
    let present_gap_average = METRICS
        .present_gap_sum_us
        .swap(0, Ordering::Relaxed)
        .checked_div(METRICS.present_gap_count.swap(0, Ordering::Relaxed))
        .unwrap_or(0);
    let audio_buffer_average = METRICS
        .audio_buffer_sum_ms
        .swap(0, Ordering::Relaxed)
        .checked_div(METRICS.audio_buffer_count.swap(0, Ordering::Relaxed))
        .unwrap_or(0);
    let rx_batch_average = METRICS
        .rx_batch_sum
        .swap(0, Ordering::Relaxed)
        .checked_div(METRICS.rx_batch_count.swap(0, Ordering::Relaxed))
        .unwrap_or(0);
    let server_offset_ms = METRICS.server_offset_us.load(Ordering::Relaxed) / 1000;
    let server_drift_ms = METRICS.server_drift_us.load(Ordering::Relaxed) / 1000;
    let input_wire_average = METRICS
        .input_wire_sum_us
        .swap(0, Ordering::Relaxed)
        .checked_div(METRICS.input_wire_count.swap(0, Ordering::Relaxed))
        .unwrap_or(0);
    format!(
        "fps d/p:{}/{} us r:{rtp_average}/{rtp_max} d/a:{}/{} up/gpu:{}/{} pr/loop:{}/{} skip:{} repl:{} q:{} slot:{} rs:{} rst:{}\nhandoff:{} ui-ovr:{} max d>h/h>u:{}/{}us age:{age_average}/{}us in-lag:{input_lag_average}/{}us kf:{} pres:{} gap:{present_gap_average}/{}us aud:{audio_buffer_average}/{}ms ars:{} flush-gap:{}us\nsrv:{server_offset_ms}ms drift:{server_drift_ms}ms remb:{}kbps rx-b:{}/{} in-wire:{}/{}us in-rate:{} av-lag:{}ms av-drift:{}ms",
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
        METRICS.presents.swap(0, Ordering::Relaxed),
        METRICS.present_gap_max_us.swap(0, Ordering::Relaxed),
        METRICS.audio_buffer_ms.swap(0, Ordering::Relaxed),
        METRICS.audio_resyncs.swap(0, Ordering::Relaxed),
        METRICS.flush_gap_max_us.swap(0, Ordering::Relaxed),
        METRICS.bandwidth_estimate_kbps.load(Ordering::Relaxed),
        rx_batch_average,
        METRICS.rx_batch_max.swap(0, Ordering::Relaxed),
        input_wire_average,
        METRICS.input_wire_max_us.swap(0, Ordering::Relaxed),
        METRICS.input_sent.swap(0, Ordering::Relaxed),
        METRICS.av_lag_us.load(Ordering::Relaxed) / 1000,
        METRICS.av_lag_drift_us.load(Ordering::Relaxed) / 1000,
    )
}
