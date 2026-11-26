use crate::api::streaming::rtc::media::{AudioReceiver, VideoReceiver};
use crate::api::streaming::rtc::transport::RtcTransport;
use crate::streaming::input::{GamepadFrame, PointerEvent};
use crate::streaming::video::metrics::{
    PUMP_STAGE_CTRL, PUMP_STAGE_DECODE, PUMP_STAGE_EVENTS, PUMP_STAGE_FLUSH, PUMP_STAGE_HANDLE,
    PUMP_STAGE_MSGS, PUMP_STAGE_SOCKET, PUMP_STAGE_STATUS, record_pump_stage,
};
use crate::streaming::video::{DecoderConfig, DirectVideoOutput};
use anyhow::{Context, Result};
use rtc::peer_connection::RTCPeerConnection;
use rtc::peer_connection::event::{RTCDataChannelEvent, RTCPeerConnectionEvent, RTCTrackEvent};
use rtc::peer_connection::message::RTCMessage;
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::peer_connection::state::RTCPeerConnectionState;
use rtc::peer_connection::transport::RTCIceCandidateInit;
use rtc::rtp_transceiver::rtp_sender::RtpCodecKind;
use rtc::sansio::Protocol;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

const KEYFRAME_REQUEST_COOLDOWN: Duration = Duration::from_millis(120);
const INITIAL_VIDEO_GRACE: Duration = Duration::from_millis(500);
const INITIAL_VIDEO_KEYFRAME_INTERVAL: Duration = Duration::from_millis(500);
/// The frame rate we advertise to the server.
///
/// The server does not take this from what we ask: the upstream client
/// asked for 30 fps and was sent 60, and this client asks for 60 and is
/// sent 60. The number is still advertised - the capabilities message and
/// the control channel's video preference both carry it, and the SDP's
/// level has to match it - but it is not a choice any more, so it is not
/// a setting.
pub(crate) const REQUESTED_VIDEO_FPS: u32 = 60;

/// Where the 1 Hz status lines of a session are kept.
const STREAM_LOG_DIR: &str = "ux0:data/xcloud-rust";
const STREAM_LOG_PATH: &str = "ux0:data/xcloud-rust/stream.log";
/// How many status lines are kept in memory. At one line per second this
/// is an hour of session, which is far more than any session that has to
/// be looked at afterwards.
const MAX_STREAM_LOG_LINES: usize = 3600;

/// Records how long the stage that just ended took, and starts the next
/// one.
///
/// The cycle used to be timed as a whole, which says that it got slower
/// but not what inside it owns the time. The stages are the socket drain,
/// the message handling, the decoder drain, the control work and the
/// status line, and they are timed apart so the worst cycle can be
/// attributed to one of them.
#[inline]
fn record_stage(stage: usize, stage_started: &mut Instant) {
    let elapsed = stage_started.elapsed().as_micros() as u64;
    crate::streaming::video::metrics::record_pump_stage(stage, elapsed);
    *stage_started = Instant::now();
}

pub(crate) struct RtcSessionConfig {
    pub stun_server: &'static str,
    pub route_probe: &'static str,
    pub audio_sample_rate: u32,
    pub audio_payload_type: u8,
    pub max_video_bitrate_kbps: u32,
    /// Whether the session's status lines are written to the session log.
    pub log_stream: bool,
    pub decoder: DecoderConfig,
}

/// Provider-specific hooks invoked by the reusable RTC session.
pub(crate) trait RtcSessionBackend {
    fn handle_channel_open(
        &mut self,
        peer: &mut RTCPeerConnection,
        channel_id: rtc::data_channel::RTCDataChannelId,
    );
    fn handle_channel_message(
        &mut self,
        peer: &mut RTCPeerConnection,
        channel_id: rtc::data_channel::RTCDataChannelId,
        data: &[u8],
    );
    fn send_gamepad_frame(&mut self, peer: &mut RTCPeerConnection, frame: GamepadFrame) -> bool;
    fn send_pointer_event(&mut self, peer: &mut RTCPeerConnection, event: PointerEvent);
    fn notify_keyframe_requested(&mut self, peer: &mut RTCPeerConnection);
    fn server_video_size(&self) -> Option<(u32, u32)>;
}

