use anyhow::{Context, Result};
use lantern_platform::{CaptureOptions, EncodedFrame, NativeParts};
use lantern_protocol::{ControlEvent, DisplayInfo};
use rtc::{
    interceptor::{Interceptor, Packet, Slot, StreamInfo, TaggedPacket},
    media::Sample,
    media_stream::MediaStreamTrack,
    rtp_transceiver::rtp_sender::{
        RTCPFeedback, RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters,
        RTCRtpEncodingParameters, RtpCodecKind,
    },
    sansio::Protocol,
};
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use tokio::{
    sync::{mpsc, Notify},
    task::JoinHandle,
};
use webrtc::{
    data_channel::{DataChannel, DataChannelEvent},
    media_stream::track_local::{static_sample::TrackLocalStaticSample, TrackLocal},
    media_stream::Track,
    peer_connection::{
        register_default_interceptors, MediaEngine, PeerConnection, PeerConnectionBuilder,
        PeerConnectionEventHandler, RTCConfigurationBuilder, RTCIceGatheringState,
        RTCPeerConnectionState, RTCSessionDescription, Registry,
    },
};

#[derive(Clone)]
struct Handler {
    stop: Arc<AtomicBool>,
    gather: Arc<Notify>,
    connected: Arc<Notify>,
    input: mpsc::Sender<ControlEvent>,
    pointer: Arc<Mutex<Option<ControlEvent>>>,
}
#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            self.gather.notify_one();
        }
    }
    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        if state == RTCPeerConnectionState::Connected {
            self.connected.notify_one();
        }
        if matches!(
            state,
            RTCPeerConnectionState::Failed
                | RTCPeerConnectionState::Closed
                | RTCPeerConnectionState::Disconnected
        ) {
            self.stop.store(true, Ordering::Release);
            let _ = self.input.try_send(ControlEvent::ReleaseAll);
        }
    }
    async fn on_data_channel(&self, channel: Arc<dyn DataChannel>) {
        let this = self.clone();
        tokio::spawn(async move {
            let label = channel.label().await.unwrap_or_default();
            if label != "pointer" && label != "control" {
                return;
            }
            while let Some(event) = channel.poll().await {
                if this.stop.load(Ordering::Acquire) {
                    break;
                }
                match event {
                    DataChannelEvent::OnMessage(message) if message.data.len() <= 32_768 => {
                        let Ok(event) = serde_json::from_slice::<ControlEvent>(&message.data)
                        else {
                            continue;
                        };
                        if !event.validate()
                            || matches!(
                                &event,
                                ControlEvent::Button { seq: 0, .. }
                                    | ControlEvent::Button { x: None, .. }
                                    | ControlEvent::Button { y: None, .. }
                            )
                        {
                            continue;
                        }
                        if label == "pointer" {
                            if let ControlEvent::Pointer { seq, .. } = &event {
                                let mut slot = this.pointer.lock().unwrap();
                                if *seq > 0
                                    && !matches!(slot.as_ref(),Some(ControlEvent::Pointer{seq:old,..}) if old>=seq)
                                {
                                    *slot = Some(event);
                                }
                            }
                        } else if !matches!(event, ControlEvent::Pointer { .. })
                            && this.input.try_send(event).is_err()
                        {
                            // Never silently lose a key-up when back-pressured.
                            this.stop.store(true, Ordering::Release);
                            break;
                        }
                    }
                    DataChannelEvent::OnClose => {
                        this.stop.store(true, Ordering::Release);
                        break;
                    }
                    _ => {}
                }
            }
        });
    }
}

// Last interceptor in the inbound path: observe PLI/FIR after default RTCP processing.
struct KeyframeRequests {
    flag: Arc<AtomicBool>,
    read: VecDeque<TaggedPacket>,
    write: VecDeque<TaggedPacket>,
}
impl Protocol<TaggedPacket, TaggedPacket, ()> for KeyframeRequests {
    type Rout = TaggedPacket;
    type Wout = TaggedPacket;
    type Eout = ();
    type Error = rtc::shared::error::Error;
    type Time = Instant;
    fn handle_read(&mut self, message: TaggedPacket) -> std::result::Result<(), Self::Error> {
        if let Packet::Rtcp(packets) = &message.message.packet {
            if packets.iter().any(|p| p.as_any().is::<rtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication>() || p.as_any().is::<rtc::rtcp::payload_feedbacks::full_intra_request::FullIntraRequest>()) { self.flag.store(true, Ordering::Release); }
        }
        self.read.push_back(message);
        Ok(())
    }
    fn poll_read(&mut self) -> Option<Self::Rout> {
        self.read.pop_front()
    }
    fn handle_write(&mut self, message: TaggedPacket) -> std::result::Result<(), Self::Error> {
        self.write.push_back(message);
        Ok(())
    }
    fn poll_write(&mut self) -> Option<Self::Wout> {
        self.write.pop_front()
    }
}
impl Interceptor for KeyframeRequests {
    fn bind_local_stream(&mut self, _: &StreamInfo) {}
    fn unbind_local_stream(&mut self, _: &StreamInfo) {}
    fn bind_remote_stream(&mut self, _: &StreamInfo) {}
    fn unbind_remote_stream(&mut self, _: &StreamInfo) {}
}

