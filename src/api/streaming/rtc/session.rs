use crate::api::streaming::rtc::media::{AudioReceiver, VideoReceiver};
use crate::api::streaming::rtc::transport::RtcTransport;
use crate::streaming::input::{GamepadFrame, PointerEvent};
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
use std::sync::Arc;
use std::time::{Duration, Instant};

const KEYFRAME_REQUEST_COOLDOWN: Duration = Duration::from_millis(120);
/// How often a fresh picture is asked for to measure how long the picture
/// takes to come back. The request is a real one, so it costs a keyframe on
/// the wire; the interval keeps that rare enough to not disturb the stream.
const KEYFRAME_PROBE_INTERVAL: Duration = Duration::from_secs(10);
const BANDWIDTH_REPORT_INTERVAL: Duration = Duration::from_secs(1);
const INITIAL_VIDEO_GRACE: Duration = Duration::from_millis(500);
const INITIAL_VIDEO_KEYFRAME_INTERVAL: Duration = Duration::from_millis(500);
/// The server is never asked for less than this, so a struggling path
/// degrades in quality instead of collapsing the stream.
const MIN_BANDWIDTH_ESTIMATE_KBPS: u32 = 1500;
/// The server's video timeline slipping by more than this over a minute means
/// a queue is growing in front of us.
///
/// It was 25 ms, so a picture getting 25 ms older in a minute was left
/// alone while it accumulated. At 6 ms/min the user already feels it, so
/// the threshold is 10: a queue that grows is grown by the rate the server
/// is asked for, and that is the one thing this client controls.
const DRIFT_BACKOFF_MS: i64 = 10;
/// Below this the path is keeping up, so the estimate may climb again.
const DRIFT_RECOVER_MS: i64 = 5;
/// The video falling this far behind the audio over a minute means the
/// server's encoder is not keeping up with the rate we ask for.
const AV_LAG_BACKOFF_MS: i64 = 10;
/// Below this the video may be climbing back towards the audio.
const AV_LAG_RECOVER_MS: i64 = 5;
/// The frame rate we advertise to the server.
///
/// The server does not take this from what we ask: the upstream client
/// asked for 30 fps and was sent 60, and this client asks for 60 and is
/// sent 60. The number is still advertised - the capabilities message and
/// the control channel's video preference both carry it, and the SDP's
/// level has to match it - but it is not a choice any more, so it is not
/// a setting.
pub(crate) const REQUESTED_VIDEO_FPS: u32 = 60;

pub(crate) struct RtcSessionConfig {
    pub stun_server: &'static str,
    pub route_probe: &'static str,
    pub audio_sample_rate: u32,
    pub audio_payload_type: u8,
    pub max_video_bitrate_kbps: u32,
    /// The frame rate asked for from the server. Shown in the HUD so the
    /// requested rate can be compared with the one the server delivers.
    pub video_fps: u32,
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
    /// The frame rate we asked the server for.
    video_fps: u32,
    /// What we currently ask the server for. The user's setting is the
    /// ceiling; this is what the path has actually proven it can carry.
    bandwidth_estimate_kbps: u32,
    last_bandwidth_report: Option<Instant>,
    last_keyframe_request: Option<Instant>,
    /// When the last keyframe probe was fired, so the next one is spaced out.
    last_keyframe_probe: Option<Instant>,
    initial_video_watchdog_started_at: Option<Instant>,
    last_initial_video_keyframe_request: Option<Instant>,
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
            video_fps: config.video_fps,
            bandwidth_estimate_kbps: config.max_video_bitrate_kbps,
            last_bandwidth_report: None,
            last_keyframe_request: None,
            last_keyframe_probe: None,
            initial_video_watchdog_started_at: None,
            last_initial_video_keyframe_request: None,
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
        self.transport.flush(&mut self.peer).await;
        self.transport.receive(&mut self.peer);
        let gathered_candidates = self.handle_peer_events();
        let mut keyframe_requested = self.handle_peer_messages();
        // Drain AFTER receiving: decode results produced since the last pump
        // must be picked up in the same iteration so the worker's frame
        // queue sees them before the next pump overwrites them. Draining
        // before receive stranded them one pump cycle and counted as `repl`.
        self.video.drain_decoder(&mut keyframe_requested);

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
        // Ask for a fresh picture every so often: the round trip is the
        // only way to see how old the picture is when it lands, because the
        // frame age the HUD reports starts when the packet leaves the
        // socket buffer.
        if self.keyframe_probe_due(now) {
            keyframe_requested = true;
        }
        self.request_keyframe(keyframe_requested, now);
        self.video.flush_nacks(&mut self.peer);
        if self
            .last_bandwidth_report
            .is_none_or(|previous| now.duration_since(previous) >= BANDWIDTH_REPORT_INTERVAL)
        {
            self.last_bandwidth_report = Some(now);
            let estimate = self.next_bandwidth_estimate();
            self.video.send_bandwidth_estimate(&mut self.peer, (estimate * 1000) as f32);
        }
        if let Some(status) = self.video.status(now) {
            let server = self
                .backend
                .server_video_size()
                .map(|(width, height)| format!("srv:{width}x{height}"))
                .unwrap_or_else(|| "srv:?".to_owned());
            self.status = format!(
                "cap:{}kbps fps:{} {server} {status}",
                self.max_video_bitrate_kbps, self.video_fps
            );
            eprintln!("{}", self.status);
        }

