use crate::api::streaming::rtc::rtp;
use crate::streaming::video::{DecodedFrame, DecoderConfig, DirectVideoOutput, VideoDecodeWorker};
use anyhow::Result;
use bytes::Bytes;
use rtc::media_stream::MediaStreamTrackId;
use rtc::peer_connection::RTCPeerConnection;
use rtc::rtp::Packet;
use rtc::rtp_transceiver::RTCRtpReceiverId;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

const STREAM_STATS_INTERVAL: Duration = Duration::from_secs(1);
/// Decoded frames kept for the UI to display, one per display frame.
const DECODED_QUEUE_DEPTH: usize = 4;

#[derive(Default)]
struct VideoStats {
    dropped: u64,
    decode_errors: u64,
    last_sample_duration_us: Option<u64>,
    encoded_resolution: Option<(u32, u32)>,
    nack_pending: Vec<u16>,
    nacks_sent: u64,
}

pub(crate) struct VideoReceiver {
    track_id: Option<MediaStreamTrackId>,
    receiver_id: Option<RTCRtpReceiverId>,
    ssrc: Option<u32>,
    rtp: rtp::VideoRtp,
    pub(crate) decoder: VideoDecodeWorker,
    /// Decoded frames waiting to be handed to the UI, oldest first.
    ///
    /// This used to hold a single frame and keep only the newest one, which
    /// threw away every frame that arrived between two display frames. The
    /// display flips faster than the source produces frames, so there is room
    /// to show them all - the UI takes them one per display frame.
    pub(crate) latest_frames: VecDeque<(u64, DecodedFrame)>,
    next_frame_id: u64,
    pub(crate) received_packet: bool,
    last_stats_report: Instant,
    bytes_since_report: u64,
    stats: VideoStats,
    direct_output: Arc<DirectVideoOutput>,
}

impl VideoReceiver {
    pub(crate) fn new(
        config: DecoderConfig,
        direct_output: Arc<DirectVideoOutput>,
    ) -> Result<Self> {
        Ok(Self {
            track_id: None,
            receiver_id: None,
            ssrc: None,
            rtp: rtp::VideoRtp::new(config.decode_width, config.decode_height),
            decoder: VideoDecodeWorker::spawn(config, Arc::clone(&direct_output))?,
            latest_frames: VecDeque::new(),
            next_frame_id: 0,
            received_packet: false,
            last_stats_report: Instant::now(),
            bytes_since_report: 0,
            stats: VideoStats::default(),
            direct_output,
        })
    }

    pub(crate) fn open(
        &mut self,
        track_id: MediaStreamTrackId,
        receiver_id: RTCRtpReceiverId,
        ssrc: u32,
    ) {
        self.track_id = Some(track_id);
        self.receiver_id = Some(receiver_id);
        self.ssrc = Some(ssrc);
    }

    pub(crate) fn handles(&self, track_id: &MediaStreamTrackId) -> bool {
        self.track_id.as_ref() == Some(track_id)
    }

    pub(crate) fn receive(&mut self, packet: Packet, keyframe_requested: &mut bool) {
        self.received_packet = true;
        self.bytes_since_report = self
            .bytes_since_report
            .saturating_add(packet.payload.len() as u64);
        let sample_stats = self.rtp.receive(&self.decoder, packet, keyframe_requested);
        // NACK is sent from `drain_decoder` (which has the peer); stash the
        // sequence numbers on VideoStats for one pump cycle.
        if !sample_stats.nack_sequence_numbers.is_empty() {
            self.stats.nack_pending
                .extend(sample_stats.nack_sequence_numbers.iter().copied());
        }
        self.stats.dropped = self
            .stats
            .dropped
            .saturating_add(sample_stats.dropped as u64);
        if sample_stats.source_frame_duration_us.is_some() {
            self.stats.last_sample_duration_us = sample_stats.source_frame_duration_us;
        }
        if sample_stats.encoded_resolution.is_some() {
            self.stats.encoded_resolution = sample_stats.encoded_resolution;
        }
    }

