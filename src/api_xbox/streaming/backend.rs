use crate::Stream;
use crate::api::streaming::PlaybackBackendEvent;
use crate::api::streaming::rtc::worker::{RtcWorker, RtcWorkerEvent};
use crate::api_xbox::streaming::rtc::worker;
use crate::jobs::{PollJob, poll_job};
use crate::streaming::input::{GamepadFrame, PointerEvent};
use crate::streaming::video::{DecodedFrame, DirectVideoOutput};
use anyhow::Result;
use bytes::Bytes;
use rtc::peer_connection::transport::RTCIceCandidateInit;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;

const STREAM_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

pub(crate) struct XboxStreamingBackend {
    stream: Stream,
    worker: RtcWorker,
    ice_next_poll_at: Instant,
    remote_ice_candidates: HashSet<String>,
    pending_local_ice_candidates: Vec<RTCIceCandidateInit>,
    ice_post_job: Option<JoinHandle<Result<()>>>,
    ice_poll_job: Option<JoinHandle<Result<Option<Vec<RTCIceCandidateInit>>>>>,
    keepalive_next_at: Instant,
    keepalive_job: Option<JoinHandle<Result<serde_json::Value>>>,
}

impl XboxStreamingBackend {
    pub(crate) fn start(stream: Stream, max_bitrate_kbps: u32) -> Result<Self> {
        let worker = worker::spawn(stream.clone(), max_bitrate_kbps)?;
        Ok(Self {
            stream,
            worker,
            ice_next_poll_at: Instant::now(),
            remote_ice_candidates: HashSet::new(),
            pending_local_ice_candidates: Vec::new(),
            ice_post_job: None,
            ice_poll_job: None,
            keepalive_next_at: Instant::now(),
            keepalive_job: None,
        })
    }

    pub(crate) fn try_recv_event(&mut self) -> Option<PlaybackBackendEvent> {
        loop {
            match self.worker.events_rx.try_recv().ok()? {
                RtcWorkerEvent::LocalCandidates(candidates) => {
                    self.pending_local_ice_candidates.extend(candidates);
                }
                RtcWorkerEvent::Status { status } => {
                    return Some(PlaybackBackendEvent::Status(status));
                }
                RtcWorkerEvent::VideoResolution(width, height) => {
                    return Some(PlaybackBackendEvent::VideoResolution(width, height));
                }
                RtcWorkerEvent::Closed => return Some(PlaybackBackendEvent::Closed),
                RtcWorkerEvent::Error(message) => {
                    return Some(PlaybackBackendEvent::Error(message));
                }
            }
        }
    }

    pub(crate) fn try_recv_audio_packets(&self) -> Option<Vec<Bytes>> {
        self.worker.audio_rx.try_recv().ok()
    }

