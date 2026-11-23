use teams_av::{engine::Engine, sdp};

#[tokio::test]
async fn meeting_starts_with_bundled_audio_and_camera_without_opening_a_device() {
    let engine = Engine::prepare()
        .await
        .expect("prepare media without devices");
    let offer = sdp::parse(engine.signaled_offer()).expect("Teams offer");
    let audio = offer.media.iter().filter(|m| m.kind == "audio").count();
    let video = offer.media.iter().filter(|m| m.kind == "video").count();
    assert_eq!(audio, 1);
    assert!(video >= 1);
    assert!(
        offer
            .session
            .attributes
            .iter()
            .any(|a| a.name == "group"
                && a.value.as_deref().is_some_and(|v| v.starts_with("BUNDLE ")))
    );
    assert!(
        offer
            .media
            .iter()
            .any(|m| m.kind == "video" && m.value("label") == Some("main-video"))
    );
    let data = offer.media.last().expect("data line");
    assert_eq!(data.kind, "x-data");
    assert_eq!(data.mid(), Some("12"));
    let camera = offer
        .media
        .iter()
        .find(|line| line.value("label") == Some("main-video"))
        .unwrap();
    let range = camera.value("x-ssrc-range").unwrap();
    let (first, last) = range.split_once('-').unwrap();
    assert_eq!(last.parse::<u32>().unwrap() - first.parse::<u32>().unwrap(), 99);
}

#[tokio::test]
async fn camera_can_be_stopped_and_restarted_without_opening_a_device() {
    let mut engine = Engine::prepare().await.unwrap();
    assert!(engine.start_camera(0, 240).is_err());
    let previous_source = engine.start_camera(320, 240).unwrap();
    assert!(engine.start_camera(320, 240).is_err());
    engine.stop_camera().unwrap();
    let next_source = engine.start_camera(320, 240).unwrap();
    engine.stop_camera().unwrap();
    drop((previous_source, next_source));
}

#[tokio::test]
async fn candidates_have_a_single_owner_from_the_start_of_gathering() {
    let mut engine = Engine::prepare().await.unwrap();
    let mut candidates = engine.take_candidates().unwrap();
    assert!(engine.take_candidates().is_none());
    let candidate = tokio::time::timeout(std::time::Duration::from_secs(5), candidates.recv())
        .await
        .unwrap();
    assert!(candidate.is_some());
    assert!(!engine.candidate_overflowed());
}
