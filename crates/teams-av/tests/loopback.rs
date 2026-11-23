use futures_util::StreamExt;
use libwebrtc::{
    audio_source::native::NativeAudioSource,
    audio_stream::native::NativeAudioStream,
    peer_connection_factory::native::PeerConnectionFactoryExt,
    prelude::*,
    rtp_transceiver::{RtpTransceiverDirection, RtpTransceiverInit},
    video_source::{VideoResolution, native::NativeVideoSource},
    video_stream::native::NativeVideoStream,
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::{
    sync::mpsc,
    time::{interval, timeout},
};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn synthetic_audio_and_h264_video_cross_the_same_local_peer_connection() {
    let factory = PeerConnectionFactory::default();
    let sender = factory
        .create_peer_connection(RtcConfiguration::default())
        .unwrap();
    let receiver = factory
        .create_peer_connection(RtcConfiguration::default())
        .unwrap();
    let (s_tx, mut s_rx) = mpsc::unbounded_channel::<IceCandidate>();
    let (r_tx, mut r_rx) = mpsc::unbounded_channel::<IceCandidate>();
    sender.on_ice_candidate(Some(Box::new(move |candidate| {
        let _ = s_tx.send(candidate);
    })));
    receiver.on_ice_candidate(Some(Box::new(move |candidate| {
        let _ = r_tx.send(candidate);
    })));
    let (track_tx, mut track_rx) = mpsc::unbounded_channel::<MediaStreamTrack>();
    receiver.on_track(Some(Box::new(move |event| {
        let _ = track_tx.send(event.track);
    })));
    let audio_source = NativeAudioSource::new(AudioSourceOptions::default(), 48_000, 1, 100);
    let audio_track = factory.create_audio_track("synthetic-audio", audio_source.clone());
    sender
        .add_transceiver(
            MediaStreamTrack::Audio(audio_track),
            RtpTransceiverInit {
                direction: RtpTransceiverDirection::SendRecv,
                stream_ids: vec!["microphone".into()],
                send_encodings: vec![],
            },
        )
        .unwrap();
    let video_source = NativeVideoSource::new(
        VideoResolution {
            width: 320,
            height: 240,
        },
        false,
    );
    let video_track = factory.create_video_track("synthetic-camera", video_source.clone());
    let video = sender
        .add_transceiver(
            MediaStreamTrack::Video(video_track),
            RtpTransceiverInit {
                direction: RtpTransceiverDirection::SendRecv,
                stream_ids: vec!["camera".into()],
                send_encodings: vec![],
            },
        )
        .unwrap();
    let codecs = factory
        .get_rtp_sender_capabilities(MediaType::Video)
        .codecs
        .into_iter()
        .filter(|codec| {
            matches!(
                codec.mime_type.to_ascii_lowercase().as_str(),
                "video/h264" | "video/rtx"
            )
        })
        .collect::<Vec<_>>();
    assert!(
        codecs
            .iter()
            .any(|codec| codec.mime_type.eq_ignore_ascii_case("video/h264"))
    );
    video.set_codec_preferences(codecs).unwrap();
    let offer = sender.create_offer(OfferOptions::default()).await.unwrap();
    let sdp = offer.to_string();
    assert!(sdp.contains("a=group:BUNDLE") && sdp.contains("m=audio") && sdp.contains("m=video"));
    sender.set_local_description(offer.clone()).await.unwrap();
    receiver.set_remote_description(offer).await.unwrap();
    let answer = receiver
        .create_answer(AnswerOptions::default())
        .await
        .unwrap();
    receiver
        .set_local_description(answer.clone())
        .await
        .unwrap();
    sender.set_remote_description(answer).await.unwrap();
    let send_peer = sender.clone();
    let recv_peer = receiver.clone();
    let relay = tokio::spawn(async move {
        loop {
            tokio::select! {
                Some(c) = s_rx.recv() => { let _ = recv_peer.add_ice_candidate(c).await; }
                Some(c) = r_rx.recv() => { let _ = send_peer.add_ice_candidate(c).await; }
                else => break,
            }
        }
    });
    let audio_task = tokio::spawn(async move {
        let mut timer = interval(Duration::from_millis(10));
        let mut phase = 0usize;
        loop {
            timer.tick().await;
            let data: Vec<i16> = (0..480)
                .map(|i| {
                    let t = (phase + i) as f32 / 48_000.0;
                    (8_000.0 * (t * 440.0 * std::f32::consts::TAU).sin()) as i16
                })
                .collect();
            phase += 480;
            let frame = AudioFrame {
                data: data.into(),
                sample_rate: 48_000,
                num_channels: 1,
                samples_per_channel: 480,
            };
            if audio_source.capture_frame(&frame).await.is_err() {
                break;
            }
        }
    });
    let video_task = tokio::spawn(async move {
        let mut timer = interval(Duration::from_millis(33));
        let mut tick = 0u8;
        loop {
            timer.tick().await;
            let mut buffer = I420Buffer::new(320, 240);
            let (y, u, v) = buffer.data_mut();
            y.fill(80 + tick % 80);
            u.fill(128);
            v.fill(128);
            let mut frame = VideoFrame::new(VideoRotation::VideoRotation0, buffer);
            frame.timestamp_us = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_micros() as i64;
            video_source.capture_frame(&frame);
            tick = tick.wrapping_add(1);
        }
    });
    let mut audio_stream = None;
    let mut video_stream = None;
    timeout(Duration::from_secs(15), async {
        while audio_stream.is_none() || video_stream.is_none() {
            match track_rx.recv().await.expect("remote track") {
                MediaStreamTrack::Audio(track) => {
                    audio_stream = Some(NativeAudioStream::new(track, 48_000, 1))
                }
                MediaStreamTrack::Video(track) => {
                    video_stream = Some(NativeVideoStream::new(track))
                }
            }
        }
    })
    .await
    .expect("remote tracks arrive");
    let mut audio_stream = audio_stream.unwrap();
    let mut video_stream = video_stream.unwrap();
    let audio_check = async {
        let mut nonzero = 0;
        for _ in 0..100 {
            let frame = audio_stream.next().await.expect("decoded audio");
            if frame.data.iter().any(|x| x.unsigned_abs() > 500) {
                nonzero += 1;
            }
            if nonzero >= 5 {
                break;
            }
        }
        assert!(nonzero >= 5, "received audible synthetic signal");
    };
    let video_check = async {
        let mut pictures = 0;
        while pictures < 5 {
            let frame = video_stream.next().await.expect("decoded H264 picture");
            assert_eq!((frame.buffer.width(), frame.buffer.height()), (320, 240));
            pictures += 1;
        }
    };
    timeout(Duration::from_secs(20), async {
        tokio::join!(audio_check, video_check);
    })
    .await
    .expect("audio and video frames arrive");
    audio_task.abort();
    video_task.abort();
    relay.abort();
    audio_stream.close();
    video_stream.close();
    sender.close();
    receiver.close();
}
