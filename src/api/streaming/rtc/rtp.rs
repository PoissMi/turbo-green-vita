use crate::streaming::video::VideoDecodeWorker;
use bytes::Bytes;
use h264_reader::annexb::AnnexBReader;
use h264_reader::nal::sps::SeqParameterSet;
use h264_reader::nal::{Nal, RefNal, UnitType};
use h264_reader::push::NalInterest;
use rtc::rtp::Packet;
use rtc::rtp::codec::h264::H264Packet;
use rtc::rtp::codec::opus::OpusPacket;
use rtc::rtp::packetizer::Depacketizer;
use rtc_media::io::sample_builder::SampleBuilder;
use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

const MAX_PENDING_AUDIO_PACKETS: usize = 32;
const MAX_H264_ACCESS_UNIT_BYTES: usize = 2 * 1024 * 1024;
const VIDEO_RTP_CLOCK_RATE: u32 = 90_000;
const AUDIO_MAX_LATE_PACKETS: u16 = 32;
const LOW_FPS_DAMAGE_LIMIT: u8 = 8;
const HIGH_FPS_DAMAGE_LIMIT: u8 = 3;
/// Frames kept in the reassembly queue while the head frame waits for its
/// missing packets.
const MAX_REORDER_FRAMES: usize = 3;
/// How long an incomplete frame may wait for a retransmitted packet before it
/// is abandoned. A Wi-Fi round trip is 10-40 ms, so 120 ms is generous while
/// still bounding how stale the pipeline can become.
const REPAIR_GRACE: Duration = Duration::from_millis(120);

#[derive(Default)]
pub(super) struct VideoSampleStats {
    pub dropped: u32,
    pub source_frame_duration_us: Option<u64>,
    pub encoded_resolution: Option<(u32, u32)>,
    /// Missing sequence numbers to NACK back to the sender (transport
    /// feedback was negotiated but previously never sent).
    pub nack_sequence_numbers: Vec<u16>,
}

pub(super) struct AudioRtp {
    samples: SampleBuilder<OpusPacket>,
    payload_type: u8,
}

impl AudioRtp {
    pub(super) fn new(sample_rate: u32, payload_type: u8) -> Self {
        Self {
            // The jitter buffer used to hold 80 ms of audio, which is 80 ms
            // the sound lags the picture by before the device buffer is even
            // counted. The video path measures sub-millisecond packet
            // reordering, so 30 ms is still generous slack.
            samples: SampleBuilder::new(AUDIO_MAX_LATE_PACKETS, OpusPacket, sample_rate)
                .with_max_time_delay(Duration::from_millis(30)),
            payload_type,
        }
    }

    pub(super) fn receive(&mut self, packet: Packet, audio_packets: &mut Vec<Bytes>) {
        if packet.header.payload_type != self.payload_type {
            return;
        }

        self.samples.push(packet);
        while audio_packets.len() < MAX_PENDING_AUDIO_PACKETS {
            let Some(sample) = self.samples.pop() else {
                break;
            };
            audio_packets.push(sample.data);
        }
    }
}

pub(super) struct VideoRtp {
    depacketizer: H264Packet,
    /// Frames being assembled, oldest first.
    ///
    /// A second entry means the head frame is still missing packets. That is
    /// not necessarily damage: on a busy link the tail of a frame can arrive
    /// after the head of the next frame. The head frame is kept so the late
    /// (or retransmitted) packets can complete it *in order*; a frame that is
    /// dropped breaks the H.264 reference chain and forces a keyframe, which
    /// saturates the 5 Mbps link and stalls the whole stream.
    frames: VecDeque<PendingVideoFrame>,
    next_sequence: Option<u16>,
    last_frame_timestamp: Option<u32>,
    source_frame_duration_us: Option<u64>,
    damage_score: u8,
    /// Set when the current receive() call damaged a frame, so a successful
    /// frame in the same call does not mask the damage.
    damaged_this_call: bool,
    stream_too_large: bool,
    waiting_for_keyframe: bool,
    max_width: u32,
    max_height: u32,
    /// Anchor for the server-timeline measurement: the first frame's RTP
    /// timestamp and the instant its first packet arrived.
    timeline_origin: Option<(u32, Instant)>,
    /// One sample per second of (arrival, RTP timestamp) so the offset can be
    /// compared against a minute-old sample.
    timeline_samples: VecDeque<(Instant, u32)>,
}

struct PendingVideoFrame {
    timestamp: u32,
    first_packet_at: Instant,
    packets: Vec<Packet>,
}

enum FrameAssembly {
    Pending,
    Complete { data: Vec<u8>, marker_sequence: u16 },
    Invalid,
}

