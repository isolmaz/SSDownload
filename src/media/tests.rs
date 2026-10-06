use super::*;
use serde_json::json;

#[test]
fn fragment_concurrency_leaves_a_tunnel_for_retries() {
    // The proxy governor rejects CONNECT attempts beyond the per-host bound with 503 once its
    // wait expires, and a fragment retry arrives while the slow fragment still holds its
    // tunnel; the job therefore never occupies the whole bound.
    assert_eq!(fragment_concurrency(None, 2), "1");
    assert_eq!(fragment_concurrency(Some(8), 2), "1");
    assert_eq!(fragment_concurrency(Some(4), 16), "4");
    assert_eq!(fragment_concurrency(Some(16), 16), "15");
    assert_eq!(fragment_concurrency(Some(3), 0), "1");
    assert_eq!(fragment_concurrency(None, 0), "1");
    assert_eq!(fragment_concurrency(Some(1), 8), "1");
}

#[test]
fn proxy_tunnel_failures_count_as_network_media_failures() {
    let observed = "[download] Got error: ('Unable to connect to proxy', \
            OSError('Tunnel connection failed: 503 Service Unavailable')). \
            Retrying (1/3)... Sleeping 1.00 seconds ...";
    assert!(network_media_failure(observed));
    assert!(!network_media_failure("Unsupported URL: example.test"));
}

#[test]
fn unknown_codecs_do_not_hide_direct_media() {
    let info = media_info_from_json(&json!({"formats": [
        {"format_id": "mp4", "ext": "mp4", "vcodec": null},
        {"format_id": "storyboard", "vcodec": "none", "acodec": "none"}
    ]}))
    .unwrap();
    assert_eq!(
        info.formats
            .iter()
            .map(|f| f.id.as_str())
            .collect::<Vec<_>>(),
        ["mp4"]
    );
}

#[test]
fn explicitly_silent_video_cannot_be_selected_as_audio() {
    let info = media_info_from_json(&json!({"formats": [
        {"format_id": "video", "vcodec": "h264", "acodec": "none"}
    ]}))
    .unwrap();
    let request = AddRequest {
        kind: DownloadKind::Audio,
        format_id: Some("video".into()),
        ..AddRequest::default()
    };
    assert!(format_selector(&request, &info).is_err());
}

#[test]
fn audio_only_formats_cannot_be_selected_as_video() {
    let info = media_info_from_json(&json!({"formats": [
        {"format_id": "audio", "vcodec": "none", "acodec": "aac"}
    ]}))
    .unwrap();
    let mut request = AddRequest {
        kind: DownloadKind::Video,
        ..AddRequest::default()
    };
    assert!(format_selector(&request, &info).is_err());
    request.format_id = Some("audio".into());
    assert!(format_selector(&request, &info).is_err());
}

#[test]
fn unknown_codecs_do_not_allow_drm_media() {
    let result = media_info_from_json(&json!({"formats": [
        {"format_id": "encrypted", "vcodec": null, "has_drm": true},
        {"format_id": "storyboard", "vcodec": "none", "acodec": "none"}
    ]}));
    assert!(result.is_err());
}

#[test]
fn direct_mp4_uses_a_generic_downloadable_selector() {
    let request = AddRequest {
        url: "https://example.test/clip.mp4?token=one".into(),
        kind: DownloadKind::Video,
        container: Some("mp4".into()),
        max_height: Some(720),
        ..AddRequest::default()
    };
    assert_eq!(
        format_selector(&request, &MediaInfo::default()).unwrap(),
        "best[ext=mp4]/best"
    );
}

#[test]
fn unicode_audio_format_id_is_selected_without_rewriting() {
    let id = "audio_1-Türkçe_Ses";
    let info = media_info_from_json(&json!({"formats": [
        {"format_id":"video","height":720,"vcodec":"h264","acodec":"none"},
        {"format_id":id,"vcodec":"none","acodec":"aac","language":"tr"}
    ]}))
    .unwrap();
    let request = AddRequest {
        kind: DownloadKind::Video,
        video_format_id: Some("video".into()),
        audio_tracks: vec![AudioSelection {
            id: id.into(),
            language: Some("tr".into()),
        }],
        ..Default::default()
    };

    assert_eq!(
        format_selector(&request, &info).unwrap(),
        format!("video+{id}")
    );
    assert!(validate_format_id("audio+other", "id").is_err());
    assert!(validate_format_id("audio other", "id").is_err());
}

