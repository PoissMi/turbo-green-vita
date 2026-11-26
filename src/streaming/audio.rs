use crate::logging::{DEFAULT_INTERVAL, Throttle, throttled};
use anyhow::{Context, Result, bail};
use bytes::Bytes;
use sdl2::audio::{AudioQueue, AudioSpecDesired};
use std::collections::VecDeque;
use std::ptr::NonNull;
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel};

pub const AUDIO_SAMPLE_RATE: i32 = 48_000;
const AUDIO_CHANNELS: usize = 2;

/// The device queue refused a buffer. Throttled, because it happens per
/// buffer and the audio thread is on the critical path of the sound.
static LOG_QUEUE: Throttle = Throttle::new(DEFAULT_INTERVAL);
/// A packet the Opus decoder could not decode. Throttled for the same
/// reason.
static LOG_OPUS: Throttle = Throttle::new(DEFAULT_INTERVAL);

const AUDIO_BYTES_PER_SECOND: u32 = AUDIO_SAMPLE_RATE as u32 * AUDIO_CHANNELS as u32 * 2;
/// Hard cap on the audio queued in the device.
///
/// Whatever sits in here is audio the player has not heard yet but will:
/// the queue depth is the client's own share of the audio's latency.
///
/// It was 50 ms, and the client still held 282 ms of sound between the
/// socket and the speaker: the jitter buffer and the decoded pipeline
/// sit in front of a queue this shallow, so the queue was not the
/// 50 ms. It was 25 ms in 0.4.10-0.4.11 and the sound was terrible: the
/// shell hands audio to the device every 15 ms and the decoder hands it
/// over in 20 ms frames, so a queue at 25 ms is one pump interval deep
/// and the device starves in between. The standing backlog the user
/// measured was 282 ms, and almost none of it was here - it was in the
/// jitter buffer and the decoded pipeline. The queue is not where the
/// delay lives.
const MAX_QUEUED_AUDIO_BYTES: u32 = AUDIO_BYTES_PER_SECOND * 50 / 1_000;
/// The delay the client is willing to carry, used as a ceiling.
///
/// The client's own audio delay - everything from the socket to the
/// speaker - is allowed to grow up to this and no further. It is a
/// ceiling, not a target: the client never needs the delay to *be*
/// this, it just refuses to let it exceed it, because a deeper buffer
/// than this only makes the sound older than the picture by more than
/// the player can tell apart.
///
/// The earlier code trimmed toward a target: as soon as the delay was
/// above 200 ms it threw audio away until it was below 120 ms, so
/// every pump over the threshold punched an 80 ms hole in the sound.
/// Each hole is the sound jumping forward in game time, which is
/// exactly the drift the player felt: the sound got ahead of the
/// picture a little more every time the trim fired, and the picture
/// looked later and later for the rest of the session. A trim that
/// discards to catch up *is* the lag.
///
/// This is now a ceiling with a wide dead band below it. The audio
/// between the two levels is not trimmed at all, and when the ceiling
/// is hit only the audio already played is thrown away - the decoded
/// pipeline is never deleted, because that is a hole in the sound and
/// the holes are the drift.
///
/// It cannot go much below this: the jitter buffer alone holds 40 ms of
/// audio and the shell hands it to the device every 15 ms. A ceiling
/// that the pipeline cannot reach would make the client trim on every
/// pass, and the sound would skip forward forever.
const AUDIO_MAX_DELAY_MS: u64 = 180;
/// The delay above which the client starts giving audio back.
///
/// The gap between this and the ceiling is the dead band. The client
/// does not act for ordinary jitter - the jitter buffer swells and
/// shrinks by a few milliseconds every pump - only when it is really
/// hoarding. It was 200 ms against a 120 ms ceiling, an 80 ms band
/// that was narrower than one pump's worth of audio, so the trim fired
/// on jitter alone.
const AUDIO_TRIM_ABOVE_MS: u64 = 260;
/// How much audio to buffer before starting playback.
///
/// This used to be 80 ms, which put the sound roughly a third of a second
/// behind the picture (the video reaches the screen ~30 ms after it
/// arrives). The brain anchors perceived latency on the sound, so this
/// buffer was the largest remaining latency source in the client.
///
/// It must stay well below `MAX_QUEUED_AUDIO_BYTES`. The 0.4.10 cut put
/// both at 25 ms, so the queue could never exceed the start threshold,
/// the `>=` test that resumes the device never passed, and the client
/// was silent from the first frame: the cap and the threshold have to be
/// different numbers, not the same one.
const AUDIO_START_BUFFER_BYTES: u32 = AUDIO_BYTES_PER_SECOND * 15 / 1_000;
const MAX_OPUS_FRAME_SAMPLES_PER_CHANNEL: usize = 5_760;
const MAX_PENDING_OPUS_PACKETS: usize = 32;
const MAX_PENDING_PCM_BUFFERS: usize = 8;
/// Audio waiting for the decoder, and audio waiting for the device.
///
/// Nothing here is thrown away. A missing piece of audio is a hole in the
/// audio timeline: the sound then plays on ahead of the picture by exactly
/// the length of the hole, and the player reads that as a late video. So
/// the audio waits instead - late audio is only late, lost audio is early.
const MAX_HELD_AUDIO_PACKETS: usize = 256;
const MAX_HELD_PCM_BUFFERS: usize = 64;

