use super::*;
use serde_json::json;

#[test]
fn fragment_concurrency_leaves_a_tunnel_for_retries() {
    // The proxy governor rejects CONNECT attempts beyond the per-host bound with 503 once its
    // wait expires, and a fragment retry arrives while the slow fragment still holds its
    // tunnel; the job therefore never occupies the whole bound.
    assert_eq!(fragment_concurrency(None, 2, 0), "1");
    assert_eq!(fragment_concurrency(Some(8), 2, 0), "1");
    assert_eq!(fragment_concurrency(Some(4), 16, 0), "4");
    assert_eq!(fragment_concurrency(Some(16), 16, 0), "15");
    assert_eq!(fragment_concurrency(Some(3), 0, 0), "1");
    assert_eq!(fragment_concurrency(None, 0, 0), "1");
    assert_eq!(fragment_concurrency(Some(1), 8, 0), "1");
}

#[test]
fn each_fragment_restart_halves_the_parallelism_down_to_one() {
    assert_eq!(fragment_concurrency(Some(16), 16, 1), "7");
    assert_eq!(fragment_concurrency(Some(16), 16, 2), "3");
    assert_eq!(fragment_concurrency(Some(16), 16, 3), "1");
    assert_eq!(fragment_concurrency(Some(16), 16, 40), "1");
    assert_eq!(fragment_concurrency(Some(1), 8, 1), "1");
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
fn a_single_stream_result_without_a_format_list_is_its_own_format() {
    // yt-dlp's generic extractor for an MP4 the server sends as `binary/octet-stream`.
    let info = media_info_from_json(&json!({
        "_type": "video", "extractor": "generic", "direct": true, "title": "clip",
        "format_id": "0", "url": "https://cdn.example/clip.mp4", "ext": "mp4",
        "vcodec": null, "acodec": null
    }))
    .unwrap();
    assert_eq!(
        info.formats
            .iter()
            .map(|f| (f.id.as_str(), f.extension.as_str()))
            .collect::<Vec<_>>(),
        [("0", "mp4")]
    );
    let request = AddRequest {
        url: "https://cdn.example/clip.mp4".into(),
        kind: DownloadKind::Video,
        container: Some("mp4".into()),
        ..AddRequest::default()
    };
    assert!(format_selector(&request, &info).is_ok());
}

#[test]
fn a_direct_link_named_hls_only_by_its_extension_is_not_a_format() {
    // yt-dlp's generic extractor for an ad server's `.m3u8` address that answers with JavaScript.
    let error = media_info_from_json(&json!({
        "_type": "video", "extractor": "generic", "direct": true, "title": "xbrjha6",
        "format_id": "0", "url": "https://ads.example/cdn/manifest/video/xbrjha6.m3u8",
        "ext": "m3u8", "protocol": "m3u8_native"
    }))
    .unwrap_err();
    assert_eq!(
        crate::error_codes::code_of(&error),
        Some(crate::error_codes::MED_001)
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
fn a_lost_fragment_discards_only_the_unfinished_stream() {
    let root =
        std::env::temp_dir().join(format!("ssdownload-stream-test-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    let request = AddRequest {
        url: "https://example.test/master.m3u8".into(),
        kind: DownloadKind::Video,
        ..AddRequest::default()
    };
    let plan = OutputPlan::new(&root.join("film.mp4"), &request, None).unwrap();
    // The video stream finished in this attempt; the audio stream lost a fragment mid-chain.
    let video = plan.workspace.join("media.f2530.mp4");
    fs::write(&video, b"complete video").unwrap();
    let unfinished = [
        "media.faudio.mp4.part",
        "media.faudio.mp4.part-Frag7",
        "media.faudio.mp4.ytdl",
        "media.temp.mp4",
    ]
    .map(|name| plan.workspace.join(name));
    for path in &unfinished {
        fs::write(path, b"x").unwrap();
    }

    let (removed, _) = discard_attempt_outputs(&plan, &HashSet::new()).unwrap();

    assert_eq!(removed, unfinished.len());
    assert!(video.exists());
    assert!(unfinished.iter().all(|path| !path.exists()));
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

/// PowerShell starts `ping` as a grandchild and records its process id in `pid_file`. The
/// owned job holds the whole tree, so ending the tool must end the grandchild as well. The path
/// reaches the script through the environment: PowerShell also ends a single-quoted string at a
/// typographic quote, which a user name in the temp path can hold.
fn spawn_tool_with_grandchild(pid_file: &Path) -> (RunningDownload, u32) {
    let variable = format!("SSDOWNLOAD_TEST_PID_FILE_{}", uuid::Uuid::new_v4().simple());
    let script = format!(
        "$p = Start-Process -FilePath ping.exe -ArgumentList '-n','20','127.0.0.1' -WindowStyle Hidden -PassThru; \
         Set-Content -LiteralPath $env:{variable} -Value $p.Id; $p.WaitForExit()"
    );
    let args = vec![
        "-NoProfile".into(),
        "-NonInteractive".into(),
        "-Command".into(),
        script,
    ];
    std::env::set_var(&variable, pid_file);
    let running =
        RunningDownload::spawn_with_proxy(Path::new("powershell.exe"), &args, b"", None).unwrap();
    std::env::remove_var(&variable);
    let deadline = Instant::now() + Duration::from_secs(30);
    let pid = loop {
        if let Some(pid) = fs::read_to_string(pid_file)
            .ok()
            .and_then(|text| text.trim().parse::<u32>().ok())
        {
            break pid;
        }
        assert!(
            Instant::now() < deadline,
            "the grandchild process never started"
        );
        thread::sleep(Duration::from_millis(50));
    };
    (running, pid)
}

fn process_is_running(pid: u32) -> bool {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, WAIT_TIMEOUT},
        System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE},
    };
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        return false;
    }
    let running = unsafe { WaitForSingleObject(handle, 0) } == WAIT_TIMEOUT;
    unsafe { CloseHandle(handle) };
    running
}

fn assert_tool_tree_ended(grandchild: u32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while process_is_running(grandchild) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !process_is_running(grandchild),
        "the grandchild process {grandchild} outlived its tool"
    );
}

