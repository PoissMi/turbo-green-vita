use crate::api::streaming::rtc::peer;
use crate::api_xbox::streaming::control::channel::{
    CHAT_CHANNEL, CONTROL_CHANNEL, INPUT_CHANNEL, MESSAGE_CHANNEL,
};
use crate::api_xbox::streaming::rtc::AUDIO_PAYLOAD_TYPE;
use crate::api_xbox::streaming::rtc::STUN_SERVER;
use crate::api_xbox::streaming::rtc::protocol::{ChannelIds, XboxRtcProtocol};
use anyhow::{Context, Result};
use rtc::peer_connection::RTCPeerConnection;
use rtc::peer_connection::configuration::media_engine::{
    MIME_TYPE_H264, MIME_TYPE_OPUS, MediaEngine,
};
use rtc::peer_connection::transport::RTCIceServer;
use rtc::rtp_transceiver::rtp_sender::{
    RTCPFeedback, RTCRtpCodec, RTCRtpCodecParameters, RtpCodecKind,
};

pub(super) fn create(
    max_bitrate_kbps: u32,
    video_width: u32,
    video_height: u32,
    video_fps: u32,
) -> Result<(RTCPeerConnection, XboxRtcProtocol)> {
    let mut media_engine = MediaEngine::default();
    register_vita_codecs(&mut media_engine).context("failed to register Vita codecs")?;

    let (peer_connection, ids) = peer::create(
        media_engine,
        vec![RTCIceServer {
            urls: vec![format!("stun:{STUN_SERVER}")],
            username: String::new(),
            credential: String::new(),
        }],
        &[
            CHAT_CHANNEL,
            CONTROL_CHANNEL,
            INPUT_CHANNEL,
            MESSAGE_CHANNEL,
        ],
    )?;

    // `ids[0]` (chat) is created for protocol compatibility with the remote peer, but its id
    // is never needed locally.
    let channel_ids = ChannelIds {
        control: ids[1],
        input: ids[2],
        message: ids[3],
    };

    Ok((
        peer_connection,
        XboxRtcProtocol::new(
            channel_ids,
            max_bitrate_kbps,
            video_width,
            video_height,
            video_fps,
        ),
    ))
}

/// The H.264 parameters we offer, which tell the server what it may send.
///
/// `max-fs` is the frame size in macroblocks: 3600 is 1280x720. `max-mbps`
/// is the frame rate in macroblocks per second, so 216000 is 60 fps.
///
/// The server does not take the frame rate from what we ask: the upstream
/// client asked for 30 fps and was sent 60 fps anyway, and this client
/// asks for 60 and is sent 60. So this is one line, not a choice - and
/// the only thing left to lower the server's load is the bitrate.
const VIDEO_FMTP_LINE: &str = "level-asymmetry-allowed=0;packetization-mode=1;profile-level-id=42e020;max-fs=3600;max-mbps=216000";

fn register_vita_codecs(media_engine: &mut MediaEngine) -> Result<()> {
    media_engine.register_codec(
        RTCRtpCodecParameters {
            rtp_codec: RTCRtpCodec {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                clock_rate: 48_000,
                channels: 2,
                sdp_fmtp_line: "minptime=10;useinbandfec=1;stereo=1".to_owned(),
                rtcp_feedback: vec![],
            },
            payload_type: AUDIO_PAYLOAD_TYPE,
        },
        RtpCodecKind::Audio,
    )?;

    media_engine.register_codec(
        RTCRtpCodecParameters {
            rtp_codec: RTCRtpCodec {
                mime_type: MIME_TYPE_H264.to_owned(),
                clock_rate: 90_000,
                channels: 0,
                sdp_fmtp_line: VIDEO_FMTP_LINE.to_owned(),
                rtcp_feedback: vec![
                    RTCPFeedback {
                        typ: "goog-remb".to_owned(),
                        parameter: "".to_owned(),
                    },
                    RTCPFeedback {
                        typ: "ccm".to_owned(),
                        parameter: "fir".to_owned(),
                    },
                    RTCPFeedback {
                        typ: "nack".to_owned(),
                        parameter: "".to_owned(),
                    },
                    RTCPFeedback {
                        typ: "nack".to_owned(),
                        parameter: "pli".to_owned(),
                    },
                ],
            },
            payload_type: 102,
        },
        RtpCodecKind::Video,
    )?;

    Ok(())
}
