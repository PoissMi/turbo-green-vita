use crate::Stream;
use crate::api::streaming::rtc::session::RtcSessionConfig;
use crate::api::streaming::rtc::worker::{RtcWorker, RtcWorkerProvider};
use crate::api_xbox::streaming::rtc::protocol::XboxRtcProtocol;
use crate::api_xbox::streaming::rtc::{AUDIO_PAYLOAD_TYPE, ROUTE_PROBE, STUN_SERVER, peer};
use crate::streaming::audio::AUDIO_SAMPLE_RATE;
use crate::streaming::video::{
    DecoderConfig, HW_OUTPUT_HEIGHT, HW_OUTPUT_WIDTH, STREAM_HEIGHT, STREAM_WIDTH,
};
use anyhow::Result;
use rtc::peer_connection::RTCPeerConnection;
use rtc::peer_connection::sdp::RTCSessionDescription;

struct XboxRtcWorkerProvider {
    stream: Stream,
    max_bitrate_kbps: u32,
}

impl RtcWorkerProvider for XboxRtcWorkerProvider {
    type Protocol = XboxRtcProtocol;

    fn create_peer(&self) -> Result<(RTCPeerConnection, Self::Protocol)> {
        peer::create(self.max_bitrate_kbps, STREAM_WIDTH, STREAM_HEIGHT)
    }

    fn session_config(&self) -> RtcSessionConfig {
        RtcSessionConfig {
            stun_server: STUN_SERVER,
            route_probe: ROUTE_PROBE,
            audio_sample_rate: AUDIO_SAMPLE_RATE as u32,
            audio_payload_type: AUDIO_PAYLOAD_TYPE,
            max_video_bitrate_kbps: self.max_bitrate_kbps,
            decoder: DecoderConfig {
                decode_width: STREAM_WIDTH,
                decode_height: STREAM_HEIGHT,
                output_width: HW_OUTPUT_WIDTH,
                output_height: HW_OUTPUT_HEIGHT,
            },
        }
    }

    async fn exchange_sdp(&self, offer: &RTCSessionDescription) -> Result<String> {
        let sdp = set_max_video_bitrate(&offer.sdp, self.max_bitrate_kbps);
        let answer = self.stream.send_sdp_offer(&sdp).await?;
        Ok(normalize_answer_ice_credentials(&answer))
    }
}

/// rtc-rs rejects answers whose media sections carry different ICE ufrag/pwd values. This client
/// runs a single bundled transport, so the per-section values are unified before parsing.
fn normalize_answer_ice_credentials(sdp: &str) -> String {
    let mut ufrag: Option<&str> = None;
    let mut pwd: Option<&str> = None;
    for line in sdp.lines() {
        if ufrag.is_none()
            && let Some(value) = line.strip_prefix("a=ice-ufrag:")
        {
            ufrag = Some(value);
        }
        if pwd.is_none()
            && let Some(value) = line.strip_prefix("a=ice-pwd:")
        {
            pwd = Some(value);
        }
    }
    let (Some(ufrag), Some(pwd)) = (ufrag, pwd) else {
        return sdp.to_owned();
    };

    let mut output = String::with_capacity(sdp.len());
    for line in sdp.split_inclusive('\n') {
        if line.starts_with("a=ice-ufrag:") {
            output.push_str("a=ice-ufrag:");
            output.push_str(ufrag);
        } else if line.starts_with("a=ice-pwd:") {
            output.push_str("a=ice-pwd:");
            output.push_str(pwd);
        } else {
            output.push_str(line);
            continue;
        }
        if line.ends_with("\r\n") {
            output.push_str("\r\n");
        } else if line.ends_with('\n') {
            output.push('\n');
        }
    }
    output
}

/// Sets `b=AS:<kbps>` in the offered video section; xCloud caps its encoder at this value. Only
/// the SDP sent to the server is modified - the local description must stay byte-identical to
/// the offer rtc-rs generated, or `set_local_description` rejects it.
fn set_max_video_bitrate(sdp: &str, kbps: u32) -> String {
    let mut lines: Vec<String> = sdp.split('\n').map(str::to_owned).collect();
    let Some(m_line) = lines.iter().position(|line| line.starts_with("m=video")) else {
        return sdp.to_owned();
    };
    let line_ending = if lines[m_line].ends_with('\r') {
        "\r"
    } else {
        ""
    };
    let mut index = m_line + 1;
    while index < lines.len() && (lines[index].starts_with("i=") || lines[index].starts_with("c=")) {
        index += 1;
    }

    let bandwidth = format!("b=AS:{kbps}{line_ending}");
    if index < lines.len() && lines[index].starts_with("b=") {
        lines[index] = bandwidth;
    } else {
        lines.insert(index, bandwidth);
    }
    lines.join("\n")
}

pub(crate) fn spawn(stream: Stream, max_bitrate_kbps: u32) -> Result<RtcWorker> {
    RtcWorker::spawn(XboxRtcWorkerProvider {
        stream,
        max_bitrate_kbps,
    })
}
