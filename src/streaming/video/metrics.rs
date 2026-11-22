use std::collections::VecDeque;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// A step of more than this many seconds between two video frames counts as
/// a stutter: the picture lost ground with the sound in one go.
const STALL_STEP_SECONDS: f64 = 0.030;

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
    /// The same for the audio: how far the server's audio timeline lags our
    /// wall clock.
    ///
    /// The video's offset says whether the picture is getting older. The
    /// audio's says whether the sound is getting younger. Between them they
    /// say which of the two the picture is drifting against - and that is
    /// the difference between a delay the client adds and one the server
    /// adds.
    pub(crate) audio_offset_us: AtomicI64,
    /// How much that offset moved over the last minute.
    pub(crate) audio_drift_us: AtomicI64,
    /// How much the client's own audio delay moved over the last minute.
    ///
    /// This is the one that answers "why does the picture drift further
    /// from the sound the longer the session runs". The sound's delay is
    /// the client's own hoard - the jitter buffer, the decoded audio
    /// waiting for the device, and the device queue. When that hoard
    /// shrinks, the sound gets closer to the picture, and the picture
    /// looks later than the sound. A negative value means the sound is
    /// catching up.
    pub(crate) audio_delay_drift_us: AtomicI64,
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
    /// microseconds. Positive means the picture is behind the sound.
    ///
    /// This one is absolute. The two streams carry their own RTP timestamp
    /// origins, so their raw difference is the delay plus a constant this
    /// client cannot know - but the first video frame and the first audio
    /// packet come from the same moment of the game, so the difference
    /// between their arrival times is an absolute reference. On top of that
    /// sits how much the picture has fallen behind since the stream
    /// started.
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
    /// How many times the picture lost more than one frame of alignment with
    /// the sound in a single step. Each one is a stutter the player can see.
    pub(crate) av_stalls: AtomicU64,
    /// How much alignment those stutters cost in total, in microseconds.
    pub(crate) av_stall_us: AtomicU64,
    /// The age of the frame on the screen right now, in microseconds: first
    /// RTP packet arrival to upload. The per-window average and maximum sit
    /// next to it; this one is the value in force at the moment the HUD is
    /// drawn.
    pub(crate) frame_age_last_us: AtomicU64,
    /// When a keyframe request went out, waiting for the fresh picture it
    /// should bring back. The round trip is the picture's absolute age.
    pub(crate) probe_armed_at: Mutex<Option<KeyframeProbe>>,
    /// How long the last keyframe probe took: from the request until the
    /// fresh picture was completely in hand. This is how old the picture is
    /// when it lands, in milliseconds - the one number the client cannot
    /// get any other way, because the frame age starts when the packet
    /// leaves the socket buffer.
    pub(crate) probe_last_us: AtomicU64,
    /// The fastest keyframe probe of the session. The server may answer
    /// late sometimes; the fastest answer is the cleanest estimate of how
    /// long the picture takes to come back.
    pub(crate) probe_min_us: AtomicU64,
    /// The same probe with the keyframe's own time on the wire taken out:
    /// this is how long the picture took to come back.
    pub(crate) probe_net_last_us: AtomicU64,
    pub(crate) probe_net_min_us: AtomicU64,
    /// Keyframe probes that produced a measurement.
    pub(crate) probe_count: AtomicU64,
    /// Keyframe probes thrown away because the picture was not captured
    /// after the request.
    pub(crate) probe_rejected: AtomicU64,
    /// The longest the client held a damaged frame waiting for its missing
    /// packets. This is the freeze the player sees when a packet is lost:
    /// the frames behind it cannot be decoded before it.
    pub(crate) repair_wait_max_us: AtomicU64,
    /// The total time spent in that repair window.
    pub(crate) repair_wait_sum_us: AtomicU64,
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
    /// Audio the client threw away because it was older than the delay the
    /// client is willing to carry.
    ///
    /// This is a hole in the sound: the player does not hear it, and the
    /// sound after it plays early. It is the price the client pays for
    /// not trailing the picture by a quarter of a second, and the HUD
    /// shows it so the price is visible.
    pub(crate) audio_trimmed_samples: AtomicU64,
    /// Audio that was in the device queue when the client trimmed the
    /// delay. It never reached the speaker.
    pub(crate) audio_cleared_samples: AtomicU64,
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
    /// The raw difference between the two streams when the session started.
    ///
    /// This is the reference for everything else. The picture and the sound
    /// come from the same moment when the stream starts, so this is where
    /// they were in sync, and the difference between this and any later
    /// reading is how far the picture has fallen behind since.
    start_skew: Option<f64>,
    /// When the first video frame arrived, and when the first audio packet
    /// arrived.
    ///
    /// Both come from the same moment of the game - the moment the stream
    /// starts - so the difference between the two arrival times is how much
    /// older the picture was than the sound when they got here. That is an
    /// absolute reference: it does not depend on the two streams sharing a
    /// timestamp origin, which they do not.
    first_video_at: Option<Instant>,
    first_audio_at: Option<Instant>,
    /// The audio's own timeline against our wall clock: the first audio
    /// packet's timestamp and when it arrived.
    audio_timeline_origin: Option<(u32, u32, Instant)>,
    /// One sample per second of (arrival, timestamp), so the offset can be
    /// compared against a minute-old sample.
    audio_timeline_samples: VecDeque<(Instant, u32)>,
    /// One sample per second of the client's own audio delay, so the
    /// change over a minute can be measured.
    audio_delay_samples: VecDeque<(Instant, u64)>,
    /// The closest the two streams ever came: the best alignment of the
    /// session.
    best_skew: Option<f64>,
    /// The previous reading, so a stutter can be told apart from a drift.
    last_skew: Option<f64>,
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
    start_skew: None,
    first_video_at: None,
    first_audio_at: None,
    audio_timeline_origin: None,
    audio_timeline_samples: VecDeque::new(),
    audio_delay_samples: VecDeque::new(),
    best_skew: None,
    last_skew: None,
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
    if sync.first_video_at.is_none() {
        sync.first_video_at = Some(arrived_at);
    }
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
    if sync.first_audio_at.is_none() {
        sync.first_audio_at = Some(arrived_at);
    }
    record_audio_timeline(&mut sync, timestamp, clock_rate, arrived_at);
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