const OPUS_OK: i32 = 0;

#[repr(C)]
struct OpusDecoderState {
    _private: [u8; 0],
}

#[link(name = "opus", kind = "static")]
unsafe extern "C" {
    fn opus_decoder_create(
        sample_rate: i32,
        channels: i32,
        error: *mut i32,
    ) -> *mut OpusDecoderState;
    fn opus_decode(
        decoder: *mut OpusDecoderState,
        data: *const u8,
        length: i32,
        pcm: *mut i16,
        frame_size: i32,
        decode_fec: i32,
    ) -> i32;
    fn opus_decoder_destroy(decoder: *mut OpusDecoderState);
}

struct NativeOpusDecoder {
    state: NonNull<OpusDecoderState>,
}

unsafe impl Send for NativeOpusDecoder {}

impl NativeOpusDecoder {
    fn new() -> Result<Self> {
        let mut error = OPUS_OK;
        // SAFETY: libopus initializes and exclusively owns the returned opaque decoder state.
        let state =
            unsafe { opus_decoder_create(AUDIO_SAMPLE_RATE, AUDIO_CHANNELS as i32, &mut error) };
        if error != OPUS_OK {
            if !state.is_null() {
                // SAFETY: a non-null state returned by libopus must be released with this function.
                unsafe { opus_decoder_destroy(state) };
            }
            bail!("libopus failed to create a decoder: error {error}");
        }
        let state = NonNull::new(state).context("libopus returned a null decoder")?;
        Ok(Self { state })
    }

    fn decode(&mut self, packet: &[u8], pcm: &mut [i16]) -> Result<usize> {
        let packet_len = i32::try_from(packet.len()).context("Opus packet is too large")?;
        // SAFETY: `state` is a live decoder, and `pcm` has room for the maximum Opus frame.
        let decoded = unsafe {
            opus_decode(
                self.state.as_ptr(),
                packet.as_ptr(),
                packet_len,
                pcm.as_mut_ptr(),
                MAX_OPUS_FRAME_SAMPLES_PER_CHANNEL as i32,
                0,
            )
        };
        if decoded < OPUS_OK {
            bail!("libopus decode error {decoded}");
        }
        Ok(decoded as usize)
    }
}

impl Drop for NativeOpusDecoder {
    fn drop(&mut self) {
        // SAFETY: this is the sole owner and the state has not previously been destroyed.
        unsafe { opus_decoder_destroy(self.state.as_ptr()) };
    }
}