pub struct Session {
    pub stop: Arc<AtomicBool>,
    peer: Option<Box<dyn PeerConnection>>,
    worker: JoinHandle<()>,
    stream: JoinHandle<()>,
}
impl Session {
    pub fn finished(&self) -> bool {
        self.stop.load(Ordering::Acquire) || self.worker.is_finished() || self.stream.is_finished()
    }
    pub async fn close(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(peer) = self.peer.take() {
            let _ = peer.close().await;
        }
        self.stream.abort();
        let _ = tokio::time::timeout(Duration::from_secs(3), &mut self.worker).await;
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.stream.abort();
        if let Some(peer) = self.peer.take() {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    let _ = peer.close().await;
                });
            }
        }
    }
}

struct StopOnDrop(Option<Arc<AtomicBool>>);
impl Drop for StopOnDrop {
    fn drop(&mut self) {
        if let Some(stop) = &self.0 {
            stop.store(true, Ordering::Release);
        }
    }
}

pub async fn answer(
    offer: String,
    options: CaptureOptions,
) -> Result<(Session, String, DisplayInfo)> {
    answer_with(
        offer,
        move || lantern_platform::open(options),
        vec!["0.0.0.0:0".into()],
    )
    .await
}

async fn answer_with<F>(
    offer: String,
    factory: F,
    binds: Vec<String>,
) -> Result<(Session, String, DisplayInfo)>
where
    F: FnOnce() -> Result<NativeParts> + Send + 'static,
{
    let stop = Arc::new(AtomicBool::new(false));
    let mut guard = StopOnDrop(Some(stop.clone()));
    let force = Arc::new(AtomicBool::new(true));
    let gather = Arc::new(Notify::new());
    let connected = Arc::new(Notify::new());
    let (input_tx, input_rx) = mpsc::channel(256);
    let pointer = Arc::new(Mutex::new(None));
    let (frame_tx, mut frame_rx) = mpsc::channel::<EncodedFrame>(1);
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let worker_stop = stop.clone();
    let worker_force = force.clone();
    let worker_pointer = pointer.clone();
    let worker = tokio::task::spawn_blocking(move || {
        // COM/WinRT initialization, use, and destruction all occur on this same MTA thread.
        let native = match factory() {
            Ok(native) => native,
            Err(error) => {
                let _ = ready_tx.send(Err(error));
                worker_stop.store(true, Ordering::Release);
                return;
            }
        };
        let _ = ready_tx.send(Ok(native.capture.display()));
        if let Err(error) = pump_native(
            native,
            input_rx,
            worker_pointer,
            frame_tx,
            worker_stop.clone(),
            worker_force,
        ) {
            tracing::error!(error = %error, "native pipeline stopped");
        }
        worker_stop.store(true, Ordering::Release);
    });
    let display = tokio::time::timeout(Duration::from_secs(20), ready_rx)
        .await
        .context("Native initialization timed out")??
        .context("Native initialization failed")?;
    let handler = Handler {
        stop: stop.clone(),
        gather: gather.clone(),
        connected: connected.clone(),
        input: input_tx,
        pointer,
    };
    let codec = RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: "video/H264".into(),
            clock_rate: 90_000,
            channels: 0,
            sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e028"
                .into(),
            rtcp_feedback: vec![
                RTCPFeedback {
                    typ: "nack".into(),
                    parameter: "pli".into(),
                },
                RTCPFeedback {
                    typ: "nack".into(),
                    parameter: String::new(),
                },
                RTCPFeedback {
                    typ: "ccm".into(),
                    parameter: "fir".into(),
                },
            ],
        },
        payload_type: 102,
    };
    let mut media = MediaEngine::default();
    media.register_codec(codec.clone(), RtpCodecKind::Video)?;
    let registry = register_default_interceptors(Registry::new(), &mut media)?.with(
        Slot::Custom(100_000),
        KeyframeRequests {
            flag: force.clone(),
            read: VecDeque::new(),
            write: VecDeque::new(),
        },
    );
    let peer = PeerConnectionBuilder::new()
        .with_configuration(RTCConfigurationBuilder::new().build())
        .with_media_engine(media)
        .with_interceptor_registry(registry)
        .with_handler(Arc::new(handler))
        .with_udp_addrs(binds)
        .build()
        .await?;
    let setup: Result<_> = async {
        let track = Arc::new(TrackLocalStaticSample::new(
            Instant::now(),
            MediaStreamTrack::new(
                "lantern-screen".into(),
                "lantern-desktop".into(),
                "Primary display".into(),
                RtpCodecKind::Video,
                vec![RTCRtpEncodingParameters {
                    rtp_coding_parameters: RTCRtpCodingParameters {
                        ssrc: Some(rand::random()),
                        ..Default::default()
                    },
                    codec: codec.rtp_codec,
                    ..Default::default()
                }],
            ),
        )?);
        let sender = peer.add_track(track.clone() as Arc<dyn TrackLocal>).await?;
        peer.set_remote_description(RTCSessionDescription::offer(offer)?)
            .await?;
        let answer = peer.create_answer(None).await?;
        peer.set_local_description(answer).await?;
        tokio::time::timeout(Duration::from_secs(15), gather.notified())
            .await
            .context("ICE gathering timed out")?;
        let sdp = peer
            .local_description()
            .await
            .context("Missing SDP answer")?
            .sdp;
        let pt = sender
            .get_parameters()
            .await?
            .rtp_parameters
            .codecs
            .first()
            .context("H.264 was not negotiated")?
            .payload_type;
        let ssrc = *track.ssrcs().await.first().context("Missing video SSRC")?;
        Ok((track, sdp, pt, ssrc))
    }
    .await;
    let (track, sdp, pt, ssrc) = match setup {
        Ok(parts) => parts,
        Err(error) => {
            let _ = peer.close().await;
            return Err(error);
        }
    };
    let stream_stop = stop.clone();
    let stream = tokio::spawn(async move {
        if tokio::time::timeout(Duration::from_secs(20), connected.notified())
            .await
            .is_err()
        {
            stream_stop.store(true, Ordering::Release);
            return;
        }
        while let Some(frame) = frame_rx.recv().await {
            if stream_stop.load(Ordering::Acquire) {
                break;
            }
            let sample = Sample {
                data: frame.data.into(),
                duration: frame.duration,
                ..Sample::new(Instant::now())
            };
            if tokio::time::timeout(
                Duration::from_millis(250),
                track.sample_writer(ssrc, pt).write_sample(&sample),
            )
            .await
            .map_or(true, |r| r.is_err())
            {
                stream_stop.store(true, Ordering::Release);
                break;
            }
        }
    });
    // Ownership of stop-on-drop transfers to Session after all fallible initialization.
    guard.0 = None;
    Ok((
        Session {
            stop,
            peer: Some(Box::new(peer)),
            worker,
            stream,
        },
        sdp,
        display,
    ))
}