pub(crate) struct RtcSession<B: RtcSessionBackend> {
    pub(crate) peer: RTCPeerConnection,
    pub(crate) transport: RtcTransport,
    pub(crate) backend: B,
    pub connection_state: RTCPeerConnectionState,
    pub(crate) video: VideoReceiver,
    pub(crate) audio: AudioReceiver,
    max_video_bitrate_kbps: u32,
    /// Whether the 1 Hz status lines are kept for the session log.
    ///
    /// The REMB governor that used to sit here is gone: over a whole
    /// session the delivered bitrate did not follow the estimate at all
    /// (`net = 2358 + 0.007 × rmb`, R² = 0.000), so the server ignores the
    /// REMB and the estimate only ever produced a number that looked like
    /// control without being any.
    log_stream: bool,
    last_keyframe_request: Option<Instant>,
    initial_video_watchdog_started_at: Option<Instant>,
    last_initial_video_keyframe_request: Option<Instant>,
    /// The 1 Hz status lines of this session, kept in memory.
    ///
    /// They used to be written to `ux0:data/xcloud-rust/stream.log` from the
    /// pump thread, once a second, with a flush after every line. That put
    /// a blocking SD-card write on the one thread that has to keep up with
    /// the incoming stream, and the file was the cheapest thing to take
    /// off that path. The lines are written once, when the session ends.
    stream_log_lines: VecDeque<String>,
    pub status: String,
}

impl<B: RtcSessionBackend> RtcSession<B> {
    /// The newest video timestamp that was received, so the input probe
    /// knows which pictures can only have been captured after the input.
    pub(crate) fn last_video_timestamp(&self) -> Option<u32> {
        self.video.last_video_timestamp()
    }

    pub async fn new(
        mut peer: RTCPeerConnection,
        backend: B,
        config: RtcSessionConfig,
        direct_output: Arc<DirectVideoOutput>,
    ) -> Result<Self> {
        let transport =
            RtcTransport::bind(&mut peer, config.stun_server, config.route_probe).await?;
        let video = VideoReceiver::new(config.decoder, direct_output)?;

        Ok(Self {
            peer,
            transport,
            backend,
            connection_state: RTCPeerConnectionState::New,
            video,
            audio: AudioReceiver::new(config.audio_sample_rate, config.audio_payload_type),
            max_video_bitrate_kbps: config.max_video_bitrate_kbps,
            log_stream: config.log_stream,
            last_keyframe_request: None,
            initial_video_watchdog_started_at: None,
            last_initial_video_keyframe_request: None,
            stream_log_lines: VecDeque::new(),
            status: "Negotiating WebRTC connection".to_owned(),
        })
    }

    pub fn create_offer(&mut self) -> Result<RTCSessionDescription> {
        let offer = self
            .peer
            .create_offer(None)
            .context("failed to create rtc offer")?;
        self.peer
            .set_local_description(offer.clone())
            .context("failed to set local rtc description")?;
        Ok(offer)
    }

    pub fn set_remote_answer(&mut self, sdp: impl Into<String>) -> Result<()> {
        let answer = RTCSessionDescription::answer(sdp.into());
        self.peer
            .set_remote_description(answer?)
            .context("failed to set remote rtc answer")
    }

    pub fn close(&mut self) -> Result<()> {
        // The session log is written here: the stream is over, so the
        // SD-card write can no longer delay a picture.
        self.write_stream_log();
        self.peer
            .close()
            .context("failed to close rtc peer connection")
    }

    pub fn add_remote_candidate(&mut self, candidate: RTCIceCandidateInit) -> Result<()> {
        self.peer
            .add_remote_candidate(candidate)
            .context("failed to add remote ICE candidate")
    }