pub struct AudioRenderer {
    queue: AudioQueue<i16>,
    packets_tx: SyncSender<Bytes>,
    samples_rx: Receiver<Vec<i16>>,
    /// Opus packets that have not reached the decoder yet.
    pending_packets: VecDeque<Bytes>,
    /// Decoded audio that has not reached the device yet.
    pending_pcm: VecDeque<Vec<i16>>,
    started: bool,
}

impl AudioRenderer {
    pub fn new(audio: &sdl2::AudioSubsystem) -> Result<Self> {
        let desired = AudioSpecDesired {
            freq: Some(AUDIO_SAMPLE_RATE),
            channels: Some(AUDIO_CHANNELS as u8),
            // 512 frames is 10.7 ms at 48 kHz. The previous 1024 frame
            // buffer added another 10.7 ms of latency for no benefit.
            samples: Some(512),
        };
        let queue = audio
            .open_queue(None, &desired)
            .map_err(anyhow::Error::msg)
            .context("failed to open SDL audio queue")?;
        let spec = queue.spec();
        if spec.freq != AUDIO_SAMPLE_RATE || spec.channels != AUDIO_CHANNELS as u8 {
            eprintln!(
                "SDL audio opened as {} Hz / {} channel(s), requested {} Hz / {} channel(s)",
                spec.freq, spec.channels, AUDIO_SAMPLE_RATE, AUDIO_CHANNELS
            );
        }
        let (packets_tx, samples_rx) = spawn_decode_worker()?;

        Ok(Self {
            queue,
            packets_tx,
            samples_rx,
            pending_packets: VecDeque::new(),
            pending_pcm: VecDeque::new(),
            started: false,
        })
    }

