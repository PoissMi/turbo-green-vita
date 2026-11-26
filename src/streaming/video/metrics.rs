use std::collections::VecDeque;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// A step of more than this many seconds between two video frames counts as
/// a stutter: the picture lost ground with the sound in one go.
const STALL_STEP_SECONDS: f64 = 0.030;

/// The parts of one RTC pump cycle, in the order they run.
///
/// The cycle used to be measured as a single number, which says that it
/// got slower but not which part of it owns the time. Each stage is timed
/// on its own so the worst cycle can be attributed.
pub(crate) const PUMP_STAGE_FLUSH: usize = 0;
/// Sending the queued datagrams.
pub(crate) const PUMP_STAGE_SOCKET: usize = 1;
/// Reading the socket. Split from the handling below, because a receive
/// cycle that takes tens of milliseconds can be either the socket read
/// itself or the work done on what it read.
pub(crate) const PUMP_STAGE_HANDLE: usize = 2;
/// The packet handling: SRTP decryption and the RTP parse.
pub(crate) const PUMP_STAGE_EVENTS: usize = 3;
/// Peer events (track open, connection state).
pub(crate) const PUMP_STAGE_MSGS: usize = 4;
/// The RTP messages: reassembly and the hand-off to the decoder.
pub(crate) const PUMP_STAGE_DECODE: usize = 5;
/// Taking the decoded frames out of the decoder worker.
pub(crate) const PUMP_STAGE_CTRL: usize = 6;
/// Timeouts, keyframe requests, NACKs and the bandwidth report.
pub(crate) const PUMP_STAGE_STATUS: usize = 7;
/// Building the 1 Hz status line the HUD shows.
pub(crate) const PUMP_STAGE_COUNT: usize = 8;