/// Measures how far the server's audio timeline lags our wall clock.
///
/// This is the audio's counterpart of the video's `srv`. The audio's RTP
/// timestamp is the server's capture time, so the gap between it and our
/// arrival clock is the delay that is already in front of the client.
///
/// It is the other half of the A/V comparison. The picture's offset says
/// whether the picture is getting older; the audio's says whether the sound
/// is getting younger. Between them they say which side the picture is
/// drifting against - and that is the difference between a delay this
/// client adds and one the server adds.
fn record_audio_timeline(
    sync: &mut AvSync,
    timestamp: u32,
    clock_rate: u32,
    arrived_at: Instant,
) {
    let Some((origin_ts, origin_rate, origin_wall)) = sync.audio_timeline_origin else {
        sync.audio_timeline_origin = Some((timestamp, clock_rate, arrived_at));
        return;
    };
    let offset_us = audio_timeline_offset_us(origin_ts, origin_rate, origin_wall, timestamp, clock_rate, arrived_at);
    METRICS
        .audio_offset_us
        .store(offset_us, Ordering::Relaxed);

    // One sample per second is enough to see a queue that grows over
    // minutes, and it keeps the comparison below free of packet jitter.
    let due = sync
        .audio_timeline_samples
        .back()
        .is_none_or(|(last_at, _)| arrived_at.duration_since(*last_at) >= Duration::from_secs(1));
    if !due {
        return;
    }
    sync.audio_timeline_samples.push_back((arrived_at, timestamp));
    while sync.audio_timeline_samples.len() > 130 {
        sync.audio_timeline_samples.pop_front();
    }
    // Compare against the sample from about a minute ago: a positive drift
    // means the audio is arriving later and later.
    if let Some((at, ts)) = sync
        .audio_timeline_samples
        .iter()
        .find(|(at, _)| arrived_at.duration_since(*at) >= Duration::from_secs(58))
    {
        let previous = audio_timeline_offset_us(
            origin_ts,
            origin_rate,
            origin_wall,
            *ts,
            origin_rate,
            *at,
        );
        METRICS
            .audio_drift_us
            .store(offset_us - previous, Ordering::Relaxed);
    }
}