    pub fn submit_packets(&mut self, packets: Vec<Bytes>) {
        if self.started && self.queue.size() == 0 {
            self.queue.pause();
            self.started = false;
        }

        // Nothing is thrown away. A missing piece of audio is a hole in the
        // audio timeline, and the sound then plays on ahead of the picture
        // by exactly the length of the hole - which the player reads as a
        // late video. So the audio waits instead.
        self.pending_packets.extend(packets);
        while self.pending_packets.len() > MAX_HELD_AUDIO_PACKETS {
            self.pending_packets.pop_front();
            let metrics = &crate::streaming::video::metrics::METRICS;
            metrics
                .audio_dropped_packets
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            // A held packet that was never decoded is a hole: the sound
            // jumps forward by its length and the picture looks later by
            // the same amount. Count the samples, not just the packet, so
            // `a-def` and `holes` see it.
            metrics
                .audio_dropped_samples
                .fetch_add(960, std::sync::atomic::Ordering::Relaxed);
        }

        // Hand the audio to the decoder, and keep whatever does not fit.
        while let Some(packet) = self.pending_packets.pop_front() {
            match self.packets_tx.try_send(packet) {
                Ok(()) => {}
                Err(TrySendError::Full(packet)) => {
                    self.pending_packets.push_front(packet);
                    break;
                }
                Err(TrySendError::Disconnected(packet)) => {
                    eprintln!("Audio decode worker stopped; restarting");
                    self.restart_decode_worker();
                    let _ = self.packets_tx.try_send(packet);
                }
            }
        }

        // Collect the decoded audio.
        loop {
            match self.samples_rx.try_recv() {
                Ok(samples) => {
                    if self.pending_pcm.len() >= MAX_HELD_PCM_BUFFERS {
                        // More audio than the device can take for a long
                        // time. This one really is lost, and it is a hole
                        // in the sound, so count the samples too.
                        let metrics = &crate::streaming::video::metrics::METRICS;
                        let count = (samples.len() / AUDIO_CHANNELS) as u64;
                        metrics
                            .audio_dropped_packets
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        metrics
                            .audio_dropped_samples
                            .fetch_add(count, std::sync::atomic::Ordering::Relaxed);
                        continue;
                    }
                    self.pending_pcm.push_back(samples);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    eprintln!("Audio decode worker output disconnected; restarting");
                    self.restart_decode_worker();
                    break;
                }
            }
        }

        // Nothing may sit in the client's audio pipeline for longer than
        // the ceiling. This is the one place where the client gives audio
        // back, and it is what keeps the sound from falling further and
        // further behind the picture over a long session. It runs before
        // the device is filled, so the queue is emptied and refilled with
        // the newest audio in the same pass - the old audio is missing,
        // but the sound does not stall.
        self.trim_to_target();

        // Feed the device while it has room.
        while let Some(samples) = self.pending_pcm.pop_front() {
            let sample_bytes = (samples.len() * size_of::<i16>()) as u32;
            if self.queue.size().saturating_add(sample_bytes) > MAX_QUEUED_AUDIO_BYTES {
                // The device queue is full. Hold the audio until it drains
                // instead of clearing it: clearing would throw away the
                // audio that is already queued, and the sound would jump
                // ahead of the picture by that much.
                self.pending_pcm.push_front(samples);
                break;
            }
            if let Err(error) = self.queue.queue_audio(&samples) {
                throttled(
                    &LOG_QUEUE,
                    format_args!("Failed to queue SDL audio: {error}"),
                );
            } else {
                // The audio that reached the device. The difference between
                // this and what was decoded is the audio still waiting for
                // its turn.
                crate::streaming::video::metrics::METRICS
                    .audio_played_samples
                    .fetch_add(
                        (samples.len() / AUDIO_CHANNELS) as u64,
                        std::sync::atomic::Ordering::Relaxed,
                    );
            }
        }
        if !self.started && self.queue.size() >= AUDIO_START_BUFFER_BYTES {
            self.queue.resume();
            self.started = true;
        }

        // How much audio is queued in the device right now. Together with the
        // RTP jitter buffer this is the audio side of the A/V desync: the
        // video is ~20 ms old when it hits the screen, so a deep audio queue
        // means the sound lags the picture by that much.
        let queued_ms = (self.queue.size() * 1_000) / AUDIO_BYTES_PER_SECOND;
        {
            let metrics = &crate::streaming::video::metrics::METRICS;
            let ordering = std::sync::atomic::Ordering::Relaxed;
            metrics.audio_buffer_ms.fetch_max(queued_ms as u64, ordering);
            metrics.audio_buffer_sum_ms.fetch_add(queued_ms as u64, ordering);
            metrics.audio_buffer_count.fetch_add(1, ordering);
            // How much audio is still waiting to be heard. The audio the
            // player hears right now is this much older than the audio the
            // server just sent, which is what the A/V comparison needs.
            // The counts are per channel, so they line up with the RTP
            // timestamps, which count one sample per channel as well.
            let queued_samples =
                (self.queue.size() / size_of::<i16>() as u32 / AUDIO_CHANNELS as u32) as u64;
            crate::streaming::video::metrics::observe_audio_queue(queued_samples);
            // The audio that was decoded but has not reached the device yet.
            let decoded = metrics.audio_decoded_samples.load(ordering);
            let played = metrics.audio_played_samples.load(ordering);
            metrics
                .audio_pipeline_samples
                .store(decoded.saturating_sub(played), ordering);
        }
    }