impl PendingVideoFrame {
    fn new(packet: Packet) -> Self {
        Self {
            timestamp: packet.header.timestamp,
            first_packet_at: Instant::now(),
            packets: vec![packet],
        }
    }

    fn insert_sorted(&mut self, packet: Packet) {
        // Packets are kept unsorted here; `assemble` sorts by forward
        // sequence distance from the marker (arrival-order independent).
        // Incremental ordered insertion is unsafe for out-of-order delivery
        // (a packet arriving before the head mis-orders the buffer), so the
        // sort stays in assemble — the optimization is NACK + queue work
        // elsewhere, not here.
        if !self
            .packets
            .iter()
            .any(|existing| existing.header.sequence_number == packet.header.sequence_number)
        {
            self.packets.push(packet);
        }
    }
    #[allow(dead_code)]
    fn insert(&mut self, packet: Packet) {
        if !self
            .packets
            .iter()
            .any(|existing| existing.header.sequence_number == packet.header.sequence_number)
        {
            self.packets.push(packet);
        }
    }

    fn marker_sequence(&self) -> Option<u16> {
        self.packets
            .iter()
            .find(|packet| packet.header.marker)
            .map(|packet| packet.header.sequence_number)
    }

    /// Sequence numbers never seen for this frame (gaps below the marker),
    /// limited to the transport NACK bitmap window (16 packets).
    ///
    /// `presumed_marker` covers the case where the marker packet itself was
    /// lost: the next frame's first sequence minus one is the best guess for
    /// where this frame ends.
    fn missing_sequences(&self, presumed_marker: Option<u16>) -> Vec<u16> {
        let Some(marker) = self.marker_sequence().or(presumed_marker) else {
            return Vec::new();
        };
        // Walk back from marker to first received packet's neighbourhood.
        // The smallest seen sequence bounds the window.
        let mut seqs = self.packets.iter().map(|p| p.header.sequence_number).collect::<Vec<_>>();
        seqs.sort_unstable();
        let start = seqs.first().copied().unwrap_or(marker);
        let mut missing = Vec::new();
        let mut seq = start;
        // At most 16 iterations: the NACK bitmap covers packet_id + 16 bits.
        // `seqs` is sorted; membership is a binary search, no HashSet alloc.
        for _ in 0..16 {
            if seqs.binary_search(&seq).is_err() {
                missing.push(seq);
            }
            if seq == marker {
                break;
            }
            seq = seq.wrapping_add(1);
            // Stop once we've passed the marker forward distance.
            if marker.wrapping_sub(seq) > 16 {
                break;
            }
        }
        missing
    }

    fn assemble(
        &self,
        depacketizer: &mut H264Packet,
        expected_sequence: Option<u16>,
    ) -> FrameAssembly {
        let Some(marker_sequence) = self.marker_sequence() else {
            return FrameAssembly::Pending;
        };
        // Sort by forward sequence distance from the marker: arrival-order
        // independent (packets arrive out of order). Previously this ran
        // on every receive (O(n^2) per frame); see insert_sorted.
        let mut packets = self.packets.iter().collect::<Vec<_>>();
        packets.sort_unstable_by_key(|packet| {
            std::cmp::Reverse(marker_sequence.wrapping_sub(packet.header.sequence_number))
        });
        let Some(first) = packets.first() else {
            return FrameAssembly::Pending;
        };
        if expected_sequence.is_some_and(|expected| first.header.sequence_number != expected)
            || !depacketizer.is_partition_head(&first.payload)
        {
            return FrameAssembly::Pending;
        }
        if packets.windows(2).any(|pair| {
            pair[1].header.sequence_number != pair[0].header.sequence_number.wrapping_add(1)
        }) {
            return FrameAssembly::Pending;
        }

        *depacketizer = H264Packet::default();
        let mut data = Vec::new();
        for packet in packets {
            let Ok(nalu) = depacketizer.depacketize(&packet.payload) else {
                *depacketizer = H264Packet::default();
                return FrameAssembly::Invalid;
            };
            data.extend_from_slice(&nalu);
            if data.len() > MAX_H264_ACCESS_UNIT_BYTES {
                *depacketizer = H264Packet::default();
                return FrameAssembly::Invalid;
            }
        }
        *depacketizer = H264Packet::default();
        FrameAssembly::Complete {
            data,
            marker_sequence,
        }
    }
}