/// The one-letter names the HUD uses for the stages.
pub(crate) const PUMP_STAGE_NAMES: [&str; PUMP_STAGE_COUNT] =
    ["f", "so", "hd", "e", "m", "d", "c", "s"];

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
    /// Decode to handoff, summed over the interval, so the HUD can show
    /// the average. The max is the worst frame of the second; the average
    /// is the standing delay.
    pub(crate) decode_to_handoff_sum_us: AtomicU64,
    pub(crate) decode_to_handoff_count: AtomicU64,
    pub(crate) handoff_to_upload_max_us: AtomicU64,
    pub(crate) handoff_to_upload_sum_us: AtomicU64,
    pub(crate) handoff_to_upload_count: AtomicU64,
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
    /// Packets drained from the socket per receive call. A rising batch size
    /// means packets are piling up in the client's own receive buffer.
    pub(crate) rx_batch_sum: AtomicU64,
    pub(crate) rx_batch_max: AtomicU64,
    pub(crate) rx_batch_count: AtomicU64,
    /// Worst RTC pump cycle duration in the window (µs): socket drain
    /// through decode drain and RTCP. A long cycle is video age added
    /// proportional to the burst — i.e. bitrate.
    pub(crate) pump_max_us: AtomicU64,
    /// Per-stage pump timing: the worst cycle of the window and the
    /// standing average, so the HUD can say which part of the cycle owns
    /// the time instead of only that the cycle got slower.
    pub(crate) pump_stage_max_us: [AtomicU64; PUMP_STAGE_COUNT],
    pub(crate) pump_stage_sum_us: [AtomicU64; PUMP_STAGE_COUNT],
    pub(crate) pump_stage_count: [AtomicU64; PUMP_STAGE_COUNT],
    /// Packets read from the socket in the window. The packet rate is what
    /// the receive path is actually paid for: it scales with the bitrate,
    /// and it is the number that decides how much of the CPU the SRTP
    /// decryption costs.
    pub(crate) rx_packets: AtomicU64,
    /// The worst receive cycle of the window, packed as
    /// `(microseconds << 20) | packets`, so the number of packets that
    /// came with the slowest cycle comes with it. Without it, the cost per
    /// packet cannot be told from a cost per cycle.
    pub(crate) recv_cycle_packed: AtomicU64,
    /// UDP datagrams flushed per cycle (ALL traffic, not SCTP alone:
    /// RTCP, SCTP control and data all exit via `poll_write`).
    /// This is NOT an SCTP queue metric — the fork exposes no public
    /// `pending_queue`/`cwnd`/`rwnd` accessor; `Association::buffered_amount`
    /// is `pub(crate)`. Do not infer queue health from this field.
    pub(crate) sctp_out_max: AtomicU64,
    pub(crate) sctp_out_sum: AtomicU64,
    pub(crate) sctp_out_count: AtomicU64,
    /// When the last input report was handed to the data channel. The next
    /// outgoing packet carries it, so the gap between the two is how long
    /// the report waited for the wire.
    pub(crate) input_queued_at: Mutex<Option<Instant>>,
    pub(crate) input_wire_sum_us: AtomicU64,
    pub(crate) input_wire_max_us: AtomicU64,
    pub(crate) input_wire_count: AtomicU64,
    /// A distinctive input change waiting for the picture it causes. The
    /// time from the change until the first frame that could have been
    /// captured after it is the input→photon round trip.
    pub(crate) input_probe_armed_at: Mutex<Option<InputProbe>>,
    /// How long the last input probe took.
    pub(crate) input_probe_last_us: AtomicU64,
    /// The fastest input probe of the session.
    pub(crate) input_probe_min_us: AtomicU64,
    /// The same with the picture's time on the wire taken out.
    pub(crate) input_probe_net_last_us: AtomicU64,
    pub(crate) input_probe_net_min_us: AtomicU64,
    /// Probes that produced a measurement.
    pub(crate) input_probe_count: AtomicU64,
    /// Probes thrown away because the picture predates the input.
    pub(crate) input_probe_rejected: AtomicU64,
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
    /// The keyframe probe used to measure the picture's round trip by
    /// asking for a keyframe every 10 seconds. It cost a hitch every time
    /// (a keyframe on a 5 Mbps link), so it is gone; the picture's age is
    /// now the server offset plus the display age, both already measured.
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
    /// The samples those dropped packets carried.
    pub(crate) audio_dropped_samples: AtomicU64,
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
    /// How much of the client's total audio time is in holes, in
    /// milliseconds. Every hole is the sound jumping forward in game
    /// time; the sum is the drift the player feels as the picture
    /// getting later and later.
    pub(crate) audio_holes_ms: AtomicI64,
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
    // The audio the client trimmed away never reached the speaker either,
    // and neither did the audio the jitter buffer dropped after its
    // deadline. Both are holes.
    let trimmed = METRICS.audio_trimmed_samples.load(Ordering::Relaxed);
    let cleared = METRICS.audio_cleared_samples.load(Ordering::Relaxed);
    let dropped = METRICS.audio_dropped_samples.load(Ordering::Relaxed);
    let lost = sent
        .saturating_sub(decoded)
        .saturating_sub(jitter)
        .saturating_add(trimmed)
        .saturating_add(cleared)
        .saturating_add(dropped);
    METRICS
        .audio_lost_ms
        .store(((lost * 1_000) / 48_000) as i64, Ordering::Relaxed);
    METRICS
        .audio_holes_ms
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
    // Every stage counted exactly once: the jitter buffer (everything
    // the server has sent that the decoder has not yet taken), the
    // decoded pipeline (what the decoder has taken and the device has
    // not), and the device queue (what the device has not played yet).
    //
    // The earlier formula added the device queue to
    // `sent - played - lost - trimmed`, which counts the queue twice:
    // `played` only increments when audio is *handed to* the device, so
    // everything sitting in that queue is inside both terms. That pushed
    // `a-del` up by the full queue depth and made `ad` - the change in
    // a number the trim itself drives down - a clamp-bound nonsense
    // reading instead of a measurement.
    let jitter = METRICS.audio_jitter_samples.load(Ordering::Relaxed);
    let pipeline = METRICS.audio_pipeline_samples.load(Ordering::Relaxed);
    let queued = METRICS.audio_queued_samples.load(Ordering::Relaxed);
    jitter.saturating_add(pipeline).saturating_add(queued)
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

/// A distinctive input event waiting for the picture it causes.
///
/// To measure input→photon end to end, the client marks a moment it knows
/// the content of - a sudden change in the controller state - and times
/// how long until a frame that can only have been captured *after* that
/// change lands. The difference is the server's simulation time plus
/// the capture, encode and network time, which is the part of the round
/// trip no other number on the screen covers: `in-lag` stops when the
/// report leaves the client, and `kp` measures a keyframe request, not
/// an input.
pub(crate) struct InputProbe {
    /// When the change was sampled.
    pub sampled_at: Instant,
    /// The newest video timestamp the client had seen at that moment.
    pub video_ts: u32,
    /// The picture must be at least this much newer than `video_ts` to
    /// count as caused by the input. Without it the answer is any frame:
    /// a picture captured a frame after the change, before the server
    /// could possibly have received the input, which reads as a 5 ms
    /// round trip while the real one is 500.
    pub min_newer_ms: i64,
}