    pub async fn pump(&mut self) -> Result<Vec<RTCIceCandidateInit>> {
        let pump_started = Instant::now();
        let mut stage_started = pump_started;

        self.transport.flush(&mut self.peer).await;
        record_stage(PUMP_STAGE_FLUSH, &mut stage_started);

        let (socket_us, handle_us) = self.transport.receive(&mut self.peer);
        record_pump_stage(PUMP_STAGE_SOCKET, socket_us);
        record_pump_stage(PUMP_STAGE_HANDLE, handle_us);
        stage_started = Instant::now();

        let gathered_candidates = self.handle_peer_events();
        record_stage(PUMP_STAGE_EVENTS, &mut stage_started);

        let mut keyframe_requested = self.handle_peer_messages();
        record_stage(PUMP_STAGE_MSGS, &mut stage_started);

        // Drain AFTER receiving: decode results produced since the last pump
        // must be picked up in the same iteration so the worker's frame
        // queue sees them before the next pump overwrites them. Draining
        // before receive stranded them one pump cycle and counted as `repl`.
        self.video.drain_decoder(&mut keyframe_requested);
        record_stage(PUMP_STAGE_DECODE, &mut stage_started);

        let now = Instant::now();
        // rtc-rs is sans-I/O, so its expired internal timer must be advanced by our pump.
        if let Some(deadline) = self.peer.poll_timeout()
            && now >= deadline
        {
            let _ = self.peer.handle_timeout(now);
        }
        if self.initial_video_keyframe_due(now) {
            eprintln!("No initial video RTP after WebRTC connected; requesting a keyframe");
            keyframe_requested = true;
        }
        self.request_keyframe(keyframe_requested, now);
        self.video.flush_nacks(&mut self.peer);
        record_stage(PUMP_STAGE_CTRL, &mut stage_started);

        if let Some(status) = self.video.status(now) {
            let server = self
                .backend
                .server_video_size()
                .map(|(width, height)| format!("srv:{width}x{height}"))
                .unwrap_or_else(|| "srv:?".to_owned());
            self.status = format!(
                "cap:{}kbps fps:{} {server} {status}",
                self.max_video_bitrate_kbps, REQUESTED_VIDEO_FPS
            );
            // The status line goes to the HUD, and into the in-memory
            // session log. Nothing is written to a file or to the console
            // from here: this thread has to keep up with the stream, and a
            // write syscall on it is time the picture does not get.
            self.record_status_line();
        }
        record_stage(PUMP_STAGE_STATUS, &mut stage_started);

        // Worst pump-cycle duration: a long cycle is video age added
        // proportional to the burst (bitrate) — this is the number that
        // would show a Cause-B stall while age/pr stay green.
        crate::streaming::video::metrics::METRICS
            .pump_max_us
            .fetch_max(pump_started.elapsed().as_micros() as u64, std::sync::atomic::Ordering::Relaxed);

        Ok(gathered_candidates)
    }

    /// Keeps the 1 Hz status line in memory.
    ///
    /// The line used to be written to disk from the pump thread. Writing it
    /// once per second is not a small cost when the file lives on the
    /// Vita's SD card: the write blocks until the card accepts it, and the
    /// pump thread is the one that has to drain the socket. The line is
    /// buffered here and written once, when the session ends.
    fn record_status_line(&mut self) {
        if !self.log_stream {
            return;
        }
        if self.stream_log_lines.is_empty() {
            // The first line of the log says what the session was: the cap
            // it asked for, and how fast the two user CPUs turned out to
            // be. The receive path runs on the faster one, and that ratio
            // is what decides how much of the bitrate the client can keep
            // up with.
            let header = format!(
                "# cap:{}kbps cpu:{}",
                self.max_video_bitrate_kbps,
                crate::streaming::video::metrics::cpu_benchmark_text()
            );
            self.stream_log_lines.push_back(header);
        }
        let secs = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or(0);
        self.stream_log_lines
            .push_back(format!("{secs} {}", self.status));
        while self.stream_log_lines.len() > MAX_STREAM_LOG_LINES {
            self.stream_log_lines.pop_front();
        }
    }

    /// Writes the buffered status lines to the session log.
    ///
    /// Called when the session ends, so the file write is off the path
    /// that has to keep up with the stream.
    fn write_stream_log(&mut self) {
        if self.stream_log_lines.is_empty() {
            return;
        }
        let mut text = String::with_capacity(self.stream_log_lines.len() * 700);
        for line in &self.stream_log_lines {
            text.push_str(line);
            text.push('\n');
        }
        self.stream_log_lines.clear();

        let _ = std::fs::create_dir_all(STREAM_LOG_DIR);
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(STREAM_LOG_PATH)
        {
            use std::io::Write;
            let _ = file.write_all(text.as_bytes());
            let _ = file.flush();
        }
    }

