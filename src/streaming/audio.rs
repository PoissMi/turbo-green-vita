use anyhow::{Context, Result, bail};
use bytes::Bytes;
use sdl2::audio::{AudioQueue, AudioSpecDesired};
use std::collections::VecDeque;
use std::ptr::NonNull;
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel};

pub const AUDIO_SAMPLE_RATE: i32 = 48_000;
const AUDIO_CHANNELS: usize = 2;

const AUDIO_BYTES_PER_SECOND: u32 = AUDIO_SAMPLE_RATE as u32 * AUDIO_CHANNELS as u32 * 2;
/// Hard cap on the audio queued in the device.
///
/// Whatever sits in here is audio the player has not heard yet but will:
/// the queue depth is the client's own share of the audio's latency.
///
/// It was 120 ms, and together with the jitter buffer in front of it the
/// client held 230 ms of sound before a single sample reached the
/// speaker. The picture reaches the screen 17 ms after it arrives, so the
/// sound was a fifth of a second older than the picture it belongs to -
/// and the player heard the sound of an action long after they saw it.
///
/// 50 ms is enough to absorb the 15 ms pump interval and the 20 ms Opus
/// frame the decoder hands over, without being a buffer the player can
/// hear.
const MAX_QUEUED_AUDIO_BYTES: u32 = AUDIO_BYTES_PER_SECOND * 50 / 1_000;
/// The delay the client aims for.
///
/// Everything between the socket and the speaker is latency the player
/// feels. The sound arrives later than the picture, so the player sees
/// the action and hears it a quarter of a second later - and the longer
/// the client holds that audio, the further the sound falls behind the
/// picture and the more the picture looks late.
///
/// The client used to hold every millisecond it was given and never give
/// any of it back: 240 ms of audio sat in the device queue and the
/// decoder's output while the picture was 34 ms old. Nothing ever threw
/// the old audio away, so the gap was there for the whole session.
///
/// When the audio in the system is older than this, the oldest of it is
/// dropped. The sound then skips ahead to meet the picture instead of
/// trailing it, and the client stops adding latency of its own. The
/// price is a short gap in the sound, once, shortly after the stream
/// starts - a far better trade than a quarter of a second of permanent
/// lag.
///
/// It cannot go much below this: the jitter buffer alone holds 40 ms of
/// audio and the client hands it to the device every 15 ms. A target
/// that the pipeline cannot reach would make the client trim on every
/// pass, and the sound would skip forward forever.
const AUDIO_MAX_DELAY_MS: u64 = 120;
/// The delay above which the client starts giving audio back.
///
/// The gap between this and the target is the dead band: the client does
/// not trim for a jitter buffer that swells and shrinks by a few
/// milliseconds, only when it is really hoarding.
const AUDIO_TRIM_ABOVE_MS: u64 = 200;
/// How much audio to buffer before starting playback.
///
/// This used to be 80 ms, which put the sound roughly a third of a second
/// behind the picture (the video reaches the screen ~30 ms after it arrives).
/// The brain anchors perceived latency on the sound, so this buffer was the
/// largest remaining latency source in the client.
const AUDIO_START_BUFFER_BYTES: u32 = AUDIO_BYTES_PER_SECOND * 25 / 1_000;
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
            crate::streaming::video::metrics::METRICS
                .audio_dropped_packets
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
                        // time. This one really is lost.
                        crate::streaming::video::metrics::METRICS
                            .audio_dropped_packets
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
        // the target. This is the one place where the client gives audio
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
                eprintln!("Failed to queue SDL audio: {error}");
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

    /// Gives the oldest audio back until the client's own delay is back
    /// inside the target.
    ///
    /// The decoded audio that has not been handed to the device yet is
    /// the oldest of it that can be dropped without stalling the sound,
    /// so that is where the trim starts. Only when the pipeline is empty
    /// and the delay is still too large is the device queue emptied as
    /// well - and only when emptying it would actually bring the client
    /// inside the target, because emptying it while the pipeline is
    /// bigger than the target would make the sound stumble for nothing.
    ///
    /// The gap this leaves in the sound is the price. It is a hole in the
    /// audio timeline, so the sound after it plays early - but the whole
    /// client stops adding a quarter of a second of its own latency,
    /// which is the larger prize.
    fn trim_to_target(&mut self) {
        use crate::streaming::video::metrics::{METRICS, client_audio_delay_samples};
        let target = AUDIO_MAX_DELAY_MS * 48;
        if client_audio_delay_samples() <= AUDIO_TRIM_ABOVE_MS * 48 {
            return;
        }
        while client_audio_delay_samples() > target {
            let Some(samples) = self.pending_pcm.pop_front() else {
                break;
            };
            let dropped = (samples.len() / AUDIO_CHANNELS) as u64;
            METRICS
                .audio_trimmed_samples
                .fetch_add(dropped, std::sync::atomic::Ordering::Relaxed);
        }
        let delay = client_audio_delay_samples();
        if delay <= target {
            return;
        }
        let queued_samples =
            (self.queue.size() / size_of::<i16>() as u32 / AUDIO_CHANNELS as u32) as u64;
        if queued_samples == 0 {
            return;
        }
        // What is left in the pipeline on its own. If that is already
        // more than the target, emptying the device queue would not help.
        if delay.saturating_sub(queued_samples) > target {
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
                        eprintln!("Failed to decode Opus audio packet: {error}");
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