    pub(crate) fn drain_decoder(&mut self, keyframe_requested: &mut bool) {
        let mut decode_errors = 0u64;
        // Drain the full queue each pump so a burst doesn't strand frames.
        let drained: Vec<_> = self
            .decoder
            .latest_result
            .lock()
            .map(|mut queue| queue.drain(..).collect())
            .unwrap_or_default();
        // Keep every decoded frame, oldest first. The display flips faster
        // than the source produces frames, so frames that arrive between two
        // display frames still have a slot to be shown in. Only a full queue
        // drops anything, and it drops the oldest.
        for result in drained {
            match result {
                Ok(frame) => {
                    self.next_frame_id = self.next_frame_id.wrapping_add(1);
                    self.latest_frames.push_back((self.next_frame_id, frame));
                    while self.latest_frames.len() > DECODED_QUEUE_DEPTH {
                        self.latest_frames.pop_front();
                        crate::streaming::video::metrics::METRICS
                            .replaced
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                }
                Err(error) => {
                    eprintln!("Failed to decode H264 video frame: {error}");
                    decode_errors = decode_errors.saturating_add(1);
                    *keyframe_requested = true;
                }
            }
        }

        if decode_errors > 0 {
            self.stats.decode_errors = self.stats.decode_errors.saturating_add(decode_errors);
            self.decoder.reset_decoder();
            self.rtp.wait_for_keyframe();
        }
    }

    pub(crate) fn request_keyframe(&self, peer: &mut RTCPeerConnection) {
        // A PLI needs both identifiers recorded when the remote video track was opened.
        if let (Some(receiver_id), Some(ssrc)) = (self.receiver_id, self.ssrc)
            && let Some(mut receiver) = peer.rtp_receiver(receiver_id)
        {
            let pli = rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication {
                sender_ssrc: 0,
                media_ssrc: ssrc,
            };
            let _ = receiver.write_rtcp(vec![Box::new(pli)]);
        }
    }

    /// Send TransportLayerNack for sequence numbers flagged missing by the
    /// reassembler. Transport feedback (`nack`) is already negotiated in
    /// the SDP; previously it was never sent, so every Wi-Fi loss became
    /// a keyframe skip.
    pub(crate) fn flush_nacks(&mut self, peer: &mut RTCPeerConnection) {
        if self.stats.nack_pending.is_empty() {
            return;
        }
        let (Some(receiver_id), Some(ssrc)) = (self.receiver_id, self.ssrc) else {
            self.stats.nack_pending.clear();
            return;
        };
        let sequences = std::mem::take(&mut self.stats.nack_pending);
        // Group into NACK bitmap pairs: each pair names one missing sequence
        // plus a 16-bit bitmap for the next 16 positions. Sort + dedup first
        // so pairs are emitted in sequence order.
        let mut sorted = sequences.clone();
        sorted.sort_unstable();
        sorted.dedup();
        let mut nacks: Vec<rtcp::transport_feedbacks::transport_layer_nack::NackPair> = Vec::new();
        let mut i = 0;
        while i < sorted.len() {
            let pid = sorted[i];
            let mut bitmap: u16 = 0;
            let mut j = i + 1;
            while j < sorted.len() {
                let dist = sorted[j].wrapping_sub(pid);
                if dist == 0 || dist > 16 {
                    break;
                }
                bitmap |= 1u16 << (dist - 1);
                j += 1;
            }
            nacks.push(rtcp::transport_feedbacks::transport_layer_nack::NackPair {
                packet_id: pid,
                lost_packets: bitmap,
            });
            i = j;
        }
        if let Some(mut receiver) = peer.rtp_receiver(receiver_id) {
            let nack = rtcp::transport_feedbacks::transport_layer_nack::TransportLayerNack {
                sender_ssrc: 0,
                media_ssrc: ssrc,
                nacks,
            };
            let _ = receiver.write_rtcp(vec![Box::new(nack)]);
            self.stats.nacks_sent =
                self.stats.nacks_sent.saturating_add(sequences.len() as u64);
        }
    }

    /// Advertises receive bandwidth to the sender via REMB; xCloud uses this to size its encoder.
    pub(crate) fn send_bandwidth_estimate(&self, peer: &mut RTCPeerConnection, bitrate: f32) {
        if let (Some(receiver_id), Some(ssrc)) = (self.receiver_id, self.ssrc)
            && let Some(mut receiver) = peer.rtp_receiver(receiver_id)
        {
            let remb = rtcp::payload_feedbacks::receiver_estimated_maximum_bitrate::ReceiverEstimatedMaximumBitrate {
                sender_ssrc: 0,
                bitrate,
                ssrcs: vec![ssrc],
            };
            let _ = receiver.write_rtcp(vec![Box::new(remb)]);
        }
    }

    pub(crate) fn status(&mut self, now: Instant) -> Option<String> {
        let elapsed = now.duration_since(self.last_stats_report);
        if elapsed < STREAM_STATS_INTERVAL {
            return None;
        }
        self.last_stats_report = now;
        let elapsed_ms = elapsed.as_millis().max(1) as u64;
        let bitrate_kbps = self.bytes_since_report.saturating_mul(8) / elapsed_ms;
        self.bytes_since_report = 0;

        let performance = crate::streaming::video::video_performance_summary();
        let source_fps = self
            .stats
            .last_sample_duration_us
            .filter(|duration| *duration > 0)
            .map(|duration| 1_000_000 / duration)
            .unwrap_or(0);
        let encoded_resolution = self
            .stats
            .encoded_resolution
            .map(|(width, height)| format!("{width}x{height}"))
            .unwrap_or_else(|| "?".to_owned());
        let memory = if self.direct_output.decoder_uses_phycont.load(std::sync::atomic::Ordering::Relaxed) {
            "PHYCONT-NC"
        } else {
            "CDRAM"
        };
        Some(format!(
            "mem:{memory} out:NV12 net:{bitrate_kbps}kbps enc:{encoded_resolution} srcfps:{source_fps} {performance} wait:{} drop:{} nack:{} err:{}",
            u8::from(self.rtp.waiting_for_keyframe()),
            self.stats.dropped,
            self.stats.nacks_sent,
            self.stats.decode_errors,
        ))
    }
}

pub(crate) struct AudioReceiver {
    track_id: Option<MediaStreamTrackId>,
    rtp: rtp::AudioRtp,
    pub(crate) packets: Vec<Bytes>,
}

impl AudioReceiver {
    pub(crate) fn new(sample_rate: u32, payload_type: u8) -> Self {
        Self {
            track_id: None,
            rtp: rtp::AudioRtp::new(sample_rate, payload_type),
            packets: Vec::new(),
        }
    }

    pub(crate) fn open(&mut self, track_id: MediaStreamTrackId) {
        self.track_id = Some(track_id);
    }

    pub(crate) fn handles(&self, track_id: &MediaStreamTrackId) -> bool {
        self.track_id.as_ref() == Some(track_id)
    }

    pub(crate) fn receive(&mut self, packet: Packet) {
        self.rtp.receive(packet, &mut self.packets);
    }
}