    /// Refuses to let the client's own audio delay grow past the ceiling,
    /// and never by taking audio the player has not heard yet.
    ///
    /// What it can drop is the audio that has *already been played*: it is
    /// in the device queue, behind the playhead, and removing it shortens
    /// the delay without leaving a hole in the middle of the stream. The
    /// decoded pipeline is never touched - it holds nothing but sound the
    /// player has not heard, and removing it is a hole, and a hole is the
    /// sound jumping forward in game time, which is exactly the drift the
    /// player felt building up as he played.
    ///
    /// It clears the device queue only when the delay is over the ceiling
    /// *and* clearing it would actually bring the delay back inside the
    /// ceiling - clearing it while the pipeline alone is already over the
    /// ceiling would do nothing but silence the sound.
    ///
    /// This used to delete decoded audio and left `a-del` and `holes` at
    /// their clamps while the speakers went silent: the player heard the
    /// trim cutting his own audio.
    ///
    /// Note what this cannot do. Both producers and consumers run in real
    /// time, so a pipeline that fills to the ceiling stays at the ceiling;
    /// nothing below can drain it, because the SDL device will not play
    /// faster than the audio arrives. What keeps the standing delay down
    /// is the queue cap below, not this trim. This trim only stops a
    /// backlog from making the sound older than the picture by more.
    fn trim_to_target(&mut self) {
        use crate::streaming::video::metrics::{METRICS, client_audio_delay_samples};
        let ceiling = AUDIO_MAX_DELAY_MS * 48;
        if client_audio_delay_samples() <= AUDIO_TRIM_ABOVE_MS * 48 {
            return;
        }
        let queued_samples =
            (self.queue.size() / size_of::<i16>() as u32 / AUDIO_CHANNELS as u32) as u64;
        if queued_samples == 0 {
            return;
        }
        // What is left in the pipeline on its own. If that is already
        // more than the ceiling, emptying the device queue would leave
        // the player in silence for nothing.
        let delay = client_audio_delay_samples();
        if delay.saturating_sub(queued_samples) > ceiling {
            return;
        }
        self.queue.clear();
        self.started = false;
        METRICS
            .audio_cleared_samples
            .fetch_add(queued_samples, std::sync::atomic::Ordering::Relaxed);
        METRICS
            .audio_resyncs
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    fn restart_decode_worker(&mut self) {
        self.queue.pause();
        self.queue.clear();
        self.started = false;
        match spawn_decode_worker() {
            Ok((packets_tx, samples_rx)) => {
                self.packets_tx = packets_tx;
                self.samples_rx = samples_rx;
            }
            Err(error) => eprintln!("Failed to restart audio decode worker: {error:#}"),
        }
    }
}

fn spawn_decode_worker() -> Result<(SyncSender<Bytes>, Receiver<Vec<i16>>)> {
    let (packets_tx, packets_rx) = sync_channel::<Bytes>(MAX_PENDING_OPUS_PACKETS);
    let (samples_tx, samples_rx) = sync_channel::<Vec<i16>>(MAX_PENDING_PCM_BUFFERS);

    let mut decoder = NativeOpusDecoder::new().context("failed to create Opus decoder")?;

    std::thread::Builder::new()
        .name("green-vita-audio-decode".to_owned())
        .spawn(move || {
            let mut decode_buf = vec![0i16; MAX_OPUS_FRAME_SAMPLES_PER_CHANNEL * AUDIO_CHANNELS];
            while let Ok(packet) = packets_rx.recv() {
                let samples_per_channel = match decoder.decode(&packet, &mut decode_buf) {
                    Ok(samples_per_channel) => samples_per_channel,
                    Err(error) => {
                        // One line per broken packet would be one line per
                        // 20 ms of sound; the decode thread has no budget
                        // for that.
                        throttled(
                            &LOG_OPUS,
                            format_args!("Failed to decode Opus audio packet: {error}"),
                        );
                        continue;
                    }
                };

                let sample_count = samples_per_channel * AUDIO_CHANNELS;
                // Counted per channel, so the numbers line up with the RTP
                // timestamps, which count one sample per channel too.
                crate::streaming::video::metrics::METRICS
                    .audio_decoded_samples
                    .fetch_add(samples_per_channel as u64, std::sync::atomic::Ordering::Relaxed);
                if samples_tx
                    .send(decode_buf[..sample_count].to_vec())
                    .is_err()
                {
                    break;
                }
            }
        })
        .context("failed to spawn audio decode worker")?;

    Ok((packets_tx, samples_rx))
}