#[test]
fn cancellation_terminates_the_owned_media_process_tree() {
    let root = std::env::temp_dir().join(format!("ssdownload-tree-test-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    let (mut running, grandchild) = spawn_tool_with_grandchild(&root.join("grandchild.pid"));
    assert!(process_is_running(grandchild));
    let control = TransferControl::default();
    control.cancel();

    let result = monitor_download(
        &mut running,
        &control,
        0,
        &mut |_| {},
        &mut HashSet::new(),
        &mut None,
        None,
    )
    .unwrap();

    assert!(matches!(result, MonitorOutcome::Cancelled));
    assert_tool_tree_ended(grandchild);
    drop(running);
    fs::remove_dir_all(&root).unwrap();
}

/// FFmpeg records a live stream into a `.part` file without a single progress line; the gauge
/// must read the size through the file while the writer still holds it open.
#[test]
fn live_gauge_measures_the_growing_part_file_while_it_is_open() {
    let root = std::env::temp_dir().join(format!("ssdownload-live-test-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("media.info.json"), vec![0u8; 500]).unwrap();
    let mut writer = File::create(root.join("media.mp4.part")).unwrap();
    writer.write_all(&vec![1u8; 4096]).unwrap();
    writer.flush().unwrap();

    let mut gauge = LiveRecordingGauge::new(&root);
    let start = Instant::now();
    let at = |millis| start + Duration::from_millis(millis);
    let first = gauge
        .sample(100, at(0))
        .expect("the first sample is immediate");
    assert_eq!(first.downloaded, 100 + 4096);
    assert_eq!(first.total, None);
    assert!(
        gauge.sample(100, at(500)).is_none(),
        "samples are at most once per second"
    );

    writer.write_all(&vec![1u8; 8192]).unwrap();
    writer.flush().unwrap();
    let second = gauge.sample(100, at(2000)).expect("a second has passed");
    assert_eq!(second.downloaded, 100 + 4096 + 8192);
    assert_eq!(
        second.speed, 4096,
        "8 KiB over the two seconds between the sizes"
    );

    // FFmpeg writes a segment every few seconds: a pause no longer than the last gap between
    // writes keeps the speed instead of dropping to zero.
    let quiet = gauge
        .sample(100, at(3000))
        .expect("another second has passed");
    assert_eq!(quiet.downloaded, 100 + 4096 + 8192);
    assert_eq!(quiet.speed, 4096);

    // A pause longer than that gap means the stream slowed: the speed falls.
    let slower = gauge.sample(100, at(6000)).expect("more time has passed");
    assert_eq!(slower.speed, 2048);

    // Far overdue: the recording has stopped growing.
    let stalled = gauge.sample(100, at(13000)).expect("more time has passed");
    assert_eq!(stalled.speed, 0);
    assert_eq!(stalled.downloaded, 100 + 4096 + 8192);

    drop(writer);
    fs::remove_dir_all(&root).unwrap();
}

/// "Finish and keep" ends the tool like a cancellation but reports its own outcome, so the
/// download keeps what was recorded instead of failing.
#[test]
fn a_finish_request_stops_the_tool_with_its_own_outcome() {
    let root = std::env::temp_dir().join(format!("ssdownload-tree-test-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    let (mut running, grandchild) = spawn_tool_with_grandchild(&root.join("grandchild.pid"));
    assert!(process_is_running(grandchild));
    let control = TransferControl::default();
    control.finish_recording();
    assert!(
        !control.stop_requested(),
        "the kept file must still be verified, which a stop would refuse"
    );

    let result = monitor_download(
        &mut running,
        &control,
        0,
        &mut |_| {},
        &mut HashSet::new(),
        &mut None,
        None,
    )
    .unwrap();

    assert!(matches!(result, MonitorOutcome::Finished));
    assert_tool_tree_ended(grandchild);
    drop(running);
    fs::remove_dir_all(&root).unwrap();
}

/// The beginning is recorded only where yt-dlp answered with formats that start there; a
/// site without them is recorded from now on.
#[test]
fn the_beginning_is_recorded_only_where_the_site_offers_it() {
    let offered = json!({"is_live": true, "formats": [
        {"format_id": "160p", "is_from_start": true},
        {"format_id": "audio", "is_from_start": true}
    ]});
    assert!(from_start_formats(&offered));
    let live_only = json!({"is_live": true, "formats": [{"format_id": "160p"}]});
    assert!(!from_start_formats(&live_only));
    let marked_false = json!({"formats": [{"format_id": "160p", "is_from_start": false}]});
    assert!(!from_start_formats(&marked_false));
    assert!(!from_start_formats(&json!({"title": "no formats"})));
}

/// A stopped recording keeps its `.part` outputs; the per-fragment pieces and empty files
/// are not media to keep.
#[test]
fn a_stopped_recording_keeps_only_its_part_outputs() {
    let root = std::env::temp_dir().join(format!("ssdownload-live-test-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("media.f140.m4a.part"), b"audio").unwrap();
    fs::write(root.join("media.f299.mp4.part"), b"video").unwrap();
    fs::write(root.join("media.f299.mp4.part-Frag7.part"), b"piece").unwrap();
    fs::write(root.join("empty.mp4.part"), b"").unwrap();
    fs::write(root.join("media.info.json"), b"{}").unwrap();

    let parts = live_recording_parts(&root).unwrap();
    assert_eq!(
        parts,
        vec![
            root.join("media.f140.m4a.part"),
            root.join("media.f299.mp4.part")
        ]
    );
    let args = live_recording_remux_args(&parts, &root.join("media.mp4"), "mp4", Some(61.0333));
    let maps: Vec<&str> = args
        .windows(2)
        .filter(|pair| pair[0] == "-map")
        .map(|pair| pair[1].as_str())
        .collect();
    assert_eq!(
        maps,
        ["0:v?", "0:a?", "1:v?", "1:a?"],
        "timed metadata stays out"
    );
    assert_eq!(args.iter().filter(|arg| *arg == "-i").count(), 2);
    assert!(args.windows(2).any(|pair| pair == ["-c", "copy"]));
    assert!(args.windows(2).any(|pair| pair == ["-f", "mp4"]));
    assert!(
        args.windows(2).any(|pair| pair == ["-t", "61.033"]),
        "the cut-off final frame stays out"
    );
    let mkv = live_recording_remux_args(&parts, &root.join("media.mkv"), "mkv", None);
    assert!(mkv.windows(2).any(|pair| pair == ["-f", "matroska"]));
    assert!(!mkv.iter().any(|arg| arg == "-movflags"));
    assert!(!mkv.iter().any(|arg| arg == "-t"));
    fs::remove_dir_all(&root).unwrap();
}

/// A live job's monitor reports the recorded size even though the tool prints nothing.
#[test]
fn monitor_reports_live_recording_size_from_a_silent_process() {
    let root = std::env::temp_dir().join(format!("ssdownload-live-test-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("media.mp4.part"), vec![0u8; 2048]).unwrap();
    let args = vec!["/C".into(), "ping -n 10 127.0.0.1 > nul".into()];
    let mut running =
        RunningDownload::spawn_with_proxy(Path::new("cmd.exe"), &args, b"", None).unwrap();
    let control = TransferControl::default();
    control.cancel();
    let mut gauge = LiveRecordingGauge::new(&root);
    let mut reported = Vec::new();

    let result = monitor_download(
        &mut running,
        &control,
        0,
        &mut |update| reported.push(update.downloaded),
        &mut HashSet::new(),
        &mut None,
        Some(&mut gauge),
    )
    .unwrap();
    assert!(matches!(result, MonitorOutcome::Cancelled));
    assert_eq!(reported, vec![2048]);
    fs::remove_dir_all(&root).unwrap();
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
    let single = json!({"id":"x","title":"t","format_id":"0","protocol":"m3u8_native",
        "url":"https://cdn.example.test/live/index.m3u8","vcodec":"avc1","acodec":"aac"});
    let page = AddRequest {
        url: "https://example.test/watch?v=1".into(),
        ..manifest
    };
    assert!(selection_is_fragmented(&single, &page, "0"));
}

#[test]
fn the_automatic_choice_resolves_to_the_formats_the_download_takes() {
    let info = media_info_from_json(&json!({"formats": [
        {"format_id":"140","vcodec":"none","acodec":"mp4a.40.2","ext":"m4a"},
        {"format_id":"251","vcodec":"none","acodec":"opus","ext":"webm"},
        {"format_id":"137","vcodec":"avc1.640028","acodec":"none","height":1080,"ext":"mp4"},
        {"format_id":"313","vcodec":"vp9","acodec":"none","height":2160,"ext":"webm"}
    ]}))
    .unwrap();
    let ids = |container: &str| {
        let request = AddRequest {
            url: "https://example.test/watch?v=1".into(),
            kind: DownloadKind::Video,
            container: Some(container.into()),
            ..AddRequest::default()
        };
        selected_formats(&request, &info)
            .iter()
            .map(|format| format.id.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(ids("mp4"), ["137", "140"]);
    assert_eq!(ids("mkv"), ["313", "251"]);
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

/// A new speed limit ends a plain download so it can resume with the limit; a live recording
/// keeps running, because a restart would cut the capture and start it over from the present.
#[test]
fn a_new_speed_limit_restarts_a_download_but_not_a_live_recording() {
    let args: Vec<String> = vec!["/C".into(), "ping -n 10 127.0.0.1 > nul".into()];

    let mut plain =
        RunningDownload::spawn_with_proxy(Path::new("cmd.exe"), &args, b"", None).unwrap();
    let control = TransferControl::default();
    control.set_speed_limit(1_000_000);
    let outcome = monitor_download(
        &mut plain,
        &control,
        0,
        &mut |_| {},
        &mut HashSet::new(),
        &mut None,
        None,
    )
    .unwrap();
    assert!(matches!(outcome, MonitorOutcome::RestartForSpeed));

    let root = std::env::temp_dir().join(format!("ssdownload-live-test-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    let mut live =
        RunningDownload::spawn_with_proxy(Path::new("cmd.exe"), &args, b"", None).unwrap();
    let control = TransferControl::default();
    control.set_speed_limit(1_000_000);
    let stopper = {
        let control = control.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(2500));
            control.cancel();
        })
    };
    let mut gauge = LiveRecordingGauge::new(&root);
    let outcome = monitor_download(
        &mut live,
        &control,
        0,
        &mut |_| {},
        &mut HashSet::new(),
        &mut None,
        Some(&mut gauge),
    )
    .unwrap();
    stopper.join().unwrap();
    assert!(
        matches!(outcome, MonitorOutcome::Cancelled),
        "the recording must run until it is stopped"
    );
    fs::remove_dir_all(&root).unwrap();
}

/// A restarted fragmented download starts its fragments again, whatever the stream is; a
/// download that is not fragmented resumes its partial file.
#[test]
fn a_restarted_fragmented_download_starts_its_fragments_again() {
    let root =
        std::env::temp_dir().join(format!("ssdownload-restart-test-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    let request = AddRequest {
        url: "https://example.test/stream.m3u8".into(),
        kind: DownloadKind::Video,
        ..AddRequest::default()
    };
    let plan = OutputPlan::new(&root.join("stream"), &request, None).unwrap();
    let partial = plan.workspace.join("media.f299.mp4.part");
    fs::write(&partial, b"partial").unwrap();

    discard_fragmented_restart(false, &plan, &HashSet::new(), "job").unwrap();
    assert!(partial.exists());

    discard_fragmented_restart(true, &plan, &HashSet::new(), "job").unwrap();
    assert!(!partial.exists());
    remove_workspace(&plan.workspace, &plan.owner).unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn only_a_pinned_source_enables_file_urls() {
    let info = Path::new(r"C:\lease\source.info.json");
    let pinned = load_info_args(info, true);
    let unpinned = load_info_args(info, false);
    assert_eq!(
        pinned,
        [
            "--enable-file-urls",
            "--load-info-json",
            r"C:\lease\source.info.json"
        ]
    );
    assert_eq!(
        unpinned,
        ["--load-info-json", r"C:\lease\source.info.json"],
        "an extractor document built from site data must not be able to open a file or share"
    );
}

#[test]
fn an_extractor_document_cannot_name_a_file_or_a_network_share() {
    let hostile = [
        json!({"formats": [{"format_id": "a", "url": "file:////attacker/share/a.m3u8"}]}),
        json!({"entries": [{"formats": [{"format_id": "a", "url": "https://cdn.example/a",
            "fragments": [{"url": "FILE:///C:/Users/x/secret"}]}]}]}),
        json!({"formats": [{"format_id": "a", "url": "https://cdn.example/a",
            "manifest_url": "file://attacker/share/master.m3u8"}]}),
        json!({"subtitles": {"en": [{"ext": "vtt", "url": "file:////attacker/share/en.vtt"}]}}),
        json!({"formats": [{"format_id": "dash", "url": "https://cdn.example/a.mpd",
            "fragment_base_url": "https://cdn.example/",
            "fragments": [{"path": "file:////attacker/share/1.m4s"}]}]}),
    ];
    for document in &hostile {
        assert!(reject_file_urls(document).is_err(), "{document}");
    }
    let fine = json!({"webpage_url": "https://example.test/watch", "formats": [
        {"format_id": "a", "url": "https://cdn.example/a.m3u8", "manifest_url": "https://cdn.example/m.m3u8",
         "fragments": [{"url": "https://cdn.example/1.ts"}, {"path": "2.ts"}]},
        {"format_id": "pinned", "url": "ssdownload-pinned-manifest:0"},
        {"format_id": "inline", "url": "data:application/vnd.apple.mpegurl;base64,I0VYVE0zVQ=="}
    ], "subtitles": {"en": [{"ext": "vtt", "url": "https://cdn.example/en.vtt"}]}});
    reject_file_urls(&fine).unwrap();
}

#[test]
fn dash_format_ids_with_an_equals_sign_are_selectable_and_selector_syntax_stays_rejected() {
    let (video, audio) = ("video=1500000", "dash-audio_eng=128000");
    let info = media_info_from_json(&json!({"formats": [
        {"format_id": video, "height": 720, "vcodec": "avc1.64001f", "acodec": "none"},
        {"format_id": audio, "vcodec": "none", "acodec": "mp4a.40.2", "language": "en"}
    ]}))
    .unwrap();
    let request = AddRequest {
        kind: DownloadKind::Video,
        video_format_id: Some(video.into()),
        audio_format_id: Some(audio.into()),
        ..AddRequest::default()
    };
    validate_options(&request).unwrap();
    assert_eq!(
        format_selector(&request, &info).unwrap(),
        format!("{video}+{audio}")
    );
    for id in [
        "a+b", "a/b", "a,b", "a[b", "a]b", "a(b", "a)b", "a b", "a'b", "",
    ] {
        assert!(validate_format_id(id, "id").is_err(), "{id:?}");
    }
}

#[test]
fn a_separate_audio_track_is_refused_for_a_video_format_that_already_has_audio() {
    let info = media_info_from_json(&json!({"formats": [
        {"format_id": "muxed", "height": 720, "vcodec": "avc1.64001f", "acodec": "mp4a.40.2"},
        {"format_id": "silent", "height": 720, "vcodec": "avc1.64001f", "acodec": "none"},
        {"format_id": "dub-tr", "vcodec": "none", "acodec": "mp4a.40.2", "language": "tr"}
    ]}))
    .unwrap();
    let request = |video: &str| AddRequest {
        kind: DownloadKind::Video,
        video_format_id: Some(video.into()),
        audio_format_id: Some("dub-tr".into()),
        ..AddRequest::default()
    };
    assert!(validate_track_selection(&request("muxed"), &info).is_err());
    assert!(format_selector(&request("muxed"), &info).is_err());
    validate_track_selection(&request("silent"), &info).unwrap();
    assert_eq!(
        format_selector(&request("silent"), &info).unwrap(),
        "silent+dub-tr"
    );
}

#[test]
fn a_rounded_extractor_frame_rate_matches_the_exact_ntsc_rate_but_not_another_rate() {
    let info = media_info_from_json(&json!({"formats": [
        {"format_id": "v60", "height": 720, "vcodec": "avc1.64001f", "acodec": "none", "fps": 60},
        {"format_id": "audio", "vcodec": "none", "acodec": "mp4a.40.2"}
    ]}))
    .unwrap();
    let request = AddRequest {
        kind: DownloadKind::Video,
        video_format_id: Some("v60".into()),
        ..AddRequest::default()
    };
    let probe = |rate: &str| {
        json!({"streams": [
            {"codec_type": "video", "codec_name": "h264", "height": 720, "avg_frame_rate": rate},
            {"codec_type": "audio", "codec_name": "aac"}
        ], "format": {"duration": "2"}})
    };
    validate_probe(&probe("60000/1001"), &request, &info).unwrap();
    validate_probe(&probe("60/1"), &request, &info).unwrap();
    assert!(validate_probe(&probe("30000/1001"), &request, &info).is_err());
    assert!(validate_probe(&probe("50/1"), &request, &info).is_err());
}

/// The extractor lists formats from worst to best in its own preference order; an alternate
/// DRC mix with more channels and bytes is listed below the original track.
#[test]
fn the_mp4_audio_pick_follows_the_extractors_preference_order() {
    let info = media_info_from_json(&json!({"formats": [
        {"format_id": "a-drc", "vcodec": "none", "acodec": "mp4a.40.2", "audio_channels": 6,
         "filesize": 9_000_000, "language": "en"},
        {"format_id": "a-main", "vcodec": "none", "acodec": "mp4a.40.2", "audio_channels": 2,
         "filesize": 1_000_000, "language": "en"},
        {"format_id": "v720", "height": 720, "vcodec": "avc1.64001f", "acodec": "none"}
    ]}))
    .unwrap();
    let request = AddRequest {
        url: "https://example.test/watch/1".into(),
        kind: DownloadKind::Video,
        container: Some("mp4".into()),
        max_height: Some(720),
        ..AddRequest::default()
    };
    assert_eq!(format_selector(&request, &info).unwrap(), "v720+a-main");
    assert_eq!(best_mp4_audio(&request, &info).unwrap().id, "a-main");
}

#[test]
fn a_playlist_mp4_selector_resolves_for_every_entry_not_only_the_sampled_one() {
    let sampled = media_info_from_json(&json!({"formats": [
        {"format_id": "f-avc", "height": 720, "vcodec": "avc1.64001f", "acodec": "none"},
        {"format_id": "f-aac", "vcodec": "none", "acodec": "mp4a.40.2"},
        {"format_id": "f-vp9", "height": 720, "vcodec": "vp09.00.31.08", "acodec": "none"},
        {"format_id": "f-opus", "vcodec": "none", "acodec": "opus"}
    ]}))
    .unwrap();
    let other = media_info_from_json(&json!({"formats": [
        {"format_id": "s-137", "height": 720, "vcodec": "avc1.4d401f", "acodec": "none"},
        {"format_id": "s-140", "vcodec": "none", "acodec": "mp4a.40.2"},
        {"format_id": "s-248", "height": 720, "vcodec": "vp9", "acodec": "none"},
        {"format_id": "s-251", "vcodec": "none", "acodec": "opus"},
        {"format_id": "s-1080", "height": 1080, "vcodec": "avc1.640028", "acodec": "none"}
    ]}))
    .unwrap();
    let mut request = AddRequest {
        url: "https://example.test/playlist".into(),
        kind: DownloadKind::Video,
        container: Some("mp4".into()),
        playlist: true,
        max_height: Some(720),
        ..AddRequest::default()
    };
    let video = "[vcodec~=?'^(avc|h264|hevc|h265|hev1|hvc1|dvh1|dvhe|av01|av1|mpeg4|mp4v)']";
    let audio = "[acodec~=?'^(aac|mp4a|alac|mp3|ac3|ac-3|eac3|ec-3)']";
    let selector = format_selector(&request, &sampled).unwrap();
    assert_eq!(
        selector,
        format!("bestvideo[height<=?720]{video}+bestaudio{audio}/best[height<=?720]{video}{audio}")
    );
    assert_eq!(
        selected_format_ids(&selector, &sampled).unwrap(),
        ["f-avc", "f-aac"]
    );
    assert_eq!(
        selected_format_ids(&selector, &other).unwrap(),
        ["s-137", "s-140"],
        "another entry has other format ids and the codec filters still pick MP4-compatible streams"
    );

    request.max_height = None;
    request.exact_height = Some(720);
    assert!(format_selector(&request, &sampled)
        .unwrap()
        .starts_with("bestvideo[height=720][vcodec~=?"));

    request.playlist = false;
    assert_eq!(format_selector(&request, &sampled).unwrap(), "f-avc+f-aac");
}

/// An HLS audio rendition has no `acodec` and a variant may have no `height`; yt-dlp drops a
/// format without the filtered field unless the operator carries a `?`.
#[test]
fn a_playlist_mp4_selector_keeps_formats_without_a_codec_or_a_height() {
    let sampled = media_info_from_json(&json!({"formats": [
        {"format_id": "v-noheight", "vcodec": "avc1.4d401f", "acodec": "none"},
        {"format_id": "v-720", "height": 720, "vcodec": "avc1.64001f", "acodec": "none"},
        {"format_id": "a-main", "vcodec": "none"},
        {"format_id": "a-opus", "vcodec": "none", "acodec": "opus"}
    ]}))
    .unwrap();
    let other = media_info_from_json(&json!({"formats": [
        {"format_id": "s-video", "vcodec": "avc1.4d401f", "acodec": "none"},
        {"format_id": "s-audio", "vcodec": "none"}
    ]}))
    .unwrap();
    let request = AddRequest {
        url: "https://example.test/playlist".into(),
        kind: DownloadKind::Video,
        container: Some("mp4".into()),
        playlist: true,
        max_height: Some(720),
        ..AddRequest::default()
    };

    let selector = format_selector(&request, &sampled).unwrap();
    assert!(selector.contains("[height<=?720]"), "{selector}");
    assert!(selector.contains("[vcodec~=?'^("), "{selector}");
    assert!(selector.contains("[acodec~=?'^("), "{selector}");
    assert_eq!(
        selected_format_ids(&selector, &sampled).unwrap(),
        ["v-720", "a-main"],
        "the rendition without a codec is chosen, the Opus one listed after it is not"
    );
    assert_eq!(
        selected_format_ids(&selector, &other).unwrap(),
        ["s-video", "s-audio"]
    );
}

#[test]
fn the_evidence_selector_follows_ytdlp_for_a_format_without_the_filtered_field() {
    let info = media_info_from_json(&json!({"formats": [
        {"format_id": "v-noheight", "vcodec": "avc1.4d401f", "acodec": "none"},
        {"format_id": "a-main", "vcodec": "none"},
        {"format_id": "a-opus", "vcodec": "none", "acodec": "opus"}
    ]}))
    .unwrap();
    let ids = |selector: &str| selected_format_ids(selector, &info).ok();

    assert_eq!(ids("bestvideo[height<=720]"), None);
    assert_eq!(
        ids("bestvideo[height<=?720]"),
        Some(vec!["v-noheight".to_string()])
    );
    assert_eq!(ids("bestaudio[acodec~='^(vorbis)']"), None);
    assert_eq!(
        ids("bestaudio[acodec~=?'^(vorbis)']"),
        Some(vec!["a-main".to_string()]),
        "the rendition without a codec passes, the reported Opus one does not"
    );
}

/// The picker keeps the selected audio language for a playlist; the sampled entry's format ids
/// would fail every entry that has other ids.
#[test]
fn a_playlist_with_an_audio_language_does_not_pin_the_sampled_entrys_ids() {
    let sampled = media_info_from_json(&json!({"formats": [
        {"format_id": "f-avc", "height": 720, "vcodec": "avc1.64001f", "acodec": "none"},
        {"format_id": "f-en", "vcodec": "none", "acodec": "mp4a.40.2", "language": "en"},
        {"format_id": "f-tr", "vcodec": "none", "acodec": "mp4a.40.2", "language": "tr"}
    ]}))
    .unwrap();
    let other = media_info_from_json(&json!({"formats": [
        {"format_id": "s-137", "height": 720, "vcodec": "avc1.4d401f", "acodec": "none"},
        {"format_id": "s-en", "vcodec": "none", "acodec": "mp4a.40.2", "language": "en"},
        {"format_id": "s-tr", "vcodec": "none", "acodec": "mp4a.40.2", "language": "tr"}
    ]}))
    .unwrap();
    let mut request = AddRequest {
        url: "https://example.test/playlist".into(),
        kind: DownloadKind::Video,
        container: Some("mp4".into()),
        playlist: true,
        max_height: Some(720),
        audio_language: Some("tr".into()),
        ..AddRequest::default()
    };

    let selector = format_selector(&request, &sampled).unwrap();
    assert!(selector.contains("[language=tr]"), "{selector}");
    assert!(
        !selector.contains("f-avc") && !selector.contains("f-tr"),
        "{selector}"
    );
    assert_eq!(
        selected_format_ids(&selector, &other).unwrap(),
        ["s-137", "s-tr"]
    );

    request.playlist = false;
    assert_eq!(format_selector(&request, &sampled).unwrap(), "f-avc+f-tr");
}

#[test]
fn a_converted_audio_file_is_not_compared_with_the_source_track() {
    let info = media_info_from_json(&json!({"formats": [
        {"format_id": "audio-tr", "vcodec": "none", "acodec": "opus", "language": "tr"}
    ]}))
    .unwrap();
    let mut request = AddRequest {
        kind: DownloadKind::Audio,
        audio_format: Some("mp3".into()),
        audio_format_id: Some("audio-tr".into()),
        audio_language: Some("tr".into()),
        ..AddRequest::default()
    };
    let mp3 = json!({"streams": [{"codec_type": "audio", "codec_name": "mp3"}],
        "format": {"duration": "2"}});

    validate_probe(&mp3, &request, &info).unwrap();
    assert!(!stream_postprocess_needed(&request).unwrap());

    // The original keeps the track, so it is still compared and tagged.
    request.audio_format = None;
    assert!(stream_postprocess_needed(&request).unwrap());
    let opus = |tag: Value| {
        json!({"streams": [{"codec_type": "audio", "codec_name": "opus", "tags": tag}],
            "format": {"duration": "2"}})
    };
    validate_probe(&opus(json!({"language": "tur"})), &request, &info).unwrap();
    assert!(validate_probe(&mp3, &request, &info).is_err());
    let untagged = validate_probe(&opus(json!({})), &request, &info).unwrap_err();
    assert_eq!(
        crate::error_codes::code_of(&untagged),
        Some(crate::error_codes::MED_016)
    );
}

#[test]
fn every_ffmpeg_input_carries_its_own_protocol_whitelist() {
    fn assert_each_input_is_guarded(args: &[String], whitelist: &str, inputs: usize) {
        let at: Vec<usize> = (0..args.len()).filter(|i| args[*i] == "-i").collect();
        assert_eq!(at.len(), inputs, "{args:?}");
        for index in at {
            assert_eq!(
                args[index - 2..index],
                ["-protocol_whitelist", whitelist],
                "the input {} has no whitelist of its own",
                args[index + 1]
            );
        }
    }
    let stream = |file: &str, role: &str| crate::browser_transfer::StreamFile {
        path: PathBuf::from(file),
        role: role.into(),
        format_id: None,
        language: None,
    };
    let request = AddRequest {
        kind: DownloadKind::Video,
        container: Some("mp4".into()),
        ..AddRequest::default()
    };
    let streams = [
        stream("video.bin", "video"),
        stream("audio.bin", "audio"),
        stream("subtitle.bin", "subtitle"),
    ];
    let mux = browser_mux_args(&request, &streams, Path::new("out.tmp"));
    assert_each_input_is_guarded(&mux, "file,crypto,data", 3);

    let subtitles = [PathBuf::from("s0.txt"), PathBuf::from("s1.txt")];
    let tagged = selected_output_args(
        Path::new("media.mp4"),
        &subtitles,
        Path::new("selected-output.mp4"),
        &request,
        0,
    );
    assert_each_input_is_guarded(&tagged, "file,pipe", 3);
}

#[test]
fn the_processing_state_comes_from_the_tool_stage_not_the_phase_text() {
    let mut seen = Vec::new();
    let mut completed = HashSet::new();
    let mut last_path = None;
    let mut gauge = DownloadGauge::new();
    for line in [
        format!("{PROGRESS_PREFIX}downloading	100	1000"),
        format!("{PROGRESS_PREFIX}finished	1000	1000"),
        format!("{POSTPROCESS_PREFIX}started"),
    ] {
        process::handle_machine_line(
            &line,
            Instant::now(),
            &mut |value| seen.push(value.processing),
            &mut completed,
            &mut last_path,
            &mut gauge,
        );
    }
    assert_eq!(seen, [false, true, true]);
}

/// Plays a tool's progress lines into the monitor's gauge on a clock the test moves, waking the
/// monitor every 100 ms between the lines as its loop does.
struct Replay {
    origin: Instant,
    now: Duration,
    gauge: DownloadGauge,
    completed: HashSet<PathBuf>,
    last_path: Option<PathBuf>,
    events: Vec<(Duration, TransferProgress)>,
}

fn ms(value: u64) -> Duration {
    Duration::from_millis(value)
}

impl Replay {
    fn new() -> Self {
        let gauge = DownloadGauge::new();
        Self {
            origin: Instant::now(),
            now: Duration::ZERO,
            gauge,
            completed: HashSet::new(),
            last_path: None,
            events: Vec::new(),
        }
    }

    fn wait_until(&mut self, to: Duration) {
        while self.now + ms(100) <= to {
            self.now += ms(100);
            if let Some(update) = self.gauge.repeat(self.origin + self.now) {
                self.events.push((self.now, update));
            }
        }
    }

    fn line(&mut self, at: Duration, fields: &str) {
        self.wait_until(at);
        self.now = self.now.max(at);
        let events = &mut self.events;
        process::handle_machine_line(
            &format!("{PROGRESS_PREFIX}{fields}"),
            self.origin + at,
            &mut |update| events.push((at, update)),
            &mut self.completed,
            &mut self.last_path,
            &mut self.gauge,
        );
    }

    /// One line every 100 ms for `seconds` at `rate` bytes a second, counting on from `start`.
    fn flow(&mut self, from: Duration, seconds: u64, rate: u64, start: u64, total: u64) {
        for step in 1..=seconds * 10 {
            self.line(
                from + ms(step * 100),
                &format!("downloading\t{}\t{total}", start + step * rate / 10),
            );
        }
    }
}

/// The lines are what yt-dlp 2026.10.26 printed (their offset in ms from the first line) for 300
/// HLS fragments over 4 connections, from the start, the middle and the end of the run. The
/// counts of its download threads reach the line out of order (36863840, then 36860768). Its
/// own speed on the last line was 10323959.
#[test]
fn the_speed_follows_the_bytes_of_recorded_progress_lines_whatever_their_order() {
    let recorded = [
        (0, "downloading\t1024\t37200000.0"),
        (1, "downloading\t3072\t37507200.0"),
        (2, "downloading\t7168\t38121600.0"),
        (1767, "downloading\t18235168\t37206227.027027026"),
        (1768, "downloading\t18243360\t37214529.72972973"),
        (3578, "downloading\t36828000\t37325252.52525253"),
        (3582, "downloading\t36829024\t37200000.0"),
        (3583, "downloading\t36831072\t37201030.872483216"),
        (3585, "downloading\t36835168\t37203092.61744966"),
        (3586, "downloading\t36843360\t37207216.10738255"),
        (3589, "downloading\t36844384\t37215463.087248325"),
        (3590, "downloading\t36861792\t37232987.91946309"),
        (3591, "downloading\t36863840\t37234018.79194631"),
        (3592, "downloading\t36860768\t37216493.95973154"),
        (3593, "downloading\t36869984\t37238142.281879194"),
        (3594, "downloading\t36902752\t37242265.77181208"),
        (3596, "downloading\t36910944\t37275253.691275164"),
        (3597, "downloading\t36970432\t37283500.67114094"),
        (3597, "downloading\t36986816\t37343387.91946309"),
        (3599, "downloading\t36865888\t37236080.536912754"),
        (3600, "downloading\t37019584\t37359881.87919463"),
        (3600, "downloading\t37023680\t37392869.798657715"),
        (3603, "downloading\t37031872\t37396993.2885906"),
        (3605, "downloading\t37091360\t37405240.26845638"),
        (3605, "downloading\t37107744\t37339826.08695652"),
        (3607, "downloading\t37091360\t37465127.51677852"),
        (3607, "downloading\t37140512\t37356264.88294314"),
        (3608, "downloading\t37200000\t37389142.47491639"),
        (3610, "downloading\t37200000\t37448829.431438126"),
        (3612, "downloading\t37200000\t37324000.0"),
    ];
    let mut replay = Replay::new();
    for (at, fields) in recorded {
        replay.line(ms(at), fields);
    }
    let last = &replay.events.last().unwrap().1;
    assert_eq!(last.downloaded, 37_200_000);
    assert!(
        last.speed.abs_diff(10_323_959) < 10_323_959 / 20,
        "{}",
        last.speed
    );
    assert_eq!(last.eta, Some(0));
}

/// A source that answers 429 after a few seconds of service makes yt-dlp wait out its retries:
/// 100 segments of 124000 bytes arrive within 1.5 s, then nothing for 17 s. The tool's own speed
/// and time left read 0 and unknown at the first line of every burst.
#[test]
fn a_source_that_sends_in_bursts_keeps_a_speed_and_a_time_left_between_them() {
    const BURST: u64 = 12_400_000;
    let mut replay = Replay::new();
    for burst in 0..8 {
        let start = ms(burst * 18_500);
        for step in 1..=15 {
            replay.line(
                start + ms(step * 100),
                &format!(
                    "downloading\t{}\t154318396",
                    burst * BURST + step * BURST / 15
                ),
            );
        }
    }
    replay.wait_until(ms(8 * 18_500));

    let average = BURST as f64 / 18.5;
    let later: Vec<_> = replay
        .events
        .iter()
        .filter(|(at, _)| *at >= ms(40_000))
        .collect();
    assert!(later.len() > 100, "{}", later.len());
    for (at, update) in later {
        let speed = update.speed as f64;
        assert!(
            speed > 0.75 * average && speed < 1.4 * average,
            "{at:?}: {update:?}"
        );
        assert!(update.eta.is_some(), "{at:?}: {update:?}");
    }
}

#[test]
fn a_download_that_stops_reads_a_falling_speed_that_ends_at_zero() {
    let mut replay = Replay::new();
    replay.flow(Duration::ZERO, 10, 2_000_000, 0, 100_000_000);
    replay.wait_until(ms(100_000));

    let silent: Vec<_> = replay
        .events
        .iter()
        .filter(|(at, _)| *at > ms(10_000))
        .collect();
    assert!(silent.len() >= 85, "{}", silent.len());
    assert!(
        silent
            .windows(2)
            .all(|pair| pair[1].1.speed <= pair[0].1.speed),
        "the speed rose while no byte arrived"
    );
    let at = |seconds: u64| {
        silent
            .iter()
            .find(|(at, _)| *at >= ms(10_000 + seconds * 1000))
            .unwrap()
            .1
            .clone()
    };
    assert!(at(2).speed > 1_000_000 && at(2).eta.is_some());
    assert!(at(30).speed > 0);
    let end = &silent.last().unwrap().1;
    assert_eq!((end.speed, end.eta), (0, None));
}

#[test]
fn a_steady_download_follows_a_change_of_its_rate_within_seconds() {
    let mut replay = Replay::new();
    replay.flow(Duration::ZERO, 30, 1_000_000, 0, 100_000_000);
    replay.flow(ms(30_000), 10, 250_000, 30_000_000, 100_000_000);

    let (_, update) = replay
        .events
        .iter()
        .find(|(at, _)| *at >= ms(38_000))
        .unwrap();
    assert!(update.speed.abs_diff(250_000) < 25_000, "{}", update.speed);
}

#[test]
fn the_next_stream_of_a_download_is_measured_from_its_own_start() {
    let mut replay = Replay::new();
    replay.flow(Duration::ZERO, 5, 1_000_000, 0, 5_000_000);
    replay.line(ms(5_000), "finished\t5000000\t5000000");
    replay.flow(ms(5_000), 3, 1_000_000, 0, 3_000_000);

    let (_, update) = replay.events.last().unwrap();
    assert!(!update.processing);
    assert!(
        update.speed.abs_diff(1_000_000) < 100_000,
        "{}",
        update.speed
    );
}

/// A download that starts over inside one run (a server that ignores the range of a resumed
/// request) counts from zero without a finished stream.
#[test]
fn a_download_that_starts_over_is_measured_afresh() {
    let mut replay = Replay::new();
    replay.flow(Duration::ZERO, 100, 1_000_000, 0, 400_000_000);
    replay.flow(ms(100_000), 70, 1_000_000, 0, 400_000_000);

    assert_one_megabyte_a_second_after(&replay, ms(100_000));
}

/// The size of the playlist items finished before it is part of the displayed count but not of
/// the stream's own.
#[test]
fn a_playlist_item_that_starts_over_is_measured_afresh() {
    let folder = crate::test_http::TempDir::new("playlist-item");
    let finished = folder.0.join("first.mp4");
    File::create(&finished)
        .unwrap()
        .set_len(300_000_000)
        .unwrap();
    let mut replay = Replay::new();
    replay.completed.insert(finished);
    replay.flow(Duration::ZERO, 100, 1_000_000, 0, 400_000_000);
    replay.flow(ms(100_000), 70, 1_000_000, 0, 400_000_000);

    assert_one_megabyte_a_second_after(&replay, ms(100_000));
}

fn assert_one_megabyte_a_second_after(replay: &Replay, restart: Duration) {
    for seconds in [10, 30, 50, 70] {
        let (_, update) = replay
            .events
            .iter()
            .find(|(at, _)| *at >= restart + ms(seconds * 1000))
            .unwrap();
        assert!(
            update.speed.abs_diff(1_000_000) < 100_000 && update.eta.is_some(),
            "{seconds} s after the restart: {update:?}"
        );
    }
}

/// A fragment retry takes a few MB off the count of a large download: that is no restart, so
/// the measurement goes on.
#[test]
fn a_drop_of_a_few_megabytes_in_a_large_count_is_not_a_restart() {
    let mut replay = Replay::new();
    replay.flow(Duration::ZERO, 100, 1_000_000, 0, 400_000_000);
    replay.line(ms(100_100), "downloading\t95000000\t400000000");
    replay.flow(ms(100_100), 10, 1_000_000, 95_000_000, 400_000_000);

    let (_, update) = replay
        .events
        .iter()
        .find(|(at, _)| *at >= ms(100_600))
        .unwrap();
    assert!(update.speed > 500_000 && update.eta.is_some(), "{update:?}");
}

/// Counts of the tool's threads that arrive late fall behind the highest one by at most a few
/// blocks; early in a download that can be more than half of it.
#[test]
fn a_late_count_early_in_a_download_does_not_restart_the_measurement() {
    let mut replay = Replay::new();
    replay.flow(Duration::ZERO, 2, 200_000, 0, 10_000_000);
    replay.line(ms(2_050), "downloading\t50000\t10000000");
    replay.flow(ms(2_050), 8, 200_000, 400_000, 10_000_000);

    let (_, update) = replay
        .events
        .iter()
        .find(|(at, _)| *at >= ms(2_500))
        .unwrap();
    assert!(update.speed.abs_diff(200_000) < 20_000, "{}", update.speed);
}

/// Runs a PowerShell script as the tool and returns when each progress update reached the
/// monitor's callback.
fn updates_from_script(
    script: &str,
    live: Option<&mut LiveRecordingGauge>,
) -> Vec<(Instant, TransferProgress)> {
    let args = vec![
        "-NoProfile".into(),
        "-NonInteractive".into(),
        "-Command".into(),
        script.to_owned(),
    ];
    let mut running =
        RunningDownload::spawn_with_proxy(Path::new("powershell.exe"), &args, b"", None).unwrap();
    let mut updates = Vec::new();
    let outcome = monitor_download(
        &mut running,
        &TransferControl::default(),
        0,
        &mut |update| updates.push((Instant::now(), update)),
        &mut HashSet::new(),
        &mut None,
        live,
    )
    .unwrap();
    assert!(matches!(outcome, MonitorOutcome::Exited(status) if status.success()));
    updates
}

/// Five lines 300 ms apart, then 3 s of silence before the tool exits.
const SLOW_THEN_SILENT: &str = "$p = \"SSDOWNLOAD_PROGRESS`tdownloading`t\"; \
    foreach ($i in 1..5) { [Console]::Out.WriteLine(\"$p$($i * 500000)`t10000000\"); \
    [Console]::Out.Flush(); Start-Sleep -Milliseconds 300 }; Start-Sleep -Seconds 3";

#[test]
fn lines_a_tool_writes_slowly_reach_the_callback_as_they_are_written() {
    let updates = updates_from_script(SLOW_THEN_SILENT, None);

    let arrival = |downloaded| {
        updates
            .iter()
            .find(|(_, update)| update.downloaded == downloaded)
            .unwrap()
            .0
    };
    let spread = arrival(2_500_000).duration_since(arrival(500_000));
    assert!(spread >= Duration::from_millis(1000), "{spread:?}");
}

#[test]
fn a_silent_tool_does_not_freeze_the_progress() {
    let updates = updates_from_script(SLOW_THEN_SILENT, None);

    let last_line = updates
        .iter()
        .position(|(_, update)| update.downloaded == 2_500_000)
        .unwrap();
    let repeats: Vec<_> = updates[last_line + 1..].iter().collect();
    assert!(repeats.len() >= 2, "{}", repeats.len());
    let silence = repeats
        .last()
        .unwrap()
        .0
        .duration_since(updates[last_line].0);
    assert!(silence >= Duration::from_millis(1800), "{silence:?}");
    for (_, update) in &repeats {
        assert_eq!(update.downloaded, 2_500_000);
        assert!(update.speed > 0 && update.eta.is_some(), "{update:?}");
    }
    assert!(repeats
        .windows(2)
        .all(|pair| pair[1].1.speed <= pair[0].1.speed));
}

/// The tool's own counter of a live job covers one stream: it neither alternates with the size
/// of the recording nor is repeated while the tool is silent.
#[test]
fn a_live_recording_reports_only_its_own_size() {
    let folder = recording_folder();
    let mut recording = LiveRecordingGauge::new(&folder.0);

    let updates = updates_from_script(SLOW_THEN_SILENT, Some(&mut recording));

    assert!(updates.len() >= 3, "{}", updates.len());
    for (_, update) in &updates {
        assert_eq!(update.downloaded, 2048, "{update:?}");
    }
}

const LIVE_TOOL_LINE: &str = "SSDOWNLOAD_PROGRESS\tdownloading\t9999999\t10000000";

fn recording_folder() -> crate::test_http::TempDir {
    let folder = crate::test_http::TempDir::new("live-recording");
    fs::write(folder.0.join("media.mp4.part"), vec![0u8; 2048]).unwrap();
    folder
}

/// A tool that has exited already, so that the monitor's exit check finds it on its first pass.
fn exited_tool() -> RunningDownload {
    let args = vec!["/C".into(), "exit 0".into()];
    let mut running =
        RunningDownload::spawn_with_proxy(Path::new("cmd.exe"), &args, b"", None).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while running.child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "the tool never exited");
        thread::sleep(Duration::from_millis(20));
    }
    running
}

/// What the monitor reports for `running` as a live recording of a folder that holds 2048
/// bytes; `after_update` runs on the monitor's thread after each update.
fn live_updates(
    mut running: RunningDownload,
    mut after_update: impl FnMut(),
) -> Vec<TransferProgress> {
    let folder = recording_folder();
    let mut recording = LiveRecordingGauge::new(&folder.0);
    let mut updates = Vec::new();
    let outcome = monitor_download(
        &mut running,
        &TransferControl::default(),
        0,
        &mut |update| {
            updates.push(update);
            after_update();
        },
        &mut HashSet::new(),
        &mut None,
        Some(&mut recording),
    )
    .unwrap();
    assert!(matches!(outcome, MonitorOutcome::Exited(_)));
    assert!(!updates.is_empty());
    for update in &updates {
        assert_eq!(update.downloaded, 2048, "{update:?}");
    }
    updates
}

/// A line that waits in the queue when the monitor starts is read by the drain at the top of
/// its loop.
#[test]
fn a_live_progress_line_in_the_queue_is_not_reported() {
    let mut running = exited_tool();
    let (sender, lines) = mpsc::sync_channel(8);
    sender.send(LIVE_TOOL_LINE.into()).unwrap();
    running.lines = lines;

    live_updates(running, || {});
}

/// A line that arrives while the monitor reports, after it has read the queue and before it
/// finds that the tool exited, is read by the drain after the exit.
#[test]
fn a_live_progress_line_that_arrives_before_the_exit_check_is_not_reported() {
    let mut running = exited_tool();
    let (sender, lines) = mpsc::sync_channel(8);
    running.lines = lines;
    let mut sent = false;

    live_updates(running, || {
        if !std::mem::replace(&mut sent, true) {
            sender.send(LIVE_TOOL_LINE.into()).unwrap();
        }
    });
}

/// A line that arrives while the monitor waits for the tool's workers is read by the drain
/// after them.
#[test]
fn a_live_progress_line_that_arrives_while_the_workers_are_joined_is_not_reported() {
    let mut running = exited_tool();
    let (sender, lines) = mpsc::sync_channel(8);
    running.lines = lines;
    running.stdin_thread = Some(thread::spawn(move || {
        thread::sleep(Duration::from_millis(500));
        sender.send(LIVE_TOOL_LINE.into()).unwrap();
        Ok(())
    }));

    live_updates(running, || {});
}

#[test]
fn a_failure_is_classified_alike_in_every_interface_language() {
    let request = AddRequest {
        url: "https://page.example/watch".into(),
        ..AddRequest::default()
    };
    let timeout = crate::error_codes::definition(crate::error_codes::NET_007).unwrap();
    let english = |stage: &str| {
        classify_download_error(
            &format!("{stage}: SSD-NET-007 {}", timeout.message_en),
            &request,
        )
    };
    assert_eq!(english("media segment"), ("medya parçası", "timeout"));
    assert_eq!(english("discovery"), ("keşif", "timeout"));
    let messages: Vec<_> = {
        let (tr, en) = (timeout.message_tr, timeout.message_en);
        crate::i18n::wordings(tr, en).collect()
    };
    for (tr, en) in FAILURE_STAGES {
        for (stage, message) in crate::i18n::wordings(tr, en).zip(&messages) {
            assert_eq!(
                classify_download_error(&format!("{stage}: SSD-NET-007 {message}"), &request),
                english(en),
                "{stage}: {message}"
            );
        }
    }
}

#[test]
fn a_line_cut_at_the_limit_is_flagged_by_the_reader_not_by_its_marker_text() {
    let lines: Vec<_> = process::cut_lines("short\nabcdefghij\nlast".as_bytes(), 6)
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        lines,
        [
            ("short".to_string(), false),
            ("abcdef".to_string(), true),
            ("last".to_string(), false)
        ]
    );
}