impl VideoRtp {
    pub(super) fn new(max_width: u32, max_height: u32) -> Self {
        Self {
            depacketizer: H264Packet::default(),
            frames: VecDeque::new(),
            next_sequence: None,
            last_frame_timestamp: None,
            source_frame_duration_us: None,
            damage_score: 0,
            damaged_this_call: false,
            stream_too_large: false,
            waiting_for_keyframe: false,
            max_width,
            max_height,
            timeline_origin: None,
            timeline_samples: VecDeque::new(),
        }
    }

    pub(super) fn waiting_for_keyframe(&self) -> bool {
        self.waiting_for_keyframe
    }

    pub(super) fn wait_for_keyframe(&mut self) {
        self.waiting_for_keyframe = true;
    }

    pub(super) fn receive(
        &mut self,
        worker: &VideoDecodeWorker,
        packet: Packet,
        keyframe_requested: &mut bool,
    ) -> VideoSampleStats {
        let mut stats = VideoSampleStats::default();
        self.damaged_this_call = false;
        if packet.payload.is_empty() {
            if self.next_sequence == Some(packet.header.sequence_number) {
                self.next_sequence = Some(packet.header.sequence_number.wrapping_add(1));
            }
            return stats;
        }

        let packet_timestamp = packet.header.timestamp;
        let packet_sequence = packet.header.sequence_number;
        // Route the packet to the frame it belongs to. A late or retransmitted
        // packet carries its original timestamp, so it lands back in the frame
        // that is still waiting for it.
        if let Some(frame) = self
            .frames
            .iter_mut()
            .find(|frame| frame.timestamp == packet_timestamp)
        {
            frame.insert_sorted(packet);
        } else {
            let starts_new_frame = self
                .frames
                .back()
                .is_none_or(|back| timestamp_is_newer(packet_timestamp, back.timestamp));
            if !starts_new_frame {
                // Belongs to a frame that was already submitted or abandoned.
                return stats;
            }
            if self.frames.is_empty()
                && self
                    .last_frame_timestamp
                    .is_some_and(|last| !timestamp_is_newer(packet_timestamp, last))
            {
                return stats;
            }
            // A newer frame started while the head frame is still missing
            // packets. NACK the gaps right away so the retransmission can
            // complete the head frame in order.
            if let Some(head) = self.frames.front() {
                let presumed_marker = packet_sequence.wrapping_sub(1);
                let missing = head.missing_sequences(Some(presumed_marker));
                if !missing.is_empty() {
                    stats.nack_sequence_numbers.extend(missing);
                }
            }
            if self.frames.len() >= MAX_REORDER_FRAMES {
                self.abandon_head(keyframe_requested, &mut stats);
            }
            self.frames.push_back(PendingVideoFrame::new(packet));
        }

        self.drain_frames(worker, keyframe_requested, &mut stats);
        stats
    }

    /// Submits every complete frame and gives up on frames that waited too
    /// long for their missing packets.
    fn drain_frames(
        &mut self,
        worker: &VideoDecodeWorker,
        keyframe_requested: &mut bool,
        stats: &mut VideoSampleStats,
    ) {
        loop {
            self.submit_ready_frames(worker, keyframe_requested, stats);
            let expired = self
                .frames
                .front()
                .is_some_and(|head| head.first_packet_at.elapsed() > REPAIR_GRACE);
            if !expired {
                return;
            }
            self.abandon_head(keyframe_requested, stats);
        }
    }

    /// Submits frames that are complete, oldest first. A frame that is still
    /// missing packets holds back the frames behind it: an H.264 P-frame
    /// cannot be decoded before its reference frame.
    fn submit_ready_frames(
        &mut self,
        worker: &VideoDecodeWorker,
        keyframe_requested: &mut bool,
        stats: &mut VideoSampleStats,
    ) {
        while let Some(frame) = self.frames.pop_front() {
            let assembly = frame.assemble(&mut self.depacketizer, self.next_sequence);
            match assembly {
                FrameAssembly::Pending => {
                    self.frames.push_front(frame);
                    return;
                }
                FrameAssembly::Invalid => {
                    self.depacketizer = H264Packet::default();
                    self.next_sequence = None;
                    *keyframe_requested = true;
                    self.record_damage();
                    self.damaged_this_call = true;
                    stats.dropped = stats.dropped.saturating_add(1);
                }
                FrameAssembly::Complete {
                    data,
                    marker_sequence,
                } => {
                    self.submit_completed(
                        frame,
                        data,
                        marker_sequence,
                        worker,
                        keyframe_requested,
                        stats,
                    );
                }
            }
        }
    }

