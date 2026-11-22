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
    /// microseconds, counted from the closest alignment the two streams
    /// ever showed. Positive means the picture is behind the sound.
    ///
    /// The two streams carry their own RTP timestamp origins, so their raw
    /// difference is the delay plus a constant this client cannot know.
    /// Measuring against the closest alignment the session ever reached
    /// removes that constant and leaves the part that matters: how much
    /// further the picture has fallen behind since it was closest.
    pub(crate) av_lag_us: AtomicI64,
    /// How much that grew over the last minute.
    pub(crate) av_lag_drift_us: AtomicI64,
    /// The raw difference between the two streams when the session started.
    ///
    /// A small value means the two streams share a timestamp origin, and
    /// then the lag above is the real, absolute delay. A huge value means
    /// the origins are unrelated and only the growth above can be trusted.
    pub(crate) av_origin_us: AtomicI64,
    /// Pairs of video and audio packets compared so far.
    pub(crate) av_pairs: AtomicU64,
    /// The same comparison, but at the output: the audio as it is heard
    /// against the video as it is seen.
    ///
    /// The arrival-based number above cannot see the client losing audio
    /// time, because a lost packet does not change the timestamps - it only
    /// shortens the timeline, and the sound then runs ahead of the picture
    /// by exactly that much. This one counts the audio still sitting in the
    /// device queue, so the difference between the two is the client's own
    /// audio error.
    pub(crate) av_playout_us: AtomicI64,
    /// The span of audio the server sent, in samples.
    pub(crate) audio_sent_samples: AtomicU64,
    /// The samples decoded so far.
    pub(crate) audio_decoded_samples: AtomicU64,
    /// The samples that reached the device so far.
    pub(crate) audio_played_samples: AtomicU64,
    /// The samples still sitting in the device queue.
    pub(crate) audio_queued_samples: AtomicU64,
    /// The samples that were decoded but have not reached the device yet.
    pub(crate) audio_pipeline_samples: AtomicU64,
    /// The audio waiting in the jitter buffer, in samples.
    pub(crate) audio_jitter_samples: AtomicU64,
    /// How much audio time the client has lost, in milliseconds.
    ///
    /// The audio the server sent minus the audio the client actually
    /// decoded. Every millisecond here is a hole in the audio timeline:
    /// the sound that follows the hole plays early, and the picture is then
    /// that much further behind what the player hears.
    pub(crate) audio_lost_ms: AtomicI64,
    /// Audio that had to be thrown away because the pipeline could not hold
    /// it. Should stay at zero.
    pub(crate) audio_dropped_packets: AtomicU64,
}

/// The latest tick of each stream, and the history used to measure how far
/// the video falls behind the audio.
struct AvSync {
    video: Option<(f64, Instant)>,
    audio: Option<(f64, Instant)>,
    /// The audio ticks that arrived, oldest first. The video frame is
    /// compared with the audio that had arrived when the frame arrived,
    /// not with the newest audio - otherwise a late picture is compared
    /// with sound that was sent after it and the delay is doubled.
    audio_history: VecDeque<(Instant, f64)>,
    /// The smallest raw difference between the two streams seen so far.
    ///
    /// This is the best estimate of the constant the two timestamp origins
    /// differ by: the moment when the picture was closest to the sound.
    best_skew: Option<f64>,
    /// The same for the output side.
    best_output_skew: Option<f64>,
    /// The very first raw difference, which shows whether the two streams
    /// share a timestamp origin at all.
    first_skew: Option<f64>,
    samples: VecDeque<(Instant, f64)>,
    /// The first and newest audio timestamp, used to measure how much audio
    /// the server sent.
    first_audio_ts: Option<u32>,
    newest_audio_ts: Option<u32>,
}

static AV_SYNC: Mutex<AvSync> = Mutex::new(AvSync {
    video: None,
    audio: None,
    audio_history: VecDeque::new(),
    best_skew: None,
    best_output_skew: None,
    first_skew: None,
    samples: VecDeque::new(),
    first_audio_ts: None,
    newest_audio_ts: None,
});