    fn initial_video_keyframe_due(&mut self, now: Instant) -> bool {
        if self.video.received_packet {
            self.initial_video_watchdog_started_at = None;
            self.last_initial_video_keyframe_request = None;
            return false;
        }
        if self.connection_state != RTCPeerConnectionState::Connected {
            self.initial_video_watchdog_started_at = None;
            self.last_initial_video_keyframe_request = None;
            return false;
        }

        let connected_at = *self.initial_video_watchdog_started_at.get_or_insert(now);
        if now.duration_since(connected_at) < INITIAL_VIDEO_GRACE {
            return false;
        }
        if self
            .last_initial_video_keyframe_request
            .is_some_and(|requested_at| {
                now.duration_since(requested_at) < INITIAL_VIDEO_KEYFRAME_INTERVAL
            })
        {
            return false;
        }

        self.last_initial_video_keyframe_request = Some(now);
        true
    }

    fn handle_peer_events(&mut self) -> Vec<RTCIceCandidateInit> {
        let mut gathered_candidates = Vec::new();
        while let Some(event) = self.peer.poll_event() {
            match event {
                RTCPeerConnectionEvent::OnConnectionStateChangeEvent(state) => {
                    self.connection_state = state;
                    self.status = format!("WebRTC connection: {state:?}");
                }
                RTCPeerConnectionEvent::OnIceCandidateEvent(ice_event) => {
                    if let Ok(candidate) = ice_event.candidate.to_json() {
                        gathered_candidates.push(candidate);
                    }
                }
                RTCPeerConnectionEvent::OnIceCandidateErrorEvent(error) => {
                    eprintln!(
                        "ICE candidate error from {} ({}:{}): {} {}",
                        error.url, error.address, error.port, error.error_code, error.error_text
                    );
                }
                RTCPeerConnectionEvent::OnTrack(RTCTrackEvent::OnOpen(init)) => {
                    let track_kind = self
                        .peer
                        .rtp_receiver(init.receiver_id)
                        .map(|receiver| receiver.track().kind());
                    match track_kind {
                        Some(RtpCodecKind::Video) => {
                            self.video.open(init.track_id, init.receiver_id, init.ssrc);
                            self.status = "Receiving video track".to_owned();
                        }
                        Some(RtpCodecKind::Audio) => {
                            self.audio.open(init.track_id);
                            self.status = "Receiving audio track".to_owned();
                        }
                        _ => {}
                    }
                }
                RTCPeerConnectionEvent::OnDataChannel(RTCDataChannelEvent::OnOpen(channel_id)) => {
                    self.backend.handle_channel_open(&mut self.peer, channel_id);
                }
                _ => {}
            }
        }
        gathered_candidates
    }

    fn handle_peer_messages(&mut self) -> bool {
        let mut keyframe_requested = false;
        while let Some(message) = self.peer.poll_read() {
            match message {
                RTCMessage::RtpPacket(track_id, packet) => {
                    if self.video.handles(&track_id) {
                        self.video.receive(packet, &mut keyframe_requested);
                    } else if self.audio.handles(&track_id) {
                        self.audio.receive(packet);
                    }
                }
                RTCMessage::DataChannelMessage(channel_id, data_message) => {
                    self.backend.handle_channel_message(
                        &mut self.peer,
                        channel_id,
                        &data_message.data,
                    );
                }
                _ => {}
            }
        }
        keyframe_requested
    }

    fn request_keyframe(&mut self, requested: bool, now: Instant) {
        let cooldown_elapsed = self.last_keyframe_request.is_none_or(|requested_at| {
            now.duration_since(requested_at) >= KEYFRAME_REQUEST_COOLDOWN
        });
        if !requested || !cooldown_elapsed {
            return;
        }

        self.last_keyframe_request = Some(now);
        crate::streaming::video::metrics::METRICS
            .keyframes_requested
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.backend.notify_keyframe_requested(&mut self.peer);
        self.video.request_keyframe(&mut self.peer);
    }
}

/// Writes the session log when the session goes away for any reason.
///
/// `close()` already does this on the normal path; this covers the ones
/// that end without it (the peer closed, the worker was torn down).
impl<B: RtcSessionBackend> Drop for RtcSession<B> {
    fn drop(&mut self) {
        self.write_stream_log();
    }
}