    /// Drops the oldest incomplete frame after its repair window expired.
    fn abandon_head(
        &mut self,
        keyframe_requested: &mut bool,
        stats: &mut VideoSampleStats,
    ) {
        let Some(frame) = self.frames.pop_front() else {
            return;
        };
        let missing = frame.missing_sequences(None);
        if !missing.is_empty() {
            stats.nack_sequence_numbers.extend(missing);
        }
        self.next_sequence = frame.marker_sequence().map(|seq| seq.wrapping_add(1));
        self.depacketizer = H264Packet::default();
        // Only escalate to a keyframe request once the damage score trips: a
        // keyframe is large and saturates the 5 Mbps link, so asking for one
        // per damaged frame is what turns a single lost packet into a stall.
        if self.record_damage() {
            *keyframe_requested = true;
        }
        self.damaged_this_call = true;
        stats.dropped = stats.dropped.saturating_add(1);
    }

    /// Measures how far the server's video timeline lags our wall clock.
    ///
    /// The frame age the HUD reports starts when a packet leaves the socket
    /// buffer, so a queue that grows in front of the client - the server's
    /// send path, or the network in between - is invisible there: every frame
    /// looks "fresh" by the time we see it while the picture itself falls
    /// further and further behind reality. The RTP timestamp is the server's
    /// capture time, so the gap between it and our arrival clock is exactly
    /// that hidden delay. Only its *change* over time matters; the absolute
    /// value carries the network delay.
    fn record_timeline(&mut self, rtp_timestamp: u32, arrived_at: Instant) {
        let Some((origin_ts, origin_wall)) = self.timeline_origin else {
            self.timeline_origin = Some((rtp_timestamp, arrived_at));
            return;
        };
        let offset_us =
            Self::timeline_offset_us(origin_ts, origin_wall, rtp_timestamp, arrived_at);
        crate::streaming::video::metrics::METRICS
            .server_offset_us
            .store(offset_us, Ordering::Relaxed);

        // One sample per second is enough to see a queue that grows over
        // minutes, and it keeps the comparison below free of packet jitter.
        let due = self
            .timeline_samples
            .back()
            .is_none_or(|(last_at, _)| arrived_at.duration_since(*last_at) >= Duration::from_secs(1));
        if !due {
            return;
        }
        self.timeline_samples.push_back((arrived_at, rtp_timestamp));
        while self.timeline_samples.len() > 130 {
            self.timeline_samples.pop_front();
        }
        // Compare against the sample from about a minute ago: a positive
        // drift means the video is arriving later and later.
        if let Some((at, ts)) = self
            .timeline_samples
            .iter()
            .find(|(at, _)| arrived_at.duration_since(*at) >= Duration::from_secs(58))
        {
            let previous = Self::timeline_offset_us(origin_ts, origin_wall, *ts, *at);
            crate::streaming::video::metrics::METRICS
                .server_drift_us
                .store(offset_us - previous, Ordering::Relaxed);
        }
    }

    fn timeline_offset_us(
        origin_ts: u32,
        origin_wall: Instant,
        rtp_timestamp: u32,
        arrived_at: Instant,
    ) -> i64 {
        let rtp_delta_us = rtp_timestamp.wrapping_sub(origin_ts) as i64 * 1_000_000
            / i64::from(VIDEO_RTP_CLOCK_RATE);
        let wall_delta_us = arrived_at.duration_since(origin_wall).as_micros() as i64;
        wall_delta_us - rtp_delta_us
    }