/// Arms the input probe.
///
/// The causality floor is the measured round trip of the *picture* path:
/// the server's own stretch (`srv`) plus the client's display age. A
/// picture newer than that could genuinely have been caused by the input;
/// anything older cannot. The keyframe probe used to supply this floor,
/// but a keyframe request costs a hitch every time, so the floor is now
/// derived from numbers that are already measured.
pub(crate) fn arm_input_probe(sampled_at: Instant, video_ts: u32) {
    let server_ms = METRICS.server_offset_us.load(Ordering::Relaxed) / 1000;
    let age_ms = METRICS.frame_age_last_us.load(Ordering::Relaxed) as i64 / 1000;
    let min_newer_ms = server_ms.max(0) + age_ms.max(0);
    if let Ok(mut slot) = METRICS.input_probe_armed_at.lock() {
        *slot = Some(InputProbe {
            sampled_at,
            video_ts,
            min_newer_ms,
        });
    }
}

/// Peeks at the armed input probe, if there is one.
///
/// The probe is peeked rather than taken: the first picture that answers
/// it is the first one new enough, and the pictures in between are too
/// old to have been caused by the input and must be skipped, not
/// consumed. Taking the probe on the first frame threw it away at once
/// and left the HUD at `?/?` forever.
pub(crate) fn peek_input_probe() -> Option<InputProbe> {
    if let Ok(slot) = METRICS.input_probe_armed_at.lock() {
        return slot.as_ref().map(|probe| InputProbe {
            sampled_at: probe.sampled_at,
            video_ts: probe.video_ts,
            min_newer_ms: probe.min_newer_ms,
        });
    }
    None
}

/// Clears the armed input probe once it has been answered or rejected.
pub(crate) fn clear_input_probe() {
    if let Ok(mut slot) = METRICS.input_probe_armed_at.lock() {
        *slot = None;
    }
}

/// Records how long the input probe took to come back as a picture.
pub(crate) fn record_input_probe(sampled_at: Instant, arrived_at: Instant, on_the_wire: Duration) {
    let elapsed_us = arrived_at.saturating_duration_since(sampled_at).as_micros() as u64;
    METRICS.input_probe_last_us.store(elapsed_us, Ordering::Relaxed);
    METRICS.input_probe_min_us.fetch_min(elapsed_us, Ordering::Relaxed);
    let net_us = elapsed_us.saturating_sub(on_the_wire.as_micros() as u64);
    METRICS.input_probe_net_last_us.store(net_us, Ordering::Relaxed);
    METRICS.input_probe_net_min_us.fetch_min(net_us, Ordering::Relaxed);
    METRICS.input_probe_count.fetch_add(1, Ordering::Relaxed);
}