#[test]
fn track_choices_are_exact_and_missing_choices_never_fall_back() {
    let info = media_info_from_json(&json!({"formats":[
            {"format_id":"v360","height":360,"vcodec":"h264","acodec":"none"},
            {"format_id":"v720","height":720,"vcodec":"h264","acodec":"none"},
            {"format_id":"tr-a","vcodec":"none","acodec":"aac","language":"tr"},
            {"format_id":"en-a","vcodec":"none","acodec":"aac","language":"en"}
        ],"subtitles":{"tr":[{"ext":"vtt"}]},"automatic_captions":{"en":[{"ext":"vtt"}]}}))
    .unwrap();
    let mut request = AddRequest {
        kind: DownloadKind::Video,
        exact_height: Some(720),
        audio_format_id: Some("tr-a".into()),
        audio_language: Some("tr".into()),
        subtitle_languages: vec!["tr".into()],
        subtitle_mode: Some("manual".into()),
        ..AddRequest::default()
    };
    assert_eq!(
        format_selector(&request, &info).unwrap(),
        "bestvideo[height=720]+tr-a"
    );
    request.exact_height = Some(1080);
    assert!(format_selector(&request, &info).is_err());
    request.exact_height = Some(720);
    request.audio_language = Some("en".into());
    assert!(validate_track_selection(&request, &info).is_err());
    request.audio_language = Some("tr".into());
    request.subtitle_mode = Some("automatic".into());
    assert!(validate_track_selection(&request, &info).is_err());
    request.subtitle_mode = None;
    request.subtitle_languages = vec!["all".into()];
    assert!(validate_track_selection(&request, &info).is_ok());
    request.subtitle_mode = Some("manual".into());
    assert!(validate_track_selection(&request, &info).is_err());
    request.subtitle_languages = vec!["tr".into()];
    let mut probe = json!({"streams":[{"codec_type":"video","codec_name":"h264","height":720},{"codec_type":"audio","codec_name":"aac","tags":{"language":"tur"}},{"codec_type":"subtitle","codec_name":"subrip","tags":{"language":"tur"}}],"format":{"duration":"2"}});
    assert!(validate_probe(&probe, &request, &info).is_ok());
    probe["streams"][1]["tags"]["language"] = json!("eng");
    assert!(validate_probe(&probe, &request, &info).is_err());
    probe["streams"][1]["tags"]["language"] = json!("tur");
    probe["streams"][0]["height"] = json!(360);
    assert!(validate_probe(&probe, &request, &info).is_err());
}

#[test]
fn single_video_request_rejects_a_page_of_trailers_and_promotions() {
    let info = MediaInfo {
        playlist_count: Some(6),
        ..MediaInfo::default()
    };
    assert!(validate_single_selection(&info, false).is_err());
    assert!(validate_single_selection(&info, true).is_ok());
}

#[test]
fn direct_media_audio_uses_best_before_ffmpeg_extraction() {
    let request = AddRequest {
        url: "https://example.test/clip.mkv".into(),
        kind: DownloadKind::Audio,
        audio_format: Some("m4a".into()),
        ..AddRequest::default()
    };
    assert_eq!(
        format_selector(&request, &MediaInfo::default()).unwrap(),
        "best"
    );
}

#[test]
fn direct_media_detection_ignores_query_and_is_case_insensitive() {
    assert!(direct_media_url(
        "https://example.test/CLIP.MP4?quality=720"
    ));
    assert!(!direct_media_url("https://example.test/live.m3u8"));
    assert!(!direct_media_url("https://example.test/live.mpd"));
    assert!(!direct_media_url("https://example.test/watch?id=clip.mp4"));
}

#[test]
fn streaming_manifests_keep_the_requested_resolution_and_codec_policy() {
    let info = media_info_from_json(&json!({"formats":[
        {"format_id":"v360","height":360,"vcodec":"h264","acodec":"none"},
        {"format_id":"v720-vp9","height":720,"vcodec":"vp9","acodec":"none"},
        {"format_id":"audio-aac","vcodec":"none","acodec":"aac"}
    ]}))
    .unwrap();
    for extension in ["m3u8", "mpd"] {
        let request = AddRequest {
            url: format!("https://example.test/master.{extension}"),
            kind: DownloadKind::Video,
            container: Some("mp4".into()),
            max_height: Some(360),
            ..AddRequest::default()
        };
        assert_eq!(format_selector(&request, &info).unwrap(), "v360+audio-aac");
    }
}

#[test]
fn exact_mp4_height_does_not_select_an_incompatible_higher_ranked_codec() {
    let info = media_info_from_json(&json!({"formats":[
        {"format_id":"h264-360","height":360,"vcodec":"h264","acodec":"none"},
        {"format_id":"vp9-720","height":720,"vcodec":"vp9","acodec":"none"},
        {"format_id":"audio-aac","vcodec":"none","acodec":"aac"}
    ]}))
    .unwrap();
    let incompatible = AddRequest {
        url: "https://example.test/master.mpd".into(),
        kind: DownloadKind::Video,
        container: Some("mp4".into()),
        exact_height: Some(720),
        ..Default::default()
    };
    assert!(format_selector(&incompatible, &info).is_err());
    let compatible = AddRequest {
        exact_height: Some(360),
        ..incompatible
    };
    assert_eq!(
        format_selector(&compatible, &info).unwrap(),
        "h264-360+audio-aac"
    );
}