    #[allow(clippy::too_many_arguments)]
    fn submit_completed(
        &mut self,
        completed: PendingVideoFrame,
        data: Vec<u8>,
        marker_sequence: u16,
        worker: &VideoDecodeWorker,
        keyframe_requested: &mut bool,
        stats: &mut VideoSampleStats,
    ) {
        // Record both average and worst-case RTP assembly time for the stream HUD.
        let assembly_us = completed.first_packet_at.elapsed().as_micros() as u64;
        // How late this frame is compared to the server's own timeline.
        self.record_timeline(completed.timestamp, completed.first_packet_at);
        crate::streaming::video::metrics::METRICS
            .rtp_assembly_sum_us
            .fetch_add(assembly_us, Ordering::Relaxed);
        crate::streaming::video::metrics::METRICS
            .rtp_assembly_count
            .fetch_add(1, Ordering::Relaxed);
        crate::streaming::video::metrics::METRICS
            .rtp_assembly_max_us
            .fetch_max(assembly_us, Ordering::Relaxed);
        self.next_sequence = Some(marker_sequence.wrapping_add(1));
        stats.source_frame_duration_us = self.last_frame_timestamp.map(|previous| {
            u64::from(completed.timestamp.wrapping_sub(previous)) * 1_000_000
                / u64::from(VIDEO_RTP_CLOCK_RATE)
        });
        if let Some(duration) = stats.source_frame_duration_us {
            self.source_frame_duration_us = Some(
                self.source_frame_duration_us
                    .map(|average| (average * 7 + duration) / 8)
                    .unwrap_or(duration),
            );
        }
        self.last_frame_timestamp = Some(completed.timestamp);

        let unit = inspect_h264_access_unit(&data);
        stats.encoded_resolution = unit.resolution;
        let sample_too_large = unit
            .resolution
            .is_some_and(|(width, height)| width > self.max_width || height > self.max_height);
        if sample_too_large {
            eprintln!(
                "Dropping H264 access unit larger than decoder: {:?} > {}x{}",
                unit.resolution, self.max_width, self.max_height
            );
            self.stream_too_large = true;
            // Flush queued decoder work once, then wait for a compatible IDR instead of feeding
            // frames that the Vita hardware cannot decode.
            *keyframe_requested = true;
            if !self.waiting_for_keyframe {
                worker.begin_resync();
            }
            self.waiting_for_keyframe = true;
            stats.dropped = stats.dropped.saturating_add(1);
            return;
        }
        if self.stream_too_large {
            if unit.resolution.is_none() || !unit.has_idr {
                *keyframe_requested = true;
                self.waiting_for_keyframe = true;
                stats.dropped = stats.dropped.saturating_add(1);
                return;
            }
            self.stream_too_large = false;
        }
        if self.waiting_for_keyframe {
            // Later keyframes may contain only IDR; AVCDEC retains SPS/PPS across resyncs.
            if !unit.has_idr {
                *keyframe_requested = true;
                self.waiting_for_keyframe = true;
                stats.dropped = stats.dropped.saturating_add(1);
                return;
            }
            self.waiting_for_keyframe = false;
            self.damage_score = 0;
        } else if !self.damaged_this_call {
            self.damage_score = self.damage_score.saturating_sub(1);
        }

        if !worker.submit_access_unit(
            data,
            self.source_frame_duration_us,
            completed.first_packet_at,
        ) {
            eprintln!("Video decoder queue is full; continuing while requesting a keyframe");
            *keyframe_requested = true;
            stats.dropped = stats.dropped.saturating_add(1);
        }
    }

    /// Bumps the damage score. Returns true when it trips, i.e. when a
    /// keyframe request is warranted.
    ///
    /// The decoder is deliberately left running: it conceals the missing
    /// reference and keeps producing pictures, so the display keeps updating
    /// instead of freezing until a keyframe arrives.
    fn record_damage(&mut self) -> bool {
        if self.waiting_for_keyframe {
            return false;
        }

        self.damage_score = self.damage_score.saturating_add(1);
        let source_fps = self
            .source_frame_duration_us
            .filter(|duration| *duration > 0)
            .map(|duration| 1_000_000 / duration)
            .unwrap_or(30);
        let damage_limit = if source_fps <= 30 {
            LOW_FPS_DAMAGE_LIMIT
        } else if source_fps >= 60 {
            HIGH_FPS_DAMAGE_LIMIT
        } else {
            LOW_FPS_DAMAGE_LIMIT
                - (((source_fps - 30) * u64::from(LOW_FPS_DAMAGE_LIMIT - HIGH_FPS_DAMAGE_LIMIT)
                    + 29)
                    / 30) as u8
        };
        if self.damage_score < damage_limit {
            return false;
        }

        self.damage_score = 0;
        true
    }
}

fn timestamp_is_newer(candidate: u32, reference: u32) -> bool {
    let distance = candidate.wrapping_sub(reference);
    distance != 0 && distance < (1 << 31)
}

struct AccessUnitInfo {
    has_idr: bool,
    resolution: Option<(u32, u32)>,
}

fn inspect_h264_access_unit(data: &[u8]) -> AccessUnitInfo {
    let mut info = AccessUnitInfo {
        has_idr: false,
        resolution: None,
    };
    let mut reader = AnnexBReader::accumulate(|nal: RefNal<'_>| {
        let Ok(header) = nal.header() else {
            return NalInterest::Ignore;
        };
        match header.nal_unit_type() {
            UnitType::SliceLayerWithoutPartitioningIdr => {
                info.has_idr = true;
                NalInterest::Ignore
            }
            UnitType::SeqParameterSet => {
                if nal.is_complete() {
                    info.resolution = SeqParameterSet::from_bits(nal.rbsp_bits())
                        .and_then(|sps| sps.pixel_dimensions())
                        .ok();
                }
                NalInterest::Buffer
            }
            _ => NalInterest::Ignore,
        }
    });
    reader.push(data);
    reader.reset();
    info
}