/// Records the game tick of a video frame and compares it with the audio.
pub(crate) fn observe_video_tick(timestamp: u32, clock_rate: u32, arrived_at: Instant) {
    let mut sync = match AV_SYNC.lock() {
        Ok(sync) => sync,
        Err(_) => return,
    };
    sync.video = Some((timestamp as f64 / f64::from(clock_rate), arrived_at));
    update_av_lag(&mut sync);
}

/// Records the game tick of an audio packet.
pub(crate) fn observe_audio_tick(timestamp: u32, clock_rate: u32, arrived_at: Instant) {
    let mut sync = match AV_SYNC.lock() {
        Ok(sync) => sync,
        Err(_) => return,
    };
    let tick = timestamp as f64 / f64::from(clock_rate);
    sync.audio = Some((tick, arrived_at));
    sync.audio_history.push_back((arrived_at, tick));
    // Two seconds of audio is enough to compare any frame that is still in
    // flight, and short enough to stay cheap.
    while sync.audio_history.len() > 160 {
        sync.audio_history.pop_front();
    }
    let first = *sync.first_audio_ts.get_or_insert(timestamp);
    sync.newest_audio_ts = Some(timestamp);
    // The span of audio the server sent, in samples. Compared with the audio
    // the client decoded, this is what tells us whether the client is
    // losing audio time.
    METRICS.audio_sent_samples.store(
        timestamp.wrapping_sub(first) as u64,
        Ordering::Relaxed,
    );
    update_audio_lost();
}

/// Reports the audio still waiting in the device queue.
pub(crate) fn observe_audio_queue(samples: u64) {
    METRICS.audio_queued_samples.store(samples, Ordering::Relaxed);
}

/// How much audio time the client lost: the audio the server sent minus the
/// audio the client decoded, ignoring the audio that is still on its way.
///
/// A hole in the audio timeline is the one thing the client can do to make
/// the picture look late without anything being late: the sound after the
/// hole plays early, and the picture is suddenly that much further behind
/// what the player hears.
fn update_audio_lost() {
    let sent = METRICS.audio_sent_samples.load(Ordering::Relaxed);
    let decoded = METRICS.audio_decoded_samples.load(Ordering::Relaxed);
    let jitter = METRICS.audio_jitter_samples.load(Ordering::Relaxed);
    if sent == 0 {
        return;
    }
    let lost = sent.saturating_sub(decoded).saturating_sub(jitter);
    METRICS
        .audio_lost_ms
        .store(((lost * 1_000) / 48_000) as i64, Ordering::Relaxed);
}

/// The audio tick that had arrived at `at`.
///
/// The video frame and the audio that was playing beside it come from the
/// same game moment, so the audio that had already arrived when the frame
/// arrived is the one to compare it with.
fn audio_tick_at(sync: &AvSync, at: Instant) -> Option<f64> {
    sync.audio_history
        .iter()
        .rev()
        .find(|(arrived_at, _)| *arrived_at <= at)
        .map(|(_, tick)| *tick)
}

/// The audio as it is heard against the video as it is seen.
fn update_playout_lag(sync: &mut AvSync) {
    let Some((video_ts, video_at)) = sync.video else {
        return;
    };
    let Some(audio_ts) = audio_tick_at(sync, video_at) else {
        return;
    };
    // The audio in the device queue and the audio waiting to be decoded
    // have not been heard yet, so they are not part of what the player
    // hears while this frame is on the screen.
    let queued = METRICS.audio_queued_samples.load(Ordering::Relaxed);
    let pipeline = METRICS.audio_pipeline_samples.load(Ordering::Relaxed);
    let unheard = (queued + pipeline) as f64 / 48_000.0;
    let skew = video_ts - (audio_ts - unheard);
    let best = *sync.best_output_skew.get_or_insert(skew);
    sync.best_output_skew = Some(best.min(skew));
    METRICS
        .av_playout_us
        .store(((best - skew) * 1_000_000.0) as i64, Ordering::Relaxed);
}