#[test]
fn unknown_hls_audio_codec_is_preserved_and_verified_before_publication() {
    let info = media_info_from_json(&json!({"formats":[
        {"format_id":"video","height":360,"vcodec":"mp4v.20.9","acodec":"none"},
        {"format_id":"audio-Original","vcodec":"none","ext":"mp4"}
    ]}))
    .unwrap();
    let request = AddRequest {
        url: "https://example.test/master.m3u8".into(),
        kind: DownloadKind::Video,
        container: Some("mp4".into()),
        expected_audio: Some(true),
        ..Default::default()
    };
    assert_eq!(
        format_selector(&request, &info).unwrap(),
        "video+audio-Original"
    );
    validate_container_selection(&request, &info).unwrap();
    let mut probe = json!({"streams":[
            {"codec_type":"video","codec_name":"mpeg4","height":360},
            {"codec_type":"audio","codec_name":"aac"}
        ],"format":{"duration":"2.0"}});
    validate_probe(&probe, &request, &info).unwrap();
    probe["streams"][1]["codec_name"] = json!("opus");
    assert!(validate_probe(&probe, &request, &info).is_err());
    probe["streams"].as_array_mut().unwrap().pop();
    assert!(validate_probe(&probe, &request, &info).is_err());
}

#[test]
fn explicit_audio_tracks_allow_provisional_video_audio_metadata() {
    let info = media_info_from_json(&json!({"formats":[
        {"format_id":"video","height":180,"vcodec":"unknown"},
        {"format_id":"audio-tr","vcodec":"none","language":"tr"}
    ]}))
    .unwrap();
    let request = AddRequest {
        kind: DownloadKind::Video,
        video_format_id: Some("video".into()),
        audio_tracks: vec![AudioSelection {
            id: "audio-tr".into(),
            language: Some("tr".into()),
        }],
        expected_audio: Some(true),
        ..Default::default()
    };

    assert_eq!(format_selector(&request, &info).unwrap(), "video+audio-tr");
}

#[test]
fn web_video_mp4_policy_never_falls_back_to_silent_transcoding() {
    let request = AddRequest {
        url: "https://example.test/watch/123".into(),
        kind: DownloadKind::Video,
        container: Some("mp4".into()),
        max_height: Some(720),
        ..AddRequest::default()
    };
    let incompatible = media_info_from_json(&json!({"formats":[
        {"format_id":"vp9","height":720,"vcodec":"vp9","acodec":"none"},
        {"format_id":"opus","vcodec":"none","acodec":"opus"}
    ]}))
    .unwrap();
    assert!(format_selector(&request, &incompatible).is_err());
}

#[test]
fn yt_dlp_policy_disables_user_configuration_and_plugins() {
    let arguments = common_args(
        Path::new(r"C:\tools\deno.exe"),
        Path::new(r"C:\tools\ffmpeg.exe"),
        false,
    );
    assert!(arguments
        .windows(2)
        .any(|pair| pair == ["--ignore-config", "--no-plugin-dirs"]));
    assert!(arguments.contains(&"--no-exec".into()));
    assert!(arguments.contains(&"--no-playlist".into()));
    assert!(arguments.contains(&"--config-locations".into()));
    assert!(arguments.contains(&"-".into()));
}

#[test]
fn media_publication_moves_only_the_final_file_out_of_private_workspace() {
    let root = std::env::temp_dir().join(format!("ssdownload-media-test-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    let output = root.join("chosen-name");
    let request = AddRequest {
        url: "https://example.test/clip.mp4".into(),
        kind: DownloadKind::Video,
        container: Some("mp4".into()),
        ..AddRequest::default()
    };
    let plan = OutputPlan::new(&output, &request, None).unwrap();
    let staged = plan.workspace.join("media.mp4");
    fs::write(&staged, b"test media").unwrap();

    let published = plan
        .publish(&output, false, &HashSet::new(), Some(&staged))
        .unwrap();

    assert_eq!(published, root.join("chosen-name.mp4"));
    assert_eq!(fs::read(&published).unwrap(), b"test media");
    assert!(!plan.workspace.exists());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn incomplete_playlist_cannot_be_published() {
    let root =
        std::env::temp_dir().join(format!("ssdownload-playlist-test-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    let output = root.join("playlist");
    let request = AddRequest {
        url: "https://example.test/playlist".into(),
        kind: DownloadKind::Video,
        playlist: true,
        ..AddRequest::default()
    };
    let plan = OutputPlan::new(&output, &request, None).unwrap();

    assert!(plan.publish(&output, true, &HashSet::new(), None).is_err());
    assert!(!output.with_extension("").exists());
    assert!(plan.workspace.exists());
    remove_workspace(&plan.workspace, &plan.owner).unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn cancellation_terminates_the_owned_media_process_tree() {
    let args = vec!["/C".into(), "ping -n 10 127.0.0.1 > nul".into()];
    let mut running =
        RunningDownload::spawn_with_proxy(Path::new("cmd.exe"), &args, b"", None).unwrap();
    let control = TransferControl::default();
    let mut completed = HashSet::new();
    let mut last = None;
    std::thread::sleep(Duration::from_millis(100));
    control.cancel();

    let result = monitor_download(
        &mut running,
        &control,
        0,
        &mut |_| {},
        &mut completed,
        &mut last,
    )
    .unwrap();
    assert!(matches!(result, MonitorOutcome::Cancelled));
}