/// A probe result that was thrown away.
pub(crate) fn note_rejected_input_probe() {
    METRICS.input_probe_rejected.fetch_add(1, Ordering::Relaxed);
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

/// How long the same piece of work took on each of the Vita's two user
/// CPUs.
///
/// The receive path is paid per packet in software, so the core it runs
/// on decides how much of the bitrate the client can keep up with. The
/// SDK does not say which core is which, so the client measures it.
#[derive(Clone, Copy)]
pub(crate) struct CpuBenchmark {
    pub(crate) user_0_ns: u64,
    pub(crate) user_1_ns: u64,
}

static CPU_BENCHMARK: std::sync::OnceLock<CpuBenchmark> = std::sync::OnceLock::new();

/// Records the result of the startup CPU comparison.
pub(crate) fn record_cpu_benchmark(benchmark: CpuBenchmark) {
    let _ = CPU_BENCHMARK.set(benchmark);
}

/// The measured speed of both user CPUs, in nanoseconds for the same work.
pub(crate) fn cpu_benchmark_text() -> String {
    match CPU_BENCHMARK.get() {
        Some(benchmark) => format!("{}/{}", benchmark.user_0_ns, benchmark.user_1_ns),
        None => "?/?".to_owned(),
    }
}

/// Records how long one part of the pump cycle took.
pub(crate) fn record_pump_stage(stage: usize, micros: u64) {
    METRICS.pump_stage_max_us[stage].fetch_max(micros, Ordering::Relaxed);
    METRICS.pump_stage_sum_us[stage].fetch_add(micros, Ordering::Relaxed);
    METRICS.pump_stage_count[stage].fetch_add(1, Ordering::Relaxed);
}

/// Records one receive cycle: how long it took, how many packets it held.
///
/// The packet count is kept with the time on purpose. A receive cycle that
/// takes 60 ms is either slow because each packet is expensive, or because
/// a burst of a hundred packets came at once - and those two need
/// different fixes.
pub(crate) fn record_receive_cycle(micros: u64, packets: u64) {
    METRICS.rx_packets.fetch_add(packets, Ordering::Relaxed);
    let packed = (micros.min(1 << 40) << 20) | packets.min(0xF_FFFF);
    METRICS.recv_cycle_packed.fetch_max(packed, Ordering::Relaxed);
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
    decode_to_handoff_sum_us: AtomicU64::new(0),
    decode_to_handoff_count: AtomicU64::new(0),
    handoff_to_upload_max_us: AtomicU64::new(0),
    handoff_to_upload_sum_us: AtomicU64::new(0),
    handoff_to_upload_count: AtomicU64::new(0),
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
    rx_batch_sum: AtomicU64::new(0),
    rx_batch_max: AtomicU64::new(0),
    rx_batch_count: AtomicU64::new(0),
    pump_max_us: AtomicU64::new(0),
    pump_stage_max_us: [const { AtomicU64::new(0) }; PUMP_STAGE_COUNT],
    pump_stage_sum_us: [const { AtomicU64::new(0) }; PUMP_STAGE_COUNT],
    pump_stage_count: [const { AtomicU64::new(0) }; PUMP_STAGE_COUNT],
    rx_packets: AtomicU64::new(0),
    recv_cycle_packed: AtomicU64::new(0),
    sctp_out_max: AtomicU64::new(0),
    sctp_out_sum: AtomicU64::new(0),
    sctp_out_count: AtomicU64::new(0),
    input_queued_at: Mutex::new(None),
    input_wire_sum_us: AtomicU64::new(0),
    input_wire_max_us: AtomicU64::new(0),
    input_wire_count: AtomicU64::new(0),
    input_probe_armed_at: Mutex::new(None),
    input_probe_last_us: AtomicU64::new(0),
    input_probe_min_us: AtomicU64::new(u64::MAX),
    input_probe_net_last_us: AtomicU64::new(0),
    input_probe_net_min_us: AtomicU64::new(u64::MAX),
    input_probe_count: AtomicU64::new(0),
    input_probe_rejected: AtomicU64::new(0),
    input_sent: AtomicU64::new(0),
    av_lag_us: AtomicI64::new(0),
    av_lag_drift_us: AtomicI64::new(0),
    av_origin_us: AtomicI64::new(0),
    av_pairs: AtomicU64::new(0),
    av_playout_us: AtomicI64::new(0),
    av_stalls: AtomicU64::new(0),
    av_stall_us: AtomicU64::new(0),
    frame_age_last_us: AtomicU64::new(0),
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
    audio_dropped_samples: AtomicU64::new(0),
    audio_trimmed_samples: AtomicU64::new(0),
    audio_cleared_samples: AtomicU64::new(0),
    audio_holes_ms: AtomicI64::new(0),
};

/// Formats the per-stage pump timings for the HUD: one value per stage,
/// in the order the stages run.
fn format_pump_stages(values: &[u64; PUMP_STAGE_COUNT]) -> String {
    let mut out = String::with_capacity(PUMP_STAGE_COUNT * 7);
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            out.push('/');
        }
        out.push_str(PUMP_STAGE_NAMES[index]);
        out.push_str(&value.to_string());
    }
    out
}

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
    // Decode to handoff and handoff to upload: the standing average of
    // each, so the HUD shows the typical delay and not only the worst
    // frame of the second. The max alone understates the backlog.
    let decode_to_handoff_average = METRICS
        .decode_to_handoff_sum_us
        .swap(0, Ordering::Relaxed)
        .checked_div(METRICS.decode_to_handoff_count.swap(0, Ordering::Relaxed))
        .unwrap_or(0);
    let handoff_to_upload_average = METRICS
        .handoff_to_upload_sum_us
        .swap(0, Ordering::Relaxed)
        .checked_div(METRICS.handoff_to_upload_count.swap(0, Ordering::Relaxed))
        .unwrap_or(0);
    let audio_buffer_average = METRICS
        .audio_buffer_sum_ms
        .swap(0, Ordering::Relaxed)
        .checked_div(METRICS.audio_buffer_count.swap(0, Ordering::Relaxed))
        .unwrap_or(0);
    let server_offset_ms = (METRICS.server_offset_us.load(Ordering::Relaxed) / 1000).clamp(-999, 999);
    let server_drift_ms = (METRICS.server_drift_us.load(Ordering::Relaxed) / 1000).clamp(-999, 999);
    // The input→photon round trip: from a sampled controller change until
    // the first picture that could have been captured after it landed, in
    // milliseconds. This is the half of the loop no other number covers:
    // `in-lag` stops when the report leaves the client, and `kp` measures
    // a keyframe request rather than an input. Latest first, then the
    // session minimum.
    let input_probe_count = METRICS.input_probe_count.load(Ordering::Relaxed);
    let input_probe_min_us = METRICS.input_probe_net_min_us.load(Ordering::Relaxed);
    let input_probe_last_us = METRICS.input_probe_net_last_us.load(Ordering::Relaxed);
    let input_probe_ms = if input_probe_count == 0 {
        "?/?".to_owned()
    } else {
        format!("{}/{}", (input_probe_last_us / 1000).min(999), (input_probe_min_us / 1000).min(999))
    };
    // Receive batch + pump depth: the two numbers that would show a
    // burst-sized stall. rx-b is packets drained per receive call;
    // pump is the worst full pump-cycle duration; sctp-out is the worst
    // number of outgoing datagrams flushed in one cycle (proxy for an
    // SCTP pending-queue backlog — the queue depth itself has no public
    // accessor in the rtc fork).
    let rx_batch_count = METRICS.rx_batch_count.swap(0, Ordering::Relaxed);
    let rx_batch_avg = if rx_batch_count == 0 {
        0
    } else {
        METRICS.rx_batch_sum.swap(0, Ordering::Relaxed) / rx_batch_count
    };
    if rx_batch_count == 0 {
        let _ = METRICS.rx_batch_sum.swap(0, Ordering::Relaxed);
    }
    let rx_batch_max = METRICS.rx_batch_max.swap(0, Ordering::Relaxed);
    let pump_max_us = METRICS.pump_max_us.swap(0, Ordering::Relaxed);
    // Per-stage pump timing: the worst cycle of the window and the
    // standing average of every cycle in it. The max alone would show a
    // single slow cycle; the average says whether the cost is a standing
    // one or a spike.
    let mut pump_stage_max = [0u64; PUMP_STAGE_COUNT];
    let mut pump_stage_avg = [0u64; PUMP_STAGE_COUNT];
    let mut pump_stage_sum = [0u64; PUMP_STAGE_COUNT];
    for stage in 0..PUMP_STAGE_COUNT {
        let count = METRICS.pump_stage_count[stage].swap(0, Ordering::Relaxed);
        let sum = METRICS.pump_stage_sum_us[stage].swap(0, Ordering::Relaxed);
        pump_stage_max[stage] = METRICS.pump_stage_max_us[stage].swap(0, Ordering::Relaxed);
        pump_stage_sum[stage] = sum;
        pump_stage_avg[stage] = if count == 0 { 0 } else { sum / count };
    }
    let pump_stages_max = format_pump_stages(&pump_stage_max);
    let pump_stages_avg = format_pump_stages(&pump_stage_avg);
    // How many packets came in during the window, and how many came with
    // the slowest receive cycle. Together with the per-stage timings these
    // say whether the receive path is expensive per packet or only
    // expensive when the server sends in bursts.
    let rx_packets = METRICS.rx_packets.swap(0, Ordering::Relaxed);
    let recv_packed = METRICS.recv_cycle_packed.swap(0, Ordering::Relaxed);
    let recv_cycle_us = recv_packed >> 20;
    let recv_cycle_packets = recv_packed & 0xF_FFFF;
    // The bitrate this client can actually keep up with, from the receive
    // cost measured in this window.
    let sctp_out_count = METRICS.sctp_out_count.swap(0, Ordering::Relaxed);
    let sctp_out_avg = if sctp_out_count == 0 {
        0
    } else {
        METRICS.sctp_out_sum.swap(0, Ordering::Relaxed) / sctp_out_count
    };
    if sctp_out_count == 0 {
        let _ = METRICS.sctp_out_sum.swap(0, Ordering::Relaxed);
    }
    let sctp_out_max = METRICS.sctp_out_max.swap(0, Ordering::Relaxed);
    // How often the input is actually leaving the client, in reports per
    // second. This is the number that says whether the input wire is
    // dropping reports: the controller is polled at 250 Hz, and a
    // healthy wire carries them at something close to that. A falling
    // rate is a lossy channel, which is invisible in every other number
    // on this screen because `input_sent` was counted and thrown away.
    //
    // The wire wait is also the client's half of the input delay: the gap
    // between a report being queued and its packet hitting the socket.
    let input_rate = METRICS.input_sent.swap(0, Ordering::Relaxed).min(999);
    let input_wire_count = METRICS.input_wire_count.swap(0, Ordering::Relaxed);
    let input_wire_avg_us = METRICS
        .input_wire_sum_us
        .swap(0, Ordering::Relaxed)
        .checked_div(input_wire_count)
        .unwrap_or(0);
    let input_wire_max_us = METRICS.input_wire_max_us.swap(0, Ordering::Relaxed);
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
    // socket and the speaker - the jitter buffer, the decoded audio
    // waiting for the device, and the device queue. This is the client's
    // own share of the audio's latency, and the picture's age next to it
    // is the client's share of the video's. Nothing is counted twice any
    // more: the device queue used to appear in both the counters and the
    // queue depth, which inflated the number and made `ad` the trim's
    // own shadow.
    let audio_delay_ms = (client_audio_delay_samples() * 1_000 / 48_000) as i64;
    let audio_delay_ms = audio_delay_ms.clamp(0, 999);
    // How much that delay changed over the last minute. Negative means
    // the sound is catching up with the picture, which is what makes the
    // picture look later and later the longer the session runs. This can
    // only move what the hoard holds; it is no longer the change in a
    // number that the trim drives down.
    let _audio_delay_drift_ms = (METRICS.audio_delay_drift_us.load(Ordering::Relaxed) / 1000).clamp(-999, 999);
    // The server's audio pipeline against our wall clock, the same number
    // the video's `srv` is. Between them they say which side the picture
    // is drifting against.
    let audio_offset_ms = (METRICS.audio_offset_us.load(Ordering::Relaxed) / 1000).clamp(-999, 999);
    // The picture's absolute age: the server's own stretch (`srv`) plus
    // the client's display age. This replaces the keyframe probe, which
    // cost a hitch every 10 seconds to measure the same thing.
    let picture_age_ms = (server_offset_ms.max(0) + METRICS.frame_age_last_us.load(Ordering::Relaxed) as i64 / 1000).clamp(0, 999);
    // The holes in the sound, in milliseconds. Each one is the sound
    // jumping forward in game time, which is the whole of the drift the
    // player feels.
    let holes_ms = METRICS.audio_holes_ms.load(Ordering::Relaxed).clamp(-999, 999);
    // The longest the client held a damaged frame waiting for its missing
    // packets, and the number of times the picture lost ground with the
    // sound in one step.
    let av_stalls = METRICS.av_stalls.load(Ordering::Relaxed).min(999);
    let audio_lost_ms = METRICS.audio_lost_ms.load(Ordering::Relaxed).clamp(-999, 999);
    format!(
        "fps d/p:{}/{} us r:{rtp_average}/{rtp_max} d/a:{}/{} up/gpu:{}/{} pr/loop:{}/{} skip:{} repl:{} q:{} slot:{} rs:{} rst:{} stut:{av_stalls} a-def:{audio_lost_ms} in-rate:{input_rate}/s in-wire:{input_wire_avg_us}/{input_wire_max_us}us rx-b:{rx_batch_avg}/{rx_batch_max} rxk:{rx_packets}/s pump:{pump_max_us}us bat:{recv_cycle_us}us/{recv_cycle_packets} sctp:{sctp_out_avg}/{sctp_out_max}\nhandoff:{} ui-ovr:{} d>h/h>u:{}/{}us h>u:{}/{}us age:{age_average}/{}us in-lag:{input_lag_average}/{}us kf:{} pres:{} gap:{present_gap_average}/{}us aud:{audio_buffer_average}/{}ms ars:{} flush-gap:{}us a-srv:{audio_offset_ms}ms\nsrv:{server_offset_ms}ms drift:{server_drift_ms}ms av-lag:{av_lag_ms}ms ld:{av_lag_drift_ms}ms av-d:{av_out_ms}ms a-del:{audio_delay_ms}ms holes:{holes_ms}ms pic:{picture_age_ms}ms ip:{input_probe_ms}ms\npmx f/so/hd/e/m/d/c/s:{pump_stages_max} pma:{pump_stages_avg}",
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
        decode_to_handoff_average,
        METRICS.decode_to_handoff_max_us.swap(0, Ordering::Relaxed),
        handoff_to_upload_average,
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