fn audio_timeline_offset_us(
    origin_ts: u32,
    origin_rate: u32,
    origin_wall: Instant,
    timestamp: u32,
    clock_rate: u32,
    arrived_at: Instant,
) -> i64 {
    let rtp_delta_us = timestamp.wrapping_sub(origin_ts) as i64 * 1_000_000
        / i64::from(clock_rate.max(origin_rate));
    let wall_delta_us = arrived_at.duration_since(origin_wall).as_micros() as i64;
    wall_delta_us - rtp_delta_us
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
    // The audio the client trimmed away never reached the speaker either.
    let trimmed = METRICS.audio_trimmed_samples.load(Ordering::Relaxed);
    let cleared = METRICS.audio_cleared_samples.load(Ordering::Relaxed);
    let lost = sent
        .saturating_sub(decoded)
        .saturating_sub(jitter)
        .saturating_add(trimmed)
        .saturating_add(cleared);
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

/// Everything the client holds between the socket and the speaker.
///
/// The device queue is only the last of four stages. Before the samples
/// reach the device they sit in the jitter buffer, waiting for a late
/// packet; then in the decoder's output, waiting for room; then in the
/// client's own pending queue. The old number counted only the last two,
/// so it left the jitter buffer - up to a full 120 ms of it - out of the
/// comparison, and the HUD said the sound was 237 ms old when it was
/// really 357 ms old.
///
/// This is the client's own latency, and it is the biggest number in the
/// client: the picture reaches the screen 34 ms after it arrives, the
/// sound reaches the speaker 357 ms after it arrives.
pub(crate) fn client_audio_delay_samples() -> u64 {
    let sent = METRICS.audio_sent_samples.load(Ordering::Relaxed);
    let decoded = METRICS.audio_decoded_samples.load(Ordering::Relaxed);
    let played = METRICS.audio_played_samples.load(Ordering::Relaxed);
    let jitter = METRICS.audio_jitter_samples.load(Ordering::Relaxed);
    let queued = METRICS.audio_queued_samples.load(Ordering::Relaxed);
    let trimmed = METRICS.audio_trimmed_samples.load(Ordering::Relaxed);
    // Audio the server sent that never reached the decoder is a hole in
    // the sound, not a delay: it is not waiting anywhere, it is gone.
    let lost = sent.saturating_sub(decoded).saturating_sub(jitter);
    sent.saturating_sub(played)
        .saturating_sub(lost)
        .saturating_sub(trimmed)
        .saturating_add(queued)
}

/// The audio as it is heard against the video as it is seen.
///
/// The arrival comparison measures both streams as they come off the wire.
/// What the player sees is the picture after the client's display path and
/// what the player hears is the sound after the client's audio path, so the
/// client's own delays have to be taken into account: the picture is older
/// by the time it reaches the screen, and the sound is older by the time it
/// reaches the speaker.
fn update_playout_lag(sync: &mut AvSync) {
    let Some(behind_us) = sync.behind_us() else {
        return;
    };
    // The picture is this much older by the time it is on the screen...
    let picture_delay_us = METRICS.frame_age_last_us.load(Ordering::Relaxed) as i64;
    // ...and the sound is this much older by the time it is heard. This is
    // every stage of the client's audio path, not just the device queue.
    let delay_samples = client_audio_delay_samples();
    let sound_delay_us = (delay_samples * 1_000 / 48) as i64;
    METRICS
        .av_playout_us
        .store(behind_us + picture_delay_us - sound_delay_us, Ordering::Relaxed);
    record_audio_delay_drift(sync, delay_samples);
}

/// How much the client's own audio delay changed over the last minute.
///
/// This is the number that explains why the picture drifts further from
/// the sound the longer the session runs. The sound the player hears is
/// held back by the client's own hoard, and the hoard does not stay the
/// same size: it is filled at the start of the stream and then drains. A
/// negative value means the sound is catching up with the picture, which
/// is exactly what makes the picture look later and later.
fn record_audio_delay_drift(sync: &mut AvSync, delay_samples: u64) {
    let now = Instant::now();
    let due = sync
        .audio_delay_samples
        .back()
        .is_none_or(|(last_at, _)| now.duration_since(*last_at) >= Duration::from_secs(1));
    if !due {
        return;
    }
    sync.audio_delay_samples.push_back((now, delay_samples));
    while sync.audio_delay_samples.len() > 130 {
        sync.audio_delay_samples.pop_front();
    }
    if let Some((_, samples)) = sync
        .audio_delay_samples
        .iter()
        .find(|(at, _)| now.duration_since(*at) >= Duration::from_secs(58))
    {
        let delta_ms = (delay_samples as i64 - *samples as i64) * 1_000 / 48;
        METRICS
            .audio_delay_drift_us
            .store(delta_ms * 1_000, Ordering::Relaxed);
    }
}

impl AvSync {
    /// How far the picture has fallen behind the sound, in microseconds.
    ///
    /// Positive means the picture is behind. The reference is the state of
    /// the two streams when the session started: the picture and the sound
    /// come from the same moment then, so this is where they were in sync.
    fn behind_us(&self) -> Option<i64> {
        let start = self.start_skew?;
        let (video_ts, video_at) = self.video?;
        let audio_ts = audio_tick_at(self, video_at)?;
        let origin_ms = match (self.first_video_at, self.first_audio_at) {
            (Some(video_at), Some(audio_at)) => {
                video_at.saturating_duration_since(audio_at).as_micros() as f64 / 1_000.0
            }
            _ => 0.0,
        };
        let behind_ms = origin_ms + (start - (video_ts - audio_ts)) * 1_000.0;
        Some((behind_ms * 1_000.0) as i64)
    }
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
    // The reference point: when the stream started, the picture and the
    // sound came from the same moment, so this is where they were in sync.
    //
    // The old code used the *smallest* difference seen so far as the
    // reference, which is the worst alignment of the session: the number
    // it produced was the change since the picture was furthest behind, not
    // the distance the picture is behind. With the sign that produced, a
    // picture that is half a second late showed up as a small number that
    // grew a little at every stutter - which is exactly what the HUD was
    // doing.
    let start = *sync.start_skew.get_or_insert(skew);
    // The absolute reference.
    //
    // The first video frame and the first audio packet come from the same
    // moment of the game - the moment the stream starts. So the difference
    // between the two arrival times is how much older the picture was than
    // the sound when they got here, and that difference is absolute: it
    // does not care that the two streams carry unrelated timestamp
    // origins.
    //
    // On top of that sits how much the picture has fallen behind since
    // then.
    let origin_ms = match (sync.first_video_at, sync.first_audio_at) {
        (Some(video_at), Some(audio_at)) => {
            video_at.saturating_duration_since(audio_at).as_micros() as f64 / 1_000.0
        }
        // The audio has not arrived yet: the reference is not known, so
        // only the change since the start counts for now.
        _ => 0.0,
    };
    let behind_ms = origin_ms + (start - skew) * 1_000.0;
    METRICS
        .av_lag_us
        .store((behind_ms * 1_000.0) as i64, Ordering::Relaxed);
    // A step of more than a frame and a half is a stutter: the picture lost
    // ground with the sound in one go, which is what the player sees as a
    // hitch.
    if let Some(previous) = sync.last_skew {
        let step_s = previous - skew;
        if step_s > STALL_STEP_SECONDS {
            METRICS.av_stalls.fetch_add(1, Ordering::Relaxed);
            METRICS
                .av_stall_us
                .fetch_add((step_s * 1_000_000.0) as u64, Ordering::Relaxed);
        }
    }
    sync.last_skew = Some(skew);
    // The closest the two ever came, kept for the record.
    let best = *sync.best_skew.get_or_insert(skew);
    sync.best_skew = Some(best.max(skew));
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
pub(crate) fn mark_input_queued(at: Instant) {    if let Ok(mut slot) = METRICS.input_queued_at.lock() {
        *slot = Some(at);
    }
}

/// A keyframe request waiting for its answer.
pub(crate) struct KeyframeProbe {
    /// When the request went out.
    pub sent_at: Instant,
    /// The newest video timestamp the client had seen when it went out.
    /// The server's capture point is at that timestamp, so a picture that
    /// comes back with a much newer timestamp was captured *after* the
    /// request - only that one says anything about how long a request takes.
    pub video_ts: u32,
}

/// Arms the keyframe probe.
///
/// From now on the next fresh picture the server sends measures how long a
/// new picture takes to come back. The frame age the HUD reports starts when
/// a packet leaves the socket buffer, so it cannot see the delay that is
/// already in front of the client; this probe can, because the moment the
/// request goes out is a moment the client knows the content of.
pub(crate) fn arm_keyframe_probe(sent_at: Instant, video_ts: u32) {
    if let Ok(mut slot) = METRICS.probe_armed_at.lock() {
        *slot = Some(KeyframeProbe { sent_at, video_ts });
    }
}

/// Takes the armed probe, if there is one.
pub(crate) fn take_keyframe_probe() -> Option<KeyframeProbe> {
    METRICS
        .probe_armed_at
        .lock()
        .ok()
        .and_then(|mut slot| slot.take())
}

/// Records how long the probe took.
///
/// The round trip is the time from the request until the fresh picture was
/// completely in hand. A keyframe is large and takes a while to arrive, so
/// the time it spent on the wire is taken out: what is left is how long the
/// picture took to come back - the delay that is already in front of the
/// client when the first packet shows up.
pub(crate) fn record_keyframe_probe(sent_at: Instant, arrived_at: Instant, on_the_wire: Duration) {
    let elapsed_us = arrived_at.saturating_duration_since(sent_at).as_micros() as u64;
    METRICS.probe_last_us.store(elapsed_us, Ordering::Relaxed);
    METRICS.probe_min_us.fetch_min(elapsed_us, Ordering::Relaxed);
    let net_us = elapsed_us.saturating_sub(on_the_wire.as_micros() as u64);
    METRICS.probe_net_last_us.store(net_us, Ordering::Relaxed);
    METRICS.probe_net_min_us.fetch_min(net_us, Ordering::Relaxed);
    METRICS.probe_count.fetch_add(1, Ordering::Relaxed);
}

/// A probe result that was thrown away: the picture was not captured after
/// the request, so it says nothing about how long a request takes.
pub(crate) fn note_rejected_probe() {
    METRICS.probe_rejected.fetch_add(1, Ordering::Relaxed);
}

/// Notes that the client held a damaged frame for this long waiting for its
/// missing packets. Every frame behind it waits too, so this is a freeze
/// the player sees.
pub(crate) fn note_repair_wait(waited_us: u64) {
    METRICS
        .repair_wait_max_us
        .fetch_max(waited_us, Ordering::Relaxed);
    METRICS
        .repair_wait_sum_us
        .fetch_add(waited_us, Ordering::Relaxed);
}

/// The age of the frame that is on the screen right now.
pub(crate) fn observe_presented_age(age_us: u64) {
    METRICS.frame_age_last_us.store(age_us, Ordering::Relaxed);
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
    audio_offset_us: AtomicI64::new(0),
    audio_drift_us: AtomicI64::new(0),
    audio_delay_drift_us: AtomicI64::new(0),
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
    av_stalls: AtomicU64::new(0),
    av_stall_us: AtomicU64::new(0),
    frame_age_last_us: AtomicU64::new(0),
    probe_armed_at: Mutex::new(None),
    probe_last_us: AtomicU64::new(0),
    probe_min_us: AtomicU64::new(u64::MAX),
    probe_net_last_us: AtomicU64::new(0),
    probe_net_min_us: AtomicU64::new(u64::MAX),
    probe_count: AtomicU64::new(0),
    probe_rejected: AtomicU64::new(0),
    repair_wait_max_us: AtomicU64::new(0),
    repair_wait_sum_us: AtomicU64::new(0),
    audio_sent_samples: AtomicU64::new(0),
    audio_decoded_samples: AtomicU64::new(0),
    audio_played_samples: AtomicU64::new(0),
    audio_queued_samples: AtomicU64::new(0),
    audio_pipeline_samples: AtomicU64::new(0),
    audio_jitter_samples: AtomicU64::new(0),
    audio_lost_ms: AtomicI64::new(0),
    audio_dropped_packets: AtomicU64::new(0),
    audio_trimmed_samples: AtomicU64::new(0),
    audio_cleared_samples: AtomicU64::new(0),
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
    let server_offset_ms = (METRICS.server_offset_us.load(Ordering::Relaxed) / 1000).clamp(-9_999, 9_999);
    let server_drift_ms = (METRICS.server_drift_us.load(Ordering::Relaxed) / 1000).clamp(-999, 999);
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
    // How far the picture is behind the sound, in milliseconds. Positive
    // means the picture is late.
    let av_lag_ms = (METRICS.av_lag_us.load(Ordering::Relaxed) / 1000).clamp(-999, 999);
    // How much that grew over the last minute. Positive means the picture
    // is still falling further behind the sound at the wire - the picture
    // is getting older, not younger.
    let av_lag_drift_ms = (METRICS.av_lag_drift_us.load(Ordering::Relaxed) / 1000).clamp(-999, 999);
    // The same at the output: the picture as it is seen against the sound
    // as it is heard. Positive means the picture looks late.
    let av_out_ms = (METRICS.av_playout_us.load(Ordering::Relaxed) / 1000).clamp(-999, 999);
    // The delay this client adds to the sound: every stage between the
    // socket and the speaker - the jitter buffer, the decoded audio waiting
    // for the device, and the device queue. This is the client's own share
    // of the audio's latency, and the picture's age next to it is the
    // client's share of the video's.
    let audio_delay_ms = (client_audio_delay_samples() * 1_000 / 48_000) as i64;
    let audio_delay_ms = audio_delay_ms.clamp(0, 999);
    // How much that delay changed over the last minute. Negative means the
    // sound is catching up with the picture, which is what makes the
    // picture look later and later the longer the session runs.
    let audio_delay_drift_ms = (METRICS.audio_delay_drift_us.load(Ordering::Relaxed) / 1000).clamp(-999, 999);
    // The server's audio pipeline against our wall clock, the same number
    // the video's `srv` is. Between them they say which side the picture
    // is drifting against.
    let audio_offset_ms = (METRICS.audio_offset_us.load(Ordering::Relaxed) / 1000).clamp(-9_999, 9_999);
    // How long the fresh picture took to come back when it was asked for.
    // This is the picture's absolute age, the one delay that happens
    // before the client can see anything at all.
    let probe_count = METRICS.probe_count.load(Ordering::Relaxed);
    let probe_min_us = METRICS.probe_net_min_us.load(Ordering::Relaxed);
    let probe_ms = if probe_count == 0 {
        // No usable answer yet: the server did not send a picture that was
        // captured after the request.
        "?".to_owned()
    } else {
        format!("{}", (probe_min_us / 1000).min(999))
    };
    // The longest the client held a damaged frame waiting for its missing
    // packets, and the number of times the picture lost ground with the
    // sound in one step.
    let repair_wait_ms = (METRICS.repair_wait_max_us.load(Ordering::Relaxed) / 1000).min(9_999);
    let av_stalls = METRICS.av_stalls.load(Ordering::Relaxed).min(999);
    // How many probes found a picture that was captured after the request,
    // and how many found one that was already on its way.
    let probe_ok = METRICS.probe_count.load(Ordering::Relaxed);
    let probe_no = METRICS.probe_rejected.load(Ordering::Relaxed);
    let _ = (probe_ok, probe_no, repair_wait_ms);
    let audio_lost_ms = METRICS.audio_lost_ms.load(Ordering::Relaxed).clamp(-99, 99);
    format!(
        "fps d/p:{}/{} us r:{rtp_average}/{rtp_max} d/a:{}/{} up/gpu:{}/{} pr/loop:{}/{} skip:{} repl:{} q:{} slot:{} rs:{} rst:{} rmb:{} stut:{av_stalls} a-def:{audio_lost_ms}\nhandoff:{} ui-ovr:{} max d>h/h>u:{}/{}us age:{age_average}/{}us in-lag:{input_lag_average}/{}us kf:{} pres:{} gap:{present_gap_average}/{}us aud:{audio_buffer_average}/{}ms ars:{} flush-gap:{}us a-srv:{audio_offset_ms}ms\nsrv:{server_offset_ms}ms drift:{server_drift_ms}ms av-lag:{av_lag_ms}ms ld:{av_lag_drift_ms}ms av-out:{av_out_ms}ms a-del:{audio_delay_ms}ms ad:{audio_delay_drift_ms}ms kp:{probe_ms}ms",
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
        METRICS.bandwidth_estimate_kbps.load(Ordering::Relaxed),
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
    )
}
