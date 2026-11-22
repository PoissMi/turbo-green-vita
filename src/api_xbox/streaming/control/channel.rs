use rtc::data_channel::RTCDataChannelId;
use rtc::peer_connection::RTCPeerConnection;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::api::streaming::rtc::peer::RtcDataChannelConfig;
use crate::api_xbox::streaming::rtc::protocol::ChannelIds;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum HandshakeStage {
    WaitingForChannels,
    WaitingForHandshakeAck,
    Ready,
}

pub(in crate::api_xbox::streaming) fn handle_data_channel_message(
    pc: &mut RTCPeerConnection,
    ids: &ChannelIds,
    max_bitrate_kbps: u32,
    video_width: u32,
    video_height: u32,
    video_fps: u32,
    handshake_stage: &mut HandshakeStage,
    channel_id: RTCDataChannelId,
    data: &[u8],
) {
    if channel_id != ids.message || *handshake_stage != HandshakeStage::WaitingForHandshakeAck {
        return;
    }
    let Ok(payload) = serde_json::from_slice::<Value>(data) else {
        return;
    };
    if payload.get("type").and_then(Value::as_str) != Some("HandshakeAck") {
        return;
    }

    if let Some(mut control_channel) = pc.data_channel(ids.control) {
        let _ = control_channel.send_text(authorization_request().to_string());
        let _ = control_channel.send_text(gamepad_changed(0, true).to_string());
        // The frame rate is asked for on the control channel as well as in
        // the capabilities message: the server takes the rate from the
        // capabilities `supportsFps` field only sometimes, and the video
        // preference is the message that names the frame rate directly.
        let _ = control_channel.send_text(
            video_preference(video_width, video_height, video_fps).to_string(),
        );
    }
    if let Some(mut message_channel) = pc.data_channel(ids.message) {
        for message in startup_messages(max_bitrate_kbps, video_width, video_height, video_fps) {
            let _ = message_channel.send_text(message.to_string());
        }
    }
    *handshake_stage = HandshakeStage::Ready;
}

pub(in crate::api_xbox::streaming) fn parse_server_video_size(
    ids: &ChannelIds,
    channel_id: RTCDataChannelId,
    data: &[u8],
) -> Option<(u32, u32)> {
    use crate::api_xbox::streaming::control::input::ReportType;

    if channel_id != ids.input || data.len() < 10 {
        return None;
    }
    let report_type = u16::from_le_bytes([data[0], data[1]]);
    if report_type != ReportType::ServerMetadata as u16 {
        return None;
    }
    let height = u32::from_le_bytes(data[2..6].try_into().ok()?);
    let width = u32::from_le_bytes(data[6..10].try_into().ok()?);
    Some((width, height))
}

pub const CHAT_CHANNEL: RtcDataChannelConfig = RtcDataChannelConfig {
    label: "chat",
    protocol: "chatV1",
    ordered: true,
    max_packet_life_time: None,
    max_retransmits: None,
};
pub const CONTROL_CHANNEL: RtcDataChannelConfig = RtcDataChannelConfig {
    label: "control",
    protocol: "controlV1",
    ordered: true,
    max_packet_life_time: None,
    max_retransmits: None,
};
pub const INPUT_CHANNEL: RtcDataChannelConfig = RtcDataChannelConfig {
    label: "input",
    protocol: "1.0",
    // Reliable and ordered, the same shape the upstream client and the
    // reference implementation use. This was silently turned into an
    // unordered, no-retransmit PR-SCTP stream by the backend refactor: the
    // server then has to skip lost chunks with a forward-TSN, and if it
    // does not, every gap holds the input back longer than the one before.
    ordered: true,
    max_packet_life_time: None,
    max_retransmits: None,
};
pub const MESSAGE_CHANNEL: RtcDataChannelConfig = RtcDataChannelConfig {
    label: "message",
    protocol: "messageV1",
    ordered: true,
    max_packet_life_time: None,
    max_retransmits: None,
};

pub fn authorization_request() -> Value {
    json!({
        "message": "authorizationRequest",
        "accessKey": "4BDB3609-C1F1-4195-9B37-FEFF45DA8B8E",
    })
}

pub fn gamepad_changed(gamepad_index: u8, was_added: bool) -> Value {
    json!({
        "message": "gamepadChanged",
        "gamepadIndex": gamepad_index,
        "wasAdded": was_added,
    })
}

pub fn video_keyframe_requested(ifr_requested: bool) -> Value {
    json!({
        "message": "videoKeyframeRequested",
        "ifrRequested": ifr_requested,
    })
}

/// The video preference the control channel accepts.
///
/// The capabilities message carries `supportsFps`, and the rate is named
/// again here. Neither is a request the server acts on: a client that asks
/// for 30 fps is still sent 60. The frame rate is one fixed number, so
/// this exists to keep the messages complete rather than to ask for
/// anything.
pub fn video_preference(width: u32, height: u32, fps: u32) -> Value {
    json!({
        "type": "VideoPreference",
        "width": width,
        "height": height,
        "fps": fps,
        "videoCodec": "H264",
        "colorSpaceFlags": 0,
    })
}

pub fn message_handshake() -> Value {
    json!({
        "type": "Handshake",
        "version": "messageV1",
        "id": "be0bfc6d-1e83-4c8a-90ed-fa8601c5a179",
        "cv": "0",
    })
}

pub fn generate_message(path: &str, data: Value) -> Value {
    json!({
        "type": "Message",
        "content": data.to_string(),
        "id": Uuid::new_v4(),
        "target": path,
        "cv": "",
    })
}

pub fn startup_messages(
    max_bitrate_kbps: u32,
    width: u32,
    height: u32,
    video_fps: u32,
) -> Vec<Value> {
    vec![
        generate_message(
            "/streaming/systemUi/configuration",
            json!({
                "version": [0, 2, 0],
                "systemUis": [],
            }),
        ),
        generate_message(
            "/streaming/properties/clientappinstallidchanged",
            json!({ "clientAppInstallId": "c97d7ee0-73b2-4239-bf1d-9d805a338429" }),
        ),
        generate_message(
            "/streaming/characteristics/orientationchanged",
            json!({ "orientation": 0 }),
        ),
        generate_message(
            "/streaming/characteristics/touchinputenabledchanged",
            json!({ "touchInputEnabled": false }),
        ),
        generate_message(
            "/streaming/characteristics/clientdevicecapabilities",
            json!({
                "supportsCustomResolution": true,
                "supportsHevc": false,
                "supportsHdr": false,
                "supportsFps": video_fps,
                "maxWidth": width,
                "maxHeight": height,
                "maxBitrateKbps": max_bitrate_kbps,
                "video": {
                    "width": width,
                    "height": height,
                    "maxWidth": width,
                    "maxHeight": height,
                    "maxBitrateKbps": max_bitrate_kbps,
                },
            }),
        ),
        generate_message(
            "/streaming/characteristics/dimensionschanged",
            json!({
                "horizontal": width,
                "vertical": height,
                "preferredWidth": width,
                "preferredHeight": height,
                "safeAreaLeft": 0,
                "safeAreaTop": 0,
                "safeAreaRight": width,
                "safeAreaBottom": height,
                "supportsCustomResolution": true,
            }),
        ),
    ]
}