fn update_av_lag(sync: &mut AvSync) {
    // The output comparison comes first: it only needs both streams to
    // exist, not to be contemporaneous.
    update_playout_lag(sync);
    let Some((video_ts, video_at)) = sync.video else {
        return;
    };
    let Some(audio_ts) = audio_tick_at(sync, video_at) else {
        return;
    };
    // There is deliberately no time window here.
    //
    // The previous version only compared the video with audio that had
    // arrived within 120 ms of it. That is exactly the comparison that
    // stops happening when the picture is late: a frame that arrives half
    // a second late arrives after the sound has moved on, so every sample
    // was thrown away and the HUD kept showing the last value from before
    // the lag started. The reading looked healthy because nothing was
    // measured at all.
    let skew = video_ts - audio_ts;
    if sync.first_skew.is_none() {
        sync.first_skew = Some(skew);
        METRICS
            .av_origin_us
            .store((skew * 1_000_000.0) as i64, Ordering::Relaxed);
    }
    let best = *sync.best_skew.get_or_insert(skew);
    sync.best_skew = Some(best.min(skew));
    // How far the picture has fallen behind since the two streams were
    // closest. Positive means the picture is behind the sound.
    METRICS
        .av_lag_us
        .store(((best - skew) * 1_000_000.0) as i64, Ordering::Relaxed);
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
    av_origin_us: AtomicI64::new(0),
    av_pairs: AtomicU64::new(0),
    av_playout_us: AtomicI64::new(0),
    audio_sent_samples: AtomicU64::new(0),
    audio_decoded_samples: AtomicU64::new(0),
    audio_played_samples: AtomicU64::new(0),
    audio_queued_samples: AtomicU64::new(0),
    audio_pipeline_samples: AtomicU64::new(0),
    audio_jitter_samples: AtomicU64::new(0),
    audio_lost_ms: AtomicI64::new(0),
    audio_dropped_packets: AtomicU64::new(0),
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
    let server_offset_ms = METRICS.server_offset_us.load(Ordering::Relaxed) / 1000;
    let server_drift_ms = METRICS.server_drift_us.load(Ordering::Relaxed) / 1000;
    // The receive batch is not on the HUD any more, but the counters are
    // still reset so that they keep counting per interval.
    let _ = METRICS.rx_batch_sum.swap(0, Ordering::Relaxed);
    let _ = METRICS.rx_batch_count.swap(0, Ordering::Relaxed);
    let _ = METRICS.rx_batch_max.swap(0, Ordering::Relaxed);
    // Reset the input counters even though they are not on the HUD line any
    // more: the input is exonerated and the line is needed for the A/V
    // numbers.
    let _ = METRICS.input_wire_sum_us.swap(0, Ordering::Relaxed);
    let _ = METRICS.input_wire_max_us.swap(0, Ordering::Relaxed);
    let _ = METRICS.input_wire_count.swap(0, Ordering::Relaxed);
    let _ = METRICS.input_sent.swap(0, Ordering::Relaxed);
    // The origin offset is only useful as a magnitude, and the two streams
    // can be thousands of seconds apart when their timestamp origins are
    // unrelated. Nine digits is enough to tell the two cases apart.
    let av_origin_ms =
        (METRICS.av_origin_us.load(Ordering::Relaxed) / 1000).clamp(-99_999_999, 99_999_999);
    format!(
        "fps d/p:{}/{} us r:{rtp_average}/{rtp_max} d/a:{}/{} up/gpu:{}/{} pr/loop:{}/{} skip:{} repl:{} q:{} slot:{} rs:{} rst:{}\nhandoff:{} ui-ovr:{} max d>h/h>u:{}/{}us age:{age_average}/{}us in-lag:{input_lag_average}/{}us kf:{} pres:{} gap:{present_gap_average}/{}us aud:{audio_buffer_average}/{}ms ars:{} flush-gap:{}us\nsrv:{server_offset_ms}ms drift:{server_drift_ms}ms remb:{} av-lag:{}ms av-drift:{}ms av-out:{}ms a-def:{}ms av-k:{}ms",
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
        METRICS.av_lag_us.load(Ordering::Relaxed) / 1000,
        METRICS.av_lag_drift_us.load(Ordering::Relaxed) / 1000,
        METRICS.av_playout_us.load(Ordering::Relaxed) / 1000,
        METRICS.audio_lost_ms.load(Ordering::Relaxed),
        av_origin_ms,
    )
}