    pub(crate) fn take_latest_frame(&self) -> Option<(u64, DecodedFrame)> {
        let mut queue = self.worker.latest_frames.lock().ok()?;
        // Take the newest frame in the queue. Older ones are superseded:
        // they will never be shown, because the render loop uploads at
        // most one frame per iteration, so handing an older one to the UI
        // slot only makes the displayed frame older by one pickup period.
        //
        // The earlier code took the *oldest* fresh frame and dropped the
        // rest, which is why the handoff-to-upload time averaged 13.6 ms:
        // a frame waited in the slot while a newer one was already
        // available. Dropping the superseded frames first removes that
        // wait, so the age measured at upload is the age of the newest
        // frame the client has, not the oldest one it happened to pick up.
        let freshest = queue
            .iter()
            .rposition(|(_, frame)| {
                frame.decoded_at.elapsed() <= crate::streaming::video::FRESH_FRAME_LIMIT
            });
        let freshest = freshest?;
        while queue.len() > freshest + 1 {
            queue.pop_front();
            crate::streaming::video::metrics::METRICS
                .ui_overwrites
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        // Drop the rest of the queue without taking it: the freshest
        // frame is the one we return, and the ones behind it are older.
        queue.drain(..freshest + 1).last()
    }

    pub(crate) fn direct_video_output(&self) -> Arc<DirectVideoOutput> {
        Arc::clone(&self.worker.direct_video_output)
    }

    pub(crate) fn frame_ready(&self) -> Arc<tokio::sync::Notify> {
        Arc::clone(&self.worker.frame_ready)
    }

    pub(crate) fn send_gamepad_frame(&self, frame: GamepadFrame) {
        self.worker.send_gamepad_frame(frame);
    }

    pub(crate) fn send_gamepad_pulse(&self, frame: GamepadFrame) {
        self.worker.send_gamepad_pulse(frame);
    }

    pub(crate) fn send_pointer_event(&self, event: PointerEvent) {
        self.worker.send_pointer_event(event);
    }

    pub(crate) async fn maintain(&mut self) -> Option<String> {
        self.post_local_ice().await;
        self.poll_remote_ice().await;
        self.keep_alive().await
    }

    pub(crate) fn description(&self) -> String {
        format!("xCloud session {}", self.stream.session_id)
    }

    pub(crate) async fn stop(self) -> Result<()> {
        let session_id = self.stream.session_id.clone();
        let response = self.stream.stop().await?;
        eprintln!("Stopped xCloud session {session_id}: {response}");
        Ok(())
    }

    async fn post_local_ice(&mut self) {
        if let Some(job) = self.ice_post_job.take() {
            match poll_job(job).await {
                PollJob::Pending(job) => {
                    self.ice_post_job = Some(job);
                    return;
                }
                PollJob::Done(Ok(())) => {}
                PollJob::Done(Err(error)) => {
                    eprintln!("Failed to post local ICE candidates: {error:#}");
                }
            }
        }

        if self.pending_local_ice_candidates.is_empty() {
            return;
        }

        let stream = self.stream.clone();
        let candidates = std::mem::take(&mut self.pending_local_ice_candidates);
        let count = candidates.len();
        self.ice_post_job = Some(tokio::spawn(async move {
            stream.post_ice_candidates(candidates).await?;
            eprintln!("Posted {count} local ICE candidate(s) to xCloud");
            Ok(())
        }));
    }

    async fn poll_remote_ice(&mut self) {
        if let Some(job) = self.ice_poll_job.take() {
            match poll_job(job).await {
                PollJob::Pending(job) => self.ice_poll_job = Some(job),
                PollJob::Done(result) => {
                    let response = result.unwrap_or_else(|error| {
                        eprintln!("Failed to poll remote ICE candidates: {error:#}");
                        None
                    });
                    for candidate in response.into_iter().flatten() {
                        let key = format!(
                            "{}|{}|{}",
                            candidate.candidate,
                            candidate.sdp_mid.as_deref().unwrap_or(""),
                            candidate.sdp_mline_index.unwrap_or(0)
                        );
                        if self.remote_ice_candidates.insert(key) {
                            self.worker.add_remote_candidate(candidate);
                        }
                    }
                }
            }
        }

        if Instant::now() < self.ice_next_poll_at || self.ice_poll_job.is_some() {
            return;
        }

        let stream = self.stream.clone();
        self.ice_next_poll_at = Instant::now() + Duration::from_secs(1);
        self.ice_poll_job = Some(tokio::spawn(
            async move { stream.poll_ice_candidates().await },
        ));
    }

    async fn keep_alive(&mut self) -> Option<String> {
        if let Some(job) = self.keepalive_job.take() {
            match poll_job(job).await {
                PollJob::Pending(job) => {
                    self.keepalive_job = Some(job);
                    return None;
                }
                PollJob::Done(Ok(response)) => {
                    if let Some(code) = response.get("code").and_then(serde_json::Value::as_str)
                        && matches!(code, "SessionNotActive" | "SessionNotFound")
                    {
                        return Some(code.to_owned());
                    }
                }
                PollJob::Done(Err(error)) => {
                    eprintln!("Failed to send xCloud keepalive: {error:#}");
                }
            }
        }

        if Instant::now() < self.keepalive_next_at {
            return None;
        }
        self.keepalive_next_at = Instant::now() + STREAM_KEEPALIVE_INTERVAL;

        let stream = self.stream.clone();
        self.keepalive_job = Some(tokio::spawn(async move { stream.send_keepalive().await }));
        None
    }
}