        Ok(gathered_candidates)
    }

    /// Decides what to ask the server for next.
    ///
    /// The server sizes its encoder from our REMB. If it then sends faster
    /// than the path can carry, the video piles up in a queue in front of us:
    /// every frame arrives later than the one before, so the picture falls
    /// steadily behind reality while every client-side number still looks
    /// perfect. The input suffers with it, because the server reads the
    /// controller from behind the same backlog - which is exactly the "the
    /// character reacts late" symptom.
    ///
    /// `server_drift_us` measures that pile-up directly, so the estimate can
    /// back off instead of asking for a rate the path cannot sustain. The
    /// user's setting stays the ceiling: the estimate climbs back towards it
    /// whenever the path keeps up.
    fn next_bandwidth_estimate(&mut self) -> u32 {
        // The user's setting is the ceiling. Nothing clamps it: the bitrate
        // the server is asked for is exactly what was chosen.
        let ceiling = self
            .max_video_bitrate_kbps
            .max(MIN_BANDWIDTH_ESTIMATE_KBPS);
        let drift_ms = crate::streaming::video::metrics::METRICS
            .server_drift_us
            .load(std::sync::atomic::Ordering::Relaxed)
            / 1000;
        // How much the video has fallen behind the audio over the last
        // minute. The audio is mixed from the game clock and the video from
        // the same clock, so the two drifting apart means the video is
        // being delivered from an older point in the game - the encoder
        // cannot keep up.
        let av_drift_ms = crate::streaming::video::metrics::METRICS
            .av_lag_drift_us
            .load(std::sync::atomic::Ordering::Relaxed)
            / 1000;
        let current = self
            .bandwidth_estimate_kbps
            .max(MIN_BANDWIDTH_ESTIMATE_KBPS);
        // The governor reacts to the picture falling *further* behind, not
        // to it being behind in the first place.
        //
        // A server that is simply slow shows a constant delay, and a
        // constant delay is not something a lower bitrate can fix - so
        // reacting to it would only throw the picture away for nothing.
        // What a lower bitrate can fix is a delay that keeps growing,
        // because that is the encoder falling behind its own clock.
        let struggling = drift_ms > DRIFT_BACKOFF_MS || av_drift_ms > AV_LAG_BACKOFF_MS;
        let healthy = drift_ms < DRIFT_RECOVER_MS && av_drift_ms < AV_LAG_RECOVER_MS;
        let next = if struggling {
            // The queue is growing: ask for noticeably less than we get.
            (current * 85 / 100).max(MIN_BANDWIDTH_ESTIMATE_KBPS)
        } else if healthy {
            // The path is keeping up: climb back towards the ceiling.
            current
                .saturating_add(ceiling.saturating_sub(current) / 8)
                .saturating_add(50)
                .min(ceiling)
        } else {
            current
        };
        self.bandwidth_estimate_kbps = next;
        crate::streaming::video::metrics::METRICS
            .bandwidth_estimate_kbps
            .store(u64::from(next), std::sync::atomic::Ordering::Relaxed);
        next
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

    /// Whether a fresh picture should be asked for to measure the round trip.
    ///
    /// The request is a real one, so it costs a keyframe on the wire and
    /// the stream stutters a little when it lands. Every ten seconds is
    /// rare enough for that to be tolerable and often enough to give the
    /// number a few samples to settle on.
    fn keyframe_probe_due(&mut self, now: Instant) -> bool {
        if self.connection_state != RTCPeerConnectionState::Connected {
            return false;
        }
        if self
            .last_keyframe_probe
            .is_some_and(|last| now.duration_since(last) < KEYFRAME_PROBE_INTERVAL)
        {
            return false;
        }
        self.last_keyframe_probe = Some(now);
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
        // From now on the fresh picture this request brings back measures
        // how long a new picture takes to come back.
        let video_ts = self.video.last_video_timestamp().unwrap_or(0);
        crate::streaming::video::metrics::arm_keyframe_probe(now, video_ts);
    }
}
