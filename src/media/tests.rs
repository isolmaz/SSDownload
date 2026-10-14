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
fn dolby_codec_names_from_the_extractor_match_the_probed_output() {
    // yt-dlp reports RFC 6381 names from HLS/DASH manifests; FFprobe names the same streams
    // ac3, eac3 and hevc, and both spellings must pass the MP4 policy and the output check.
    assert!(mp4_audio_codec("ec-3") && mp4_audio_codec("ac-3") && mp4_audio_codec("eac3"));
    assert!(mp4_video_codec("dvh1.05.06") && mp4_video_codec("dvhe.08.07"));
    assert!(codec_equivalent("ec-3", "eac3"));
    assert!(codec_equivalent("ac-3", "ac3"));
    assert!(codec_equivalent("dvh1.05.06", "hevc"));
    assert!(!codec_equivalent("ec-3", "ac3"));
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
    assert!(arguments
        .windows(2)
        .any(|pair| pair == ["--encoding", "utf-8"]));
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
fn published_name_keeps_dots_that_belong_to_the_title() {
    let extension = std::ffi::OsStr::new("mp4");
    assert_eq!(
        with_media_extension(Path::new(r"C:\d\Python 3.12 guide"), extension),
        PathBuf::from(r"C:\d\Python 3.12 guide.mp4")
    );
    assert_eq!(
        with_media_extension(Path::new(r"C:\d\Mr. Robot S01E02"), extension),
        PathBuf::from(r"C:\d\Mr. Robot S01E02.mp4")
    );
    // A name that already carries a container extension is retargeted, not doubled.
    assert_eq!(
        with_media_extension(Path::new(r"C:\d\clip.WEBM"), extension),
        PathBuf::from(r"C:\d\clip.mp4")
    );
    assert_eq!(
        without_media_extension(Path::new(r"C:\d\Vol. 2 best of")),
        PathBuf::from(r"C:\d\Vol. 2 best of")
    );
    assert_eq!(
        without_media_extension(Path::new(r"C:\d\list.m3u8")),
        PathBuf::from(r"C:\d\list")
    );

    let root = std::env::temp_dir().join(format!("ssdownload-media-test-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    let output = root.join("Python 3.12 guide");
    let request = AddRequest {
        url: "https://example.test/watch".into(),
        kind: DownloadKind::Video,
        ..AddRequest::default()
    };
    let plan = OutputPlan::new(&output, &request, None).unwrap();
    let staged = plan.workspace.join("media.mkv");
    fs::write(&staged, b"test media").unwrap();
    let published = plan
        .publish(&output, false, &HashSet::new(), Some(&staged))
        .unwrap();
    assert_eq!(published, root.join("Python 3.12 guide.mkv"));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn discarding_a_failed_playlist_attempt_keeps_workspace_state_and_finished_items() {
    let root =
        std::env::temp_dir().join(format!("ssdownload-playlist-test-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    let output = root.join("list");
    let request = AddRequest {
        url: "https://example.test/playlist".into(),
        kind: DownloadKind::Video,
        playlist: true,
        ..AddRequest::default()
    };
    let plan = OutputPlan::new(&output, &request, None).unwrap();
    let finished = plan.workspace.join("001 - First.mp4");
    let partial = plan.workspace.join("002 - Second.mp4.part");
    fs::write(&finished, b"done").unwrap();
    fs::write(&partial, b"partial").unwrap();
    // The tool reports the extended-length form of the workspace path.
    let keep = HashSet::from([finished.canonicalize().unwrap()]);

    let (removed, bytes) = discard_attempt_outputs(&plan, &keep).unwrap();

    assert_eq!((removed, bytes), (1, 7));
    assert!(finished.exists());
    assert!(!partial.exists());
    verify_workspace(&plan.workspace, &plan.owner).unwrap();
    assert!(plan.workspace.join(".request").exists());
    remove_workspace(&plan.workspace, &plan.owner).unwrap();
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
fn a_capture_that_outlives_its_budget_reports_the_timeout_code() {
    let args = vec!["/C".into(), "ping -n 10 127.0.0.1 > nul".into()];
    let error = run_capture(
        Path::new("cmd.exe"),
        &args,
        b"",
        Duration::from_millis(300),
        &TransferControl::default(),
    )
    .err()
    .expect("the capture must time out");
    assert_eq!(
        crate::error_codes::code_of(&error),
        Some(crate::error_codes::MED_011)
    );
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

#[test]
fn site_refusals_map_to_one_final_line_without_the_debug_trace() {
    let stderr = "[debug] Python 3.10.11 (OpenSSL 1.1.1t  7 Feb 2023)\n\
        [debug] Command-line config: ['--fragment-retries', '20']\n\
        [youtube] Extracting URL: https://example.test/watch\n\
        ERROR: [youtube] abc: Video unavailable. This video is not available in your country";
    assert!(site_access_blocked(stderr));
    assert!(!site_access_blocked(
        "ERROR: [generic] Requested format is not available. Use --list-formats"
    ));
    assert!(!site_access_blocked(
        "WARNING: Sign in to see more\nERROR: HTTP Error 404"
    ));
    let visible = non_debug_lines(stderr);
    assert!(!visible.contains("OpenSSL") && !visible.contains("--fragment-retries"));
    assert_eq!(
        final_error_text(&visible),
        "ERROR: [youtube] abc: Video unavailable. This video is not available in your country"
    );
    // A carriage return inside the tool's line separates its error from the progress text.
    assert_eq!(
        final_error_text(&non_debug_lines("ERROR: \r[download] Got error: gone\n")),
        "ERROR: \n[download] Got error: gone"
    );
}

#[test]
fn fragmented_resume_follows_the_selected_format_protocol_not_the_page_address() {
    let inspection = json!({"id":"x","title":"t","webpage_url":"https://example.test/watch?v=1","formats":[
        {"format_id":"hls-720","protocol":"m3u8_native","vcodec":"avc1","acodec":"aac","height":720},
        {"format_id":"http-360","protocol":"https","vcodec":"avc1","acodec":"aac","height":360}
    ]});
    let page = AddRequest {
        url: "https://example.test/watch?v=1".into(),
        kind: DownloadKind::Video,
        ..AddRequest::default()
    };
    assert!(selection_is_fragmented(&inspection, &page, "hls-720"));
    assert!(!selection_is_fragmented(&inspection, &page, "http-360"));
    let manifest = AddRequest {
        url: "https://example.test/master.m3u8?t=1".into(),
        ..page
    };
    assert!(selection_is_fragmented(&inspection, &manifest, "http-360"));
}

#[test]
fn maybe_drm_formats_are_kept_and_true_drm_is_not() {
    let info = media_info_from_json(&json!({"formats": [
        {"format_id":"maybe","vcodec":"avc1","acodec":"aac","has_drm":"maybe"},
        {"format_id":"locked","vcodec":"avc1","acodec":"aac","has_drm":true}
    ]}))
    .unwrap();
    assert_eq!(
        info.formats
            .iter()
            .map(|f| f.id.as_str())
            .collect::<Vec<_>>(),
        ["maybe"]
    );
}

#[test]
fn playlist_items_separate_request_fingerprints_but_unset_keeps_the_stored_value() {
    let mut request = AddRequest {
        url: "https://example.test/list".into(),
        kind: DownloadKind::Video,
        playlist: true,
        ..AddRequest::default()
    };
    let all = request_fingerprint(&request).unwrap();
    assert!(!serde_json::to_string(&fingerprint_value(&request, true))
        .unwrap()
        .contains("playlist_items"));
    request.playlist_items = Some("1-3".into());
    let first = request_fingerprint(&request).unwrap();
    request.playlist_items = Some("4-6".into());
    let second = request_fingerprint(&request).unwrap();
    assert_ne!(all, first);
    assert_ne!(first, second);
}

#[test]
fn startup_sweep_removes_cookie_jars_and_extractor_document_leases_only() {
    let root = std::env::temp_dir().join(format!("ssdownload-sweep-test-{}", uuid::Uuid::new_v4()));
    let info = root.join(format!("{INFO_LEASE_PREFIX}one"));
    let pinned = root.join(format!("{PINNED_LEASE_PREFIX}two"));
    fs::create_dir_all(&info).unwrap();
    fs::create_dir_all(&pinned).unwrap();
    fs::write(info.join("source.info.json"), b"{}").unwrap();
    let jar = root.join("ssdownload-cookie-three.txt");
    fs::write(&jar, b"jar").unwrap();
    let other = root.join("unrelated.txt");
    fs::write(&other, b"keep").unwrap();

    sweep_stale_session_files(&root);

    assert!(!info.exists() && !pinned.exists() && !jar.exists());
    assert!(other.exists());
    fs::remove_dir_all(root).unwrap();
}
