use super::ice;
use crate::logging::{DEFAULT_INTERVAL, Throttle, throttled};
use anyhow::{Context, Result};
use bytes::BytesMut;
use rtc::peer_connection::RTCPeerConnection;
use rtc::sansio::Protocol;
use rtc::shared::{TaggedBytesMut, TransportContext, TransportProtocol};
use std::net::SocketAddr;
use std::time::Instant;
use tokio::net::UdpSocket;

/// A failed send is logged, but not once per datagram: a send that keeps
/// failing would otherwise print one line per packet, on the thread that
/// has to keep up with the stream.
static LOG_SEND: Throttle = Throttle::new(DEFAULT_INTERVAL);
/// The same for a packet that cannot be handled.
static LOG_HANDLE: Throttle = Throttle::new(DEFAULT_INTERVAL);
/// And for a read that fails.
static LOG_RECV: Throttle = Throttle::new(DEFAULT_INTERVAL);

/// UDP/ICE transport shared by WebRTC streaming providers.
pub(crate) struct RtcTransport {
    pub(crate) socket: UdpSocket,
    local_addr: SocketAddr,
    recv_buf: Vec<u8>,
    /// When the last data-channel flush went out. The input waits for the
    /// next flush, so the worst gap between flushes bounds the input's
    /// delay on the wire.
    last_flush_at: Option<Instant>,
}

impl RtcTransport {
    pub(crate) async fn bind(
        peer: &mut RTCPeerConnection,
        stun_server: &str,
        route_probe: &str,
    ) -> Result<Self> {
        let socket = UdpSocket::bind("0.0.0.0:0")
            .await
            .context("failed to bind UDP socket for WebRTC transport")?;
        let socket_addr = socket
            .local_addr()
            .context("failed to read local UDP socket address")?;
        let local_addr = ice::add_host_candidate(peer, socket_addr.port(), route_probe)
            .context("failed to add local host ICE candidate")?;

        match ice::discover_server_reflexive_candidate(&socket, local_addr, stun_server).await {
            Ok(Some(public_addr)) => {
                if let Err(error) =
                    ice::add_srflx_candidate(peer, public_addr, local_addr, stun_server)
                {
                    eprintln!("Failed to add server-reflexive ICE candidate: {error:#}");
                }
            }
            Ok(None) => eprintln!("STUN request produced no usable response"),
            Err(error) => eprintln!("STUN discovery failed: {error:#}"),
        }

        ice::local_candidate(peer, String::new(), None)
            .context("failed to signal end-of-candidates")?;

        Ok(Self {
            socket,
            local_addr,
            recv_buf: vec![0u8; 2048],
            last_flush_at: None,
        })
    }

    pub(crate) async fn flush(&mut self, peer: &mut RTCPeerConnection) {
        let mut sent = false;
        let mut outgoing_count = 0u64;
        while let Some(outgoing) = peer.poll_write() {
            sent = true;
            outgoing_count += 1;
            // The input report that was queued is riding along with this
            // packet: this is how long it waited for the wire.
            if let Some(queued_at) = crate::streaming::video::metrics::take_input_queued() {
                let waited_us = queued_at.elapsed().as_micros() as u64;
                crate::streaming::video::metrics::METRICS
                    .input_wire_sum_us
                    .fetch_add(waited_us, std::sync::atomic::Ordering::Relaxed);
                crate::streaming::video::metrics::METRICS
                    .input_wire_max_us
                    .fetch_max(waited_us, std::sync::atomic::Ordering::Relaxed);
                crate::streaming::video::metrics::METRICS
                    .input_wire_count
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            if let Err(error) = self
                .socket
                .send_to(&outgoing.message, outgoing.transport.peer_addr)
                .await
            {
                throttled(
                    &LOG_SEND,
                    format_args!(
                        "Failed to send WebRTC UDP packet to {}: {error}",
                        outgoing.transport.peer_addr
                    ),
                );
            }
        }
        if sent {
            let now = Instant::now();
            if let Some(previous) = self.last_flush_at {
                crate::streaming::video::metrics::METRICS
                    .flush_gap_max_us
                    .fetch_max(now.duration_since(previous).as_micros() as u64, std::sync::atomic::Ordering::Relaxed);
            }
            self.last_flush_at = Some(now);
            // Outgoing datagrams per flush (ALL UDP traffic: RTCP +
            // SCTP control + data). This is NOT an SCTP queue metric and
            // must not be read as one — no public pending_queue/cwnd
            // accessor exists in the rtc fork. Kept only as a cycle-size
            // diagnostic alongside rx-b/pump.
            crate::streaming::video::metrics::METRICS
                .sctp_out_sum
                .fetch_add(outgoing_count, std::sync::atomic::Ordering::Relaxed);
            crate::streaming::video::metrics::METRICS
                .sctp_out_max
                .fetch_max(outgoing_count, std::sync::atomic::Ordering::Relaxed);
            crate::streaming::video::metrics::METRICS
                .sctp_out_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Reads everything that is waiting on the socket and hands it to the
    /// peer.
    ///
    /// The two halves are timed apart. A receive cycle that takes tens of
    /// milliseconds can be the socket read itself (the Vita's network
    /// stack) or the work done on what was read (SRTP decryption and the
    /// RTP parse, in software), and those need different fixes: the first
    /// is the drain strategy, the second is the packet rate.
    ///
    /// Returns the microseconds spent in the socket read and the
    /// microseconds spent handling the packets.
    pub(crate) fn receive(&mut self, peer: &mut RTCPeerConnection) -> (u64, u64) {
        let mut packets = 0u64;
        let mut socket_us = 0u64;
        let mut handle_us = 0u64;
        loop {
            let read_started = Instant::now();
            let received = self.socket.try_recv_from(&mut self.recv_buf);
            socket_us += read_started.elapsed().as_micros() as u64;
            match received {
                Ok((n, peer_addr)) => {
                    packets += 1;
                    let handle_started = Instant::now();
                    if let Err(error) = peer.handle_read(TaggedBytesMut {
                        now: Instant::now(),
                        transport: TransportContext {
                            local_addr: self.local_addr,
                            peer_addr,
                            ecn: None,
                            transport_protocol: TransportProtocol::UDP,
                        },
                        message: BytesMut::from(&self.recv_buf[..n]),
                    }) {
                        throttled(
                            &LOG_HANDLE,
                            format_args!(
                                "Failed to handle WebRTC UDP packet from {peer_addr}: {error}"
                            ),
                        );
                    }
                    handle_us += handle_started.elapsed().as_micros() as u64;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => {
                    throttled(
                        &LOG_RECV,
                        format_args!("Failed to receive WebRTC UDP packet: {error}"),
                    );
                    break;
                }
            }
        }
        crate::streaming::video::metrics::record_receive_cycle(socket_us + handle_us, packets);
        // A batch of many packets means the server is sending faster than we
        // drain, so the queue in front of the client is growing.
        if packets > 0 {
            crate::streaming::video::metrics::METRICS
                .rx_batch_sum
                .fetch_add(packets, std::sync::atomic::Ordering::Relaxed);
            crate::streaming::video::metrics::METRICS
                .rx_batch_max
                .fetch_max(packets, std::sync::atomic::Ordering::Relaxed);
            crate::streaming::video::metrics::METRICS
                .rx_batch_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        (socket_us, handle_us)
    }
}
