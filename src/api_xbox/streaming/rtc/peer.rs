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
    register_vita_codecs(&mut media_engine, video_fps).context("failed to register Vita codecs")?;

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
/// is the frame rate in macroblocks per second, so 108000 is 30 fps and
/// 216000 is 60. It is the one number that decides how hard the server's
/// encoder has to work, and a 720p60 encode is twice the work of a 720p30
/// one - when the server cannot finish in real time the finished frames
/// pile up in front of us and the picture arrives from an older and older
/// point in the game.
///
/// Upstream asked for 30 fps with the level that goes with it. This fork
/// doubled both when it moved to 60 fps, and that is the change the late
/// picture came with. The bitrate cap cannot fix it: the encoder's cost is
/// per frame, so a lower bitrate barely takes any of that work away.
fn video_fmtp_line(video_fps: u32) -> String {
    if video_fps <= 30 {
        "level-asymmetry-allowed=0;packetization-mode=1;profile-level-id=42e01f;max-fs=3600;max-mbps=108000".to_owned()
    } else {
        "level-asymmetry-allowed=0;packetization-mode=1;profile-level-id=42e020;max-fs=3600;max-mbps=216000".to_owned()
    }
}

fn register_vita_codecs(media_engine: &mut MediaEngine, video_fps: u32) -> Result<()> {    media_engine.register_codec(
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
                sdp_fmtp_line: video_fmtp_line(video_fps),
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