#[derive(Default)]
struct InputOrder {
    latest: Option<(u32, f64, f64)>,
    button_barrier: u32,
}
impl InputOrder {
    fn inject(
        &mut self,
        input: &mut dyn lantern_platform::InputInjector,
        event: &ControlEvent,
    ) -> Result<()> {
        match event {
            ControlEvent::Pointer { x, y, seq } => {
                if *seq <= self.button_barrier || self.latest.is_some_and(|(s, _, _)| *seq <= s) {
                    return Ok(());
                }
                self.latest = Some((*seq, *x, *y));
                input.inject(event)
            }
            ControlEvent::Button {
                x: Some(x),
                y: Some(y),
                seq,
                ..
            } => {
                // Click coordinates are carried on the same reliable message as the transition.
                input.inject(&ControlEvent::Pointer {
                    x: *x,
                    y: *y,
                    seq: *seq,
                })?;
                input.inject(event)?;
                self.button_barrier = self.button_barrier.max(*seq);
                if let Some((s, x, y)) = self.latest {
                    if s > *seq {
                        input.inject(&ControlEvent::Pointer { x, y, seq: s })?;
                    }
                }
                Ok(())
            }
            _ => input.inject(event),
        }
    }
}

fn pump_native(
    mut native: NativeParts,
    mut input: mpsc::Receiver<ControlEvent>,
    pointer: Arc<Mutex<Option<ControlEvent>>>,
    frames: mpsc::Sender<EncodedFrame>,
    stop: Arc<AtomicBool>,
    force: Arc<AtomicBool>,
) -> Result<()> {
    let mut order = InputOrder::default();
    let mut waiting_idr = true;
    let result = (|| {
        tracing::info!(encoder = native.encoder.mode(), "native pipeline ready");
        while !stop.load(Ordering::Acquire) {
            // Process a bounded batch so a flood cannot starve capture or shutdown.
            for _ in 0..256 {
                match input.try_recv() {
                    Ok(event) => order.inject(native.input.as_mut(), &event)?,
                    Err(_) => break,
                }
            }
            if let Some(event) = pointer.lock().unwrap().take() {
                order.inject(native.input.as_mut(), &event)?;
            }
            if let Some(frame) = native.capture.next_frame(Duration::from_millis(35))? {
                let requested = force.swap(false, Ordering::AcqRel) || waiting_idr;
                for encoded in native.encoder.encode(frame, requested)? {
                    if waiting_idr && !encoded.keyframe {
                        continue;
                    }
                    let keyframe = encoded.keyframe;
                    if frames.try_send(encoded).is_err() {
                        waiting_idr = true;
                        force.store(true, Ordering::Release);
                    } else if keyframe {
                        waiting_idr = false;
                    }
                }
            }
        }
        Ok(())
    })();
    // Unconditionally run after capture, encoder and injection failures as well as normal exit.
    let released = native.input.release_all();
    result.and(released)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lantern_platform::{CaptureSource, InputInjector, RawFrame, VideoEncoder};
    use lantern_protocol::MouseButton;
    use rtc::rtp_transceiver::{RTCRtpTransceiverDirection, RTCRtpTransceiverInit};
    use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};

    #[derive(Clone)]
    struct Recorder {
        events: Arc<Mutex<Vec<ControlEvent>>>,
        released: Arc<AtomicBool>,
    }
    impl InputInjector for Recorder {
        fn inject(&mut self, event: &ControlEvent) -> Result<()> {
            self.events.lock().unwrap().push(event.clone());
            Ok(())
        }
        fn release_all(&mut self) -> Result<()> {
            self.released.store(true, Ordering::Release);
            Ok(())
        }
    }
    #[test]
    fn atomic_click_uses_its_position_and_rejects_late_pointer() {
        let mut order = InputOrder::default();
        let mut input = Recorder {
            events: Arc::default(),
            released: Arc::default(),
        };
        order
            .inject(
                &mut input,
                &ControlEvent::Button {
                    button: MouseButton::Left,
                    down: true,
                    x: Some(0.7),
                    y: Some(0.8),
                    seq: 2,
                },
            )
            .unwrap();
        order
            .inject(
                &mut input,
                &ControlEvent::Pointer {
                    x: 0.1,
                    y: 0.2,
                    seq: 1,
                },
            )
            .unwrap();
        let events = input.events.lock().unwrap();
        assert!(matches!(
            events[0],
            ControlEvent::Pointer { x: 0.7, y: 0.8, .. }
        ));
        assert!(matches!(events[1], ControlEvent::Button { down: true, .. }));
        assert_eq!(events.len(), 2);
    }
    #[test]
    fn reliable_button_is_preserved_if_later_motion_arrived_first() {
        let mut order = InputOrder::default();
        let mut input = Recorder {
            events: Arc::default(),
            released: Arc::default(),
        };
        order
            .inject(
                &mut input,
                &ControlEvent::Pointer {
                    x: 0.9,
                    y: 0.8,
                    seq: 3,
                },
            )
            .unwrap();
        order
            .inject(
                &mut input,
                &ControlEvent::Button {
                    button: MouseButton::Left,
                    down: false,
                    x: Some(0.5),
                    y: Some(0.6),
                    seq: 2,
                },
            )
            .unwrap();
        let events = input.events.lock().unwrap();
        assert!(matches!(
            events[1],
            ControlEvent::Pointer { x: 0.5, y: 0.6, .. }
        ));
        assert!(matches!(
            events[2],
            ControlEvent::Button { down: false, .. }
        ));
        assert!(matches!(
            events[3],
            ControlEvent::Pointer { x: 0.9, y: 0.8, .. }
        ));
    }
    struct FixtureCapture;
    impl CaptureSource for FixtureCapture {
        fn display(&self) -> DisplayInfo {
            DisplayInfo {
                id: "fixture".into(),
                width: 16,
                height: 16,
                scale: 1.0,
            }
        }
        fn next_frame(&mut self, _: Duration) -> Result<Option<RawFrame>> {
            std::thread::sleep(Duration::from_millis(33));
            Ok(Some(RawFrame {
                bgra: vec![],
                width: 16,
                height: 16,
                stride: 0,
                captured_at: Instant::now(),
            }))
        }
    }
    struct FixtureEncoder;
    impl VideoEncoder for FixtureEncoder {
        fn mode(&self) -> &'static str {
            "test-fixture"
        }
        fn encode(&mut self, _: RawFrame, _: bool) -> Result<Vec<EncodedFrame>> {
            Ok(vec![EncodedFrame {
                data: include_bytes!("../tests/fixtures/black.h264").to_vec(),
                duration: Duration::from_millis(33),
                keyframe: true,
            }])
        }
    }
    #[derive(Clone)]
    struct ReceiverHandler {
        gather: Arc<Notify>,
        packet: Arc<Notify>,
    }
    #[async_trait::async_trait]
    impl PeerConnectionEventHandler for ReceiverHandler {
        async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
            if state == RTCIceGatheringState::Complete {
                self.gather.notify_one();
            }
        }
        async fn on_track(&self, track: Arc<dyn TrackRemote>) {
            let received = self.packet.clone();
            tokio::spawn(async move {
                while let Some(event) = track.poll().await {
                    if let TrackRemoteEvent::OnRtpPacket(packet) = event {
                        if !packet.payload.is_empty() {
                            received.notify_one();
                            break;
                        }
                    }
                }
            });
        }
    }
    /// Real UDP ICE, DTLS, SRTP and SCTP. Fixture video and recording input are explicitly
    /// test-only; this does not certify system capture, browser decode or input injection.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_loopback_transports_h264_and_control_and_releases_on_close() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let gather = Arc::new(Notify::new());
            let packet = Arc::new(Notify::new());
            let mut media = MediaEngine::default();
            media.register_default_codecs().unwrap();
            let registry = register_default_interceptors(Registry::new(), &mut media).unwrap();
            let browser = PeerConnectionBuilder::new()
                .with_media_engine(media)
                .with_interceptor_registry(registry)
                .with_handler(Arc::new(ReceiverHandler {
                    gather: gather.clone(),
                    packet: packet.clone(),
                }))
                .with_udp_addrs(vec!["127.0.0.1:0"])
                .build()
                .await
                .unwrap();
            let control = browser.create_data_channel("control", None).await.unwrap();
            browser
                .add_transceiver_from_kind(
                    RtpCodecKind::Video,
                    Some(RTCRtpTransceiverInit {
                        direction: RTCRtpTransceiverDirection::Recvonly,
                        ..Default::default()
                    }),
                )
                .await
                .unwrap();
            let offer = browser.create_offer(None).await.unwrap();
            browser.set_local_description(offer).await.unwrap();
            gather.notified().await;
            let recorder = Recorder {
                events: Arc::default(),
                released: Arc::default(),
            };
            let observed = recorder.clone();
            let (session, answer, _) = answer_with(
                browser.local_description().await.unwrap().sdp,
                move || {
                    Ok(NativeParts {
                        capture: Box::new(FixtureCapture),
                        encoder: Box::new(FixtureEncoder),
                        input: Box::new(recorder),
                    })
                },
                vec!["127.0.0.1:0".into()],
            )
            .await
            .unwrap();
            browser
                .set_remote_description(RTCSessionDescription::answer(answer).unwrap())
                .await
                .unwrap();
            loop {
                if matches!(control.poll().await, Some(DataChannelEvent::OnOpen)) {
                    break;
                }
            }
            control
                .send_text(r#"{"type":"key","code":"ShiftLeft","down":true}"#)
                .await
                .unwrap();
            packet.notified().await;
            while observed.events.lock().unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            session.close().await;
            browser.close().await.unwrap();
            assert!(observed.released.load(Ordering::Acquire));
            assert!(matches!(
                observed.events.lock().unwrap()[0],
                ControlEvent::Key { down: true, .. }
            ));
        })
        .await
        .expect("real loopback transport must finish within 15s");
    }
}
