//! End-to-end HLS downloads through the installed yt-dlp and FFmpeg against a local server.
//! They need the media tools the app installs, so they run explicitly:
//! `cargo test --lib hls_tests -- --ignored`.

use super::*;
use std::{
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    sync::{Arc, Mutex},
    thread,
};

#[derive(Default)]
struct Source {
    video_seconds: u32,
    audio_seconds: u32,
    /// Answers 404 to this file's first requests.
    failing: Option<(&'static str, usize)>,
    /// Holds every segment response this long, so a test can act mid-download.
    segment_delay: Duration,
    /// A tag added to both media playlists.
    playlist_tag: Option<&'static str>,
    /// Downloads into a folder 164 characters deep, so the files of the media workspace pass
    /// the 259 characters of an unprefixed Windows path.
    deep_folder: bool,
}

struct Server {
    site: PathBuf,
    requests: Mutex<Vec<String>>,
    failing: Option<(&'static str, usize)>,
    segment_delay: Duration,
}

struct Fixture {
    root: PathBuf,
    /// Where the download is saved.
    folder: PathBuf,
    paths: AppPaths,
    master: String,
    server: Arc<Server>,
}

/// The cookie-jar folder (extractor documents included) is process-wide and set by the first
/// `AppPaths`, so every test shares one profile: a test that finishes never removes the folder
/// another test's download is using.
fn shared_profile() -> AppPaths {
    static ROOT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    let root = ROOT.get_or_init(|| {
        std::env::temp_dir().join(format!("ssdownload-hls-profile-{}", std::process::id()))
    });
    let mut paths = AppPaths::new(root.clone()).unwrap();
    paths.tools_dir = PathBuf::from(std::env::var("LOCALAPPDATA").unwrap())
        .join("SSDownload")
        .join("tools");
    paths
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

impl Fixture {
    /// Encodes the video and audio playlists in 2 s segments and serves them with a master
    /// playlist that offers the audio as a separate rendition.
    fn new(source: Source) -> Self {
        let root = std::env::temp_dir().join(format!("ssdownload-hls-{}", uuid::Uuid::new_v4()));
        let site = root.join("site");
        fs::create_dir_all(&site).unwrap();
        let paths = shared_profile();
        let tools = tools::verified_toolchain(&paths, &TransferControl::default())
            .expect("the installed media tools");
        let encode = |input: &str, codec: &[&str], playlist: &str, segment: &str| {
            let status = std::process::Command::new(&tools.ffmpeg)
                .args(["-v", "error", "-f", "lavfi", "-i", input])
                .args(codec)
                .args(["-f", "hls", "-hls_time", "2", "-hls_playlist_type", "vod"])
                .arg("-hls_segment_filename")
                .arg(site.join(segment))
                .arg(site.join(playlist))
                .status()
                .unwrap();
            assert!(status.success());
            if let Some(tag) = source.playlist_tag {
                let path = site.join(playlist);
                let body = fs::read_to_string(&path).unwrap();
                fs::write(
                    &path,
                    body.replacen("#EXTM3U\n", &format!("#EXTM3U\n{tag}\n"), 1),
                )
                .unwrap();
            }
        };
        encode(
            &format!(
                "testsrc=size=320x180:rate=25:duration={}",
                source.video_seconds
            ),
            &["-c:v", "libopenh264", "-g", "50"],
            "video.m3u8",
            "v%d.ts",
        );
        encode(
            &format!(
                "sine=frequency=440:sample_rate=48000:duration={}",
                source.audio_seconds
            ),
            &["-c:a", "aac", "-b:a", "64k"],
            "audio.m3u8",
            "a%d.ts",
        );
        fs::write(
            site.join("master.m3u8"),
            "#EXTM3U\n#EXT-X-VERSION:3\n\
             #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"Turkish\",LANGUAGE=\"tr\",DEFAULT=YES,AUTOSELECT=YES,URI=\"audio.m3u8\"\n\
             #EXT-X-STREAM-INF:BANDWIDTH=300000,RESOLUTION=320x180,CODECS=\"avc1.42c00d,mp4a.40.2\",AUDIO=\"aud\"\n\
             video.m3u8\n",
        )
        .unwrap();
        let server = Arc::new(Server {
            site,
            requests: Mutex::new(Vec::new()),
            failing: source.failing,
            segment_delay: source.segment_delay,
        });
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let master = format!("http://{}/master.m3u8", listener.local_addr().unwrap());
        let serving = Arc::clone(&server);
        thread::spawn(move || {
            for stream in listener.incoming().map_while(Result::ok) {
                let server = Arc::clone(&serving);
                thread::spawn(move || server.serve(stream));
            }
        });
        let folder = if source.deep_folder {
            let padding = 164_usize.saturating_sub(root.as_os_str().len() + 1).max(1);
            root.join("d".repeat(padding))
        } else {
            root.clone()
        };
        fs::create_dir_all(&folder).unwrap();
        Self {
            root,
            folder,
            paths,
            master,
            server,
        }
    }

    fn download(&self, control: &TransferControl, connections: u8) -> Result<PathBuf> {
        let request = AddRequest {
            url: self.master.clone(),
            kind: DownloadKind::Video,
            container: Some("mp4".into()),
            expected_audio: Some(true),
            connections: Some(connections),
            ..AddRequest::default()
        };
        let output = self.folder.join("film.mp4");
        download(
            &self.paths,
            &request,
            &output,
            control,
            &NetworkGovernor::new(16),
            &mut |_| {},
            "hls-end-to-end",
        )?;
        Ok(output)
    }

    /// How often each segment the encoder wrote for `prefix` was requested, in playlist order.
    fn requests(&self, prefix: &str) -> Vec<usize> {
        let requests = self.server.requests.lock().unwrap();
        let segments = fs::read_dir(&self.server.site)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .filter(|name| name.starts_with(prefix) && name.ends_with(".ts"))
            .count();
        assert!(segments > 0);
        (0..segments)
            .map(|index| {
                let name = format!("{prefix}{index}.ts");
                requests.iter().filter(|served| **served == name).count()
            })
            .collect()
    }

    /// Positions in the request order of the first and last request for a `prefix` segment.
    fn span(&self, prefix: &str) -> (usize, usize) {
        let requests = self.server.requests.lock().unwrap();
        let positions = requests
            .iter()
            .enumerate()
            .filter(|(_, name)| name.starts_with(prefix) && name.ends_with(".ts"))
            .map(|(position, _)| position)
            .collect::<Vec<_>>();
        (positions[0], positions[positions.len() - 1])
    }

    fn stream_seconds(&self, output: &Path, kind: &str) -> f64 {
        let tools = tools::verified_toolchain(&self.paths, &TransferControl::default()).unwrap();
        let probe = std::process::Command::new(&tools.ffprobe)
            .args(["-v", "error", "-show_entries", "stream=codec_type,duration"])
            .args(["-of", "json"])
            .arg(output)
            .output()
            .unwrap();
        let value: Value = serde_json::from_slice(&probe.stdout).unwrap();
        let streams = value["streams"].as_array().unwrap();
        assert_eq!(streams.len(), 2, "{value}");
        let stream = streams.iter().find(|stream| stream["codec_type"] == kind);
        stream.unwrap()["duration"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap()
    }
}

impl Server {
    fn serve(&self, stream: TcpStream) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut writer = stream;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            let name = line
                .split_whitespace()
                .nth(1)
                .and_then(|target| target.rsplit('/').next())
                .unwrap_or_default()
                .to_owned();
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).unwrap_or(0) == 0 {
                    return;
                }
                if header == "\r\n" {
                    break;
                }
            }
            let count = {
                let mut requests = self.requests.lock().unwrap();
                requests.push(name.clone());
                requests.iter().filter(|served| **served == name).count()
            };
            if name.ends_with(".ts") {
                thread::sleep(self.segment_delay);
            }
            let refused = self
                .failing
                .is_some_and(|(file, times)| file == name && count <= times);
            let body = (!refused)
                .then(|| fs::read(self.site.join(&name)).ok())
                .flatten();
            let reply = match &body {
                Some(body) => format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()),
                None => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".into(),
            };
            if writer.write_all(reply.as_bytes()).is_err()
                || body.is_some_and(|body| writer.write_all(&body).is_err())
            {
                return;
            }
        }
    }
}

#[test]
#[ignore = "needs the installed media tools; run explicitly"]
fn a_source_with_a_longer_audio_playlist_downloads_both_streams_at_once() {
    // The audio playlist runs 8 s past the video's, as streaming sites serve it.
    let fixture = Fixture::new(Source {
        video_seconds: 12,
        audio_seconds: 20,
        segment_delay: Duration::from_millis(100),
        ..Source::default()
    });
    let output = fixture
        .download(&TransferControl::default(), 16)
        .expect("the download");

    assert!((fixture.stream_seconds(&output, "video") - 12.0).abs() < 0.5);
    assert!((fixture.stream_seconds(&output, "audio") - 20.0).abs() < 0.5);
    assert!(fixture.requests("v").iter().all(|count| *count == 1));
    assert!(fixture.requests("a").iter().all(|count| *count == 1));
    // The audio stream starts before the video stream ends: one pass, not one after the other.
    let (first_audio, _) = fixture.span("a");
    let (_, last_video) = fixture.span("v");
    assert!(first_audio < last_video, "{first_audio} {last_video}");
}

#[test]
#[ignore = "needs the installed media tools; run explicitly"]
fn a_failing_segment_is_retried_alone_without_restarting_its_stream() {
    let fixture = Fixture::new(Source {
        video_seconds: 12,
        audio_seconds: 20,
        failing: Some(("a5.ts", 3)),
        ..Source::default()
    });
    let output = fixture
        .download(&TransferControl::default(), 16)
        .expect("the download");

    assert!((fixture.stream_seconds(&output, "audio") - 20.0).abs() < 0.5);
    assert!(fixture.requests("v").iter().all(|count| *count == 1));
    let audio = fixture.requests("a");
    assert_eq!(audio[5], 4, "{audio:?}");
    assert!(
        audio
            .iter()
            .enumerate()
            .all(|(index, count)| index == 5 || *count == 1),
        "{audio:?}"
    );
}

#[test]
#[ignore = "needs the installed media tools; run explicitly"]
fn a_paused_download_resumes_after_the_segments_it_already_has() {
    let fixture = Fixture::new(Source {
        video_seconds: 40,
        audio_seconds: 40,
        segment_delay: Duration::from_millis(300),
        ..Source::default()
    });
    let control = TransferControl::default();
    let pausing = control.clone();
    let server = Arc::clone(&fixture.server);
    thread::spawn(move || {
        while server
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|name| name.ends_with(".ts"))
            .count()
            < 12
        {
            thread::sleep(Duration::from_millis(20));
        }
        pausing.pause();
    });
    // Two connections per host: four segments at a time, so most are finished at the pause.
    let paused = fixture.download(&control, 2).unwrap_err();
    assert!(format!("{paused:#}").contains("SSD-TRF-009"), "{paused:#}");
    let before =
        fixture.requests("v").iter().sum::<usize>() + fixture.requests("a").iter().sum::<usize>();

    let output = fixture
        .download(&TransferControl::default(), 2)
        .expect("the resumed download");

    assert!((fixture.stream_seconds(&output, "video") - 40.0).abs() < 0.5);
    assert!((fixture.stream_seconds(&output, "audio") - 40.0).abs() < 0.5);
    let video = fixture.requests("v");
    let audio = fixture.requests("a");
    // Only the four requests the pause cut short are made again.
    let repeated = video
        .iter()
        .chain(&audio)
        .filter(|count| **count > 1)
        .count();
    assert!(before >= 12, "{before}");
    assert!(repeated <= 4, "{video:?} {audio:?}");
    assert!(video
        .iter()
        .chain(&audio)
        .all(|count| (1..=2).contains(count)));
}

#[test]
#[ignore = "needs the installed media tools and waits out yt-dlp's fragment retries (~5 min)"]
fn a_playlist_the_app_does_not_download_itself_keeps_its_finished_video_through_yt_dlp() {
    // A tag the native downloader does not know leaves the source to yt-dlp. yt-dlp tries a
    // fragment 21 times (1 + `--fragment-retries 20`) before it gives the stream up, so the
    // first attempt loses `a5.ts` and the restart gets it.
    let fixture = Fixture::new(Source {
        video_seconds: 12,
        audio_seconds: 20,
        failing: Some(("a5.ts", 21)),
        playlist_tag: Some("#EXT-X-DATERANGE:ID=\"intro\",START-DATE=\"2026-01-01T00:00:00Z\""),
        ..Source::default()
    });
    let output = fixture
        .download(&TransferControl::default(), 16)
        .expect("the download");

    assert!((fixture.stream_seconds(&output, "audio") - 20.0).abs() < 0.5);
    // The video stream finished before the loss and is not downloaded again.
    assert!(fixture.requests("v").iter().all(|count| *count == 1));
    let audio = fixture.requests("a");
    assert!(audio[5] > 21, "{audio:?}");
    assert!(
        audio
            .iter()
            .enumerate()
            .all(|(index, count)| index == 5 || (1..=2).contains(count)),
        "{audio:?}"
    );
    // yt-dlp downloads the formats one after the other.
    let (first_audio, _) = fixture.span("a");
    let (_, last_video) = fixture.span("v");
    assert!(last_video < first_audio, "{first_audio} {last_video}");
}

#[test]
#[ignore = "needs the installed media tools; run explicitly"]
fn a_download_started_from_the_picker_does_not_analyze_the_source_again() {
    let fixture = Fixture::new(Source {
        video_seconds: 6,
        audio_seconds: 6,
        ..Source::default()
    });
    let analysis = InspectRequest {
        url: fixture.master.clone(),
        ..InspectRequest::default()
    };
    inspect_controlled(
        &fixture.paths,
        &analysis,
        &TransferControl::default(),
        &NetworkGovernor::new(16),
    )
    .expect("the picker's analysis");
    let masters = || {
        let requests = fixture.server.requests.lock().unwrap();
        requests
            .iter()
            .filter(|name| *name == "master.m3u8")
            .count()
    };
    let analyzed = masters();

    fixture
        .download(&TransferControl::default(), 16)
        .expect("the download");

    assert_eq!(masters(), analyzed);
}

/// An encoder that writes its last frame twice with one timestamp leaves a file that decodes in
/// full; a damaged frame still fails verification.
#[test]
#[ignore = "needs the installed media tools; run explicitly"]
fn a_repeated_last_frame_timestamp_passes_verification_but_a_damaged_frame_does_not() {
    let root = std::env::temp_dir().join(format!("ssdownload-verify-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    let tools = tools::verified_toolchain(&shared_profile(), &TransferControl::default())
        .expect("the installed media tools");
    let ffmpeg = |args: &[&str]| {
        let status = std::process::Command::new(&tools.ffmpeg)
            .args(["-v", "error", "-y"])
            .args(args)
            .current_dir(&root)
            .status()
            .unwrap();
        assert!(status.success(), "{args:?}");
    };
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc=size=320x180:rate=25:duration=4",
        "-f",
        "lavfi",
        "-i",
        "sine=duration=4",
        "-c:v",
        "libopenh264",
        "-g",
        "50",
        "-c:a",
        "aac",
        "-shortest",
        "source.ts",
    ]);
    // The 100th and last frame takes its predecessor's presentation time; the others are
    // shifted two frames later so presentation never precedes decoding.
    ffmpeg(&[
        "-i",
        "source.ts",
        "-map",
        "0",
        "-c",
        "copy",
        "-bsf:v",
        "setts=pts='if(eq(N\\,99)\\,PREV_OUTPTS\\,PTS+7200)'",
        "repeated.ts",
    ]);
    ffmpeg(&["-i", "repeated.ts", "-c", "copy", "repeated.mp4"]);
    ffmpeg(&["-i", "source.ts", "-c", "copy", "damaged.mp4"]);
    let mut bytes = fs::read(root.join("damaged.mp4")).unwrap();
    let middle = bytes.len() / 2;
    let mut state = 0x9e37_79b9_u32;
    for byte in &mut bytes[middle..middle + 16 * 1024] {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        *byte = state as u8;
    }
    fs::write(root.join("damaged.mp4"), bytes).unwrap();
    let request = AddRequest {
        kind: DownloadKind::Video,
        container: Some("mp4".into()),
        ..AddRequest::default()
    };
    let verify = |name: &str| {
        verify_media_output(
            &root.join(name),
            &request,
            &MediaInfo::default(),
            &tools,
            &TransferControl::default(),
        )
    };

    let repeated = verify("repeated.mp4");
    let damaged = verify("damaged.mp4");
    let _ = fs::remove_dir_all(&root);

    repeated.expect("a repeated last timestamp is not a decoding error");
    let error = damaged.expect_err("a damaged frame fails verification");
    assert!(
        error
            .to_string()
            .contains(crate::i18n::ui("çözülemedi", "could not be decoded")),
        "{error}"
    );
}

/// Speech-recognition captions can hold cues that end where they start and two cues that start
/// together; they still reach the MP4 as one subtitle track, also when the request names no
/// container and the source's MP4 is kept.
#[test]
#[ignore = "needs the installed media tools; run explicitly"]
fn captions_with_empty_and_simultaneous_cues_mux_into_mp4() {
    let root = std::env::temp_dir().join(format!("ssdownload-captions-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    let tools = tools::verified_toolchain(&shared_profile(), &TransferControl::default())
        .expect("the installed media tools");
    let status = std::process::Command::new(&tools.ffmpeg)
        .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
        .arg("testsrc=size=320x180:rate=25:duration=8")
        .args([
            "-f",
            "lavfi",
            "-i",
            "sine=duration=8",
            "-c:v",
            "libopenh264",
        ])
        .args(["-c:a", "aac", "-shortest", "video.mp4"])
        .current_dir(&root)
        .status()
        .unwrap();
    assert!(status.success());
    let captions = "1\n00:00:01,50 --> 00:00:01,50\nInvisible\n\n\
                    2\n00:00:03,01 --> 00:00:03,61\nWhere\n\n\
                    3\n00:00:03,01 --> 00:00:03,61\nwhere.\n\n\
                    4\n00:00:05,00 --> 00:00:06,00\nThere\n";
    fs::write(root.join("served.srt"), captions).unwrap();
    fs::write(root.join("captions.srt"), captions).unwrap();
    let request = AddRequest {
        kind: DownloadKind::Video,
        container: Some("mp4".into()),
        external_subtitles: vec![ExternalSubtitle {
            language: "en".into(),
            label: "English".into(),
            ..ExternalSubtitle::default()
        }],
        ..AddRequest::default()
    };
    let kept_request = AddRequest {
        container: None,
        ..request.clone()
    };
    let mux = |request: &AddRequest, subtitle: &str, output: &str| {
        let args = selected_output_args(
            &root.join("video.mp4"),
            &[root.join(subtitle)],
            &root.join(output),
            request,
            1,
        );
        std::process::Command::new(&tools.ffmpeg)
            .args(args)
            .output()
            .unwrap()
    };
    let verify = |request: &AddRequest, output: &str| {
        verify_media_output(
            &root.join(output),
            request,
            &MediaInfo::default(),
            &tools,
            &TransferControl::default(),
        )
    };

    let served = mux(&request, "served.srt", "served.mp4");
    let checked = validate_external_subtitle_file(
        &root.join("captions.srt"),
        &request.external_subtitles[0],
        Some(8.0),
        None,
    );
    let muxed = mux(&request, "captions.srt", "captions.mp4");
    let verified = verify(&request, "captions.mp4");
    // A request without a container keeps the source's MP4.
    let kept = mux(&kept_request, "captions.srt", "kept.mp4");
    let kept_verified = verify(&kept_request, "kept.mp4");
    let text = std::process::Command::new(&tools.ffmpeg)
        .args(["-v", "error", "-i"])
        .arg(root.join("captions.mp4"))
        .args(["-map", "0:s", "-f", "srt", "-"])
        .output()
        .unwrap();
    let _ = fs::remove_dir_all(&root);

    assert!(
        !served.status.success(),
        "the captions as served cannot be stored in MP4"
    );
    checked.expect("the captions pass validation");
    assert!(
        muxed.status.success() && muxed.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&muxed.stderr)
    );
    let text = String::from_utf8_lossy(&text.stdout).replace("\r\n", "\n");
    assert!(text.contains("Where\nwhere."), "{text}");
    assert!(text.contains("There"), "{text}");
    assert!(!text.contains("Invisible"), "{text}");
    verified.expect("the MP4 with the captions passes verification");
    assert!(
        kept.status.success() && kept.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&kept.stderr)
    );
    kept_verified.expect("the kept MP4 with the captions passes verification");
}

/// The browser transfer writes the segments of an HLS variant one after the other. A variant that
/// holds the video and the audio together, sent as an `audio` stream for an audio-only request,
/// gives an output with the audio alone that passes the verification, for both segment kinds; sent
/// as a `video` or `file` stream the same request is refused.
#[test]
#[ignore = "needs the installed media tools; run explicitly"]
fn the_audio_of_a_combined_stream_is_muxed_and_verified_as_an_audio_only_output() {
    let paths = shared_profile();
    let tools = tools::verified_toolchain(&paths, &TransferControl::default())
        .expect("the installed media tools");
    for (label, fragmented) in [("MPEG-TS segments", false), ("fMP4 segments", true)] {
        let root =
            std::env::temp_dir().join(format!("ssdownload-combined-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let mut encode = std::process::Command::new(&tools.ffmpeg);
        encode
            .args(["-v", "error", "-f", "lavfi", "-i"])
            .arg("testsrc=size=320x180:rate=25:duration=4")
            .args(["-f", "lavfi", "-i"])
            .arg("sine=frequency=440:sample_rate=48000:duration=4")
            .args([
                "-c:v",
                "libopenh264",
                "-g",
                "50",
                "-c:a",
                "aac",
                "-b:a",
                "64k",
            ])
            .args(["-f", "hls", "-hls_time", "2", "-hls_playlist_type", "vod"]);
        if fragmented {
            encode
                .args(["-hls_segment_type", "fmp4", "-hls_fmp4_init_filename"])
                .arg(root.join("init.mp4"));
        }
        let extension = if fragmented { "m4s" } else { "ts" };
        encode
            .arg("-hls_segment_filename")
            .arg(root.join(format!("seg%d.{extension}")))
            .arg(root.join("variant.m3u8"));
        assert!(encode.status().unwrap().success(), "{label}: encoding");

        let mut bytes = if fragmented {
            fs::read(root.join("init.mp4")).unwrap()
        } else {
            Vec::new()
        };
        let mut segments = 0;
        while let Ok(segment) = fs::read(root.join(format!("seg{segments}.{extension}"))) {
            bytes.extend(segment);
            segments += 1;
        }
        assert!(segments >= 2, "{label}: {segments} segments");
        let combined = root.join("stream-1");
        fs::write(&combined, bytes).unwrap();

        let request = AddRequest {
            url: "https://site.example/master.m3u8".into(),
            kind: DownloadKind::Audio,
            audio_format_id: Some("hls-300".into()),
            expected_audio: Some(true),
            ..AddRequest::default()
        };
        let stream = |role: &str| crate::browser_transfer::StreamFile {
            path: combined.clone(),
            role: role.into(),
            format_id: Some("hls-300".into()),
            language: None,
        };
        let streams = [stream("audio")];
        let output = root.join("audio.mkv");
        let control = TransferControl::default();
        mux_browser_streams(&paths, &request, &streams, &output, &control)
            .unwrap_or_else(|error| panic!("{label}: {error:#}"));
        verify_external_result(&paths, &request, &output, &streams, Some(4.0), true)
            .unwrap_or_else(|error| panic!("{label}: {error:#}"));
        for (role, refusal) in [
            (
                "video",
                crate::i18n::ui(
                    "Tarayıcı yalnız ses aktarımında görüntü akışı gönderdi",
                    "The browser sent a video stream in an audio-only transfer",
                ),
            ),
            (
                "file",
                crate::i18n::ui(
                    "Tarayıcı bilinmeyen bir medya akışı rolü gönderdi",
                    "The browser sent an unknown media stream role",
                ),
            ),
        ] {
            let refused = root.join(format!("{role}.mkv"));
            let error = mux_browser_streams(&paths, &request, &[stream(role)], &refused, &control)
                .err()
                .unwrap_or_else(|| panic!("{label}: the same stream sent as {role} is accepted"));
            assert_eq!(error.to_string(), refusal, "{label}: {role}");
        }

        let probe = std::process::Command::new(&tools.ffprobe)
            .args(["-v", "error", "-show_entries", "stream=codec_type"])
            .args(["-of", "json"])
            .arg(&output)
            .output()
            .unwrap();
        let value: Value = serde_json::from_slice(&probe.stdout).unwrap();
        let kinds = value["streams"]
            .as_array()
            .unwrap()
            .iter()
            .map(|stream| stream["codec_type"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        let _ = fs::remove_dir_all(&root);
        assert_eq!(
            kinds,
            ["audio"],
            "{label}: the output holds the audio alone"
        );
    }
}

/// The download folder is the user's choice and holds the media workspace, whose files then pass
/// the 259 characters of an unprefixed Windows path. Both downloaders still finish with an MP4.
#[test]
#[ignore = "needs the installed media tools; run explicitly"]
fn a_download_into_a_deep_folder_completes_as_an_mp4() {
    // A tag the app's own downloader does not know leaves the source to yt-dlp.
    let unknown_tag = "#EXT-X-DATERANGE:ID=\"intro\",START-DATE=\"2026-01-01T00:00:00Z\"";
    for (label, playlist_tag) in [
        ("the app's own downloader", None),
        ("yt-dlp", Some(unknown_tag)),
    ] {
        let fixture = Fixture::new(Source {
            video_seconds: 6,
            audio_seconds: 6,
            playlist_tag,
            deep_folder: true,
            ..Source::default()
        });
        let output = fixture
            .download(&TransferControl::default(), 4)
            .unwrap_or_else(|error| panic!("{label}: {error:#}"));

        let owner = crate::output::identity(&output).unwrap();
        let part_file = workspace_path(&output, &owner).join("media.fhls-300.mp4.part");
        assert!(
            part_file.as_os_str().len() > 259,
            "{label}: the work files must pass MAX_PATH: {part_file:?}"
        );
        let tools = tools::verified_toolchain(&fixture.paths, &TransferControl::default()).unwrap();
        let container = container_of(&tools, &output);
        assert!(
            container.split(',').any(|name| name == "mp4"),
            "{label}: {container}"
        );
        assert!((fixture.stream_seconds(&output, "video") - 6.0).abs() < 0.5);
        assert!((fixture.stream_seconds(&output, "audio") - 6.0).abs() < 0.5);
    }
}

/// ffprobe's name for the container of `file`.
fn container_of(tools: &tools::VerifiedTools, file: &Path) -> String {
    let probe = std::process::Command::new(&tools.ffprobe)
        .args(["-v", "error", "-show_entries", "format=format_name"])
        .args(["-of", "default=nw=1:nk=1"])
        .arg(file)
        .output()
        .unwrap();
    String::from_utf8_lossy(&probe.stdout).trim().to_owned()
}

/// yt-dlp that cannot start FFmpeg warns, keeps the MPEG-TS it downloaded under an `.mp4` name
/// and exits successfully. Such a file, or any file whose container is not the one its extension
/// names, fails verification; the real containers behind the same extensions pass.
#[test]
#[ignore = "needs the installed media tools; run explicitly"]
fn a_file_in_another_container_than_its_extension_names_fails_verification() {
    let root = std::env::temp_dir().join(format!("ssdownload-container-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    let tools = tools::verified_toolchain(&shared_profile(), &TransferControl::default())
        .expect("the installed media tools");
    let ffmpeg = |args: &[&str]| {
        let output = std::process::Command::new(&tools.ffmpeg)
            .args(["-v", "error", "-y"])
            .args(args)
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(output.status.success(), "{args:?}");
    };
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc=size=320x180:rate=25:duration=4",
        "-f",
        "lavfi",
        "-i",
        "sine=duration=4",
        "-c:v",
        "libopenh264",
        "-g",
        "50",
        "-c:a",
        "aac",
        "-shortest",
        "source.mkv",
    ]);
    // (file, muxer forced for the copy, whether the extension names that container)
    let copies = [
        ("right.mp4", "mp4", true),
        ("right.mov", "mov", true),
        ("right.mkv", "matroska", true),
        ("wrong.mp4", "mpegts", false),
        ("wrong.mov", "matroska", false),
        ("wrong.mkv", "mp4", false),
    ];
    for (file, muxer, _) in copies {
        ffmpeg(&["-i", "source.mkv", "-c", "copy", "-f", muxer, file]);
    }
    let audio_copies = [("right.m4a", "ipod", true), ("wrong.m4a", "mpegts", false)];
    for (file, muxer, _) in audio_copies {
        ffmpeg(&["-i", "source.mkv", "-vn", "-c", "copy", "-f", muxer, file]);
    }
    let verify = |file: &str, kind: DownloadKind| {
        let request = AddRequest {
            kind,
            ..AddRequest::default()
        };
        verify_media_output(
            &root.join(file),
            &request,
            &MediaInfo::default(),
            &tools,
            &TransferControl::default(),
        )
    };

    let outcomes = copies
        .iter()
        .map(|(file, _, right)| (file, right, verify(file, DownloadKind::Video)))
        .chain(
            audio_copies
                .iter()
                .map(|(file, _, right)| (file, right, verify(file, DownloadKind::Audio))),
        )
        .collect::<Vec<_>>();
    let _ = fs::remove_dir_all(&root);

    for (file, right, outcome) in outcomes {
        if *right {
            outcome.unwrap_or_else(|error| panic!("{file}: {error:#}"));
        } else {
            let error = outcome.expect_err(file);
            let extension = file.rsplit('.').next().unwrap();
            assert!(
                error.to_string().contains(&format!(".{extension}")),
                "{file}: {error}"
            );
        }
    }
}

/// The browser relays a muxed HLS playlist as one file of MPEG-TS segments, and a packed-audio
/// playlist as raw ADTS, whatever container the job's name promises. A media job gets a copy of
/// the file in the container its name asks for, also when the segments were muxed one by one; a
/// file already in it and a raw file job are moved as they are.
#[test]
#[ignore = "needs the installed media tools; run explicitly"]
fn a_relayed_file_is_copied_into_the_container_its_name_promises() {
    let paths = shared_profile();
    let tools = tools::verified_toolchain(&paths, &TransferControl::default())
        .expect("the installed media tools");
    let root = std::env::temp_dir().join(format!("ssdownload-relayed-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    let ffmpeg = |args: &[&str]| {
        let output = std::process::Command::new(&tools.ffmpeg)
            .args(["-v", "error", "-y"])
            .args(args)
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    let sources = [
        "-f",
        "lavfi",
        "-i",
        "testsrc=size=320x180:rate=25:duration=4",
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:sample_rate=48000:duration=4",
        "-c:v",
        "libopenh264",
        "-g",
        "50",
        "-c:a",
        "aac",
        "-b:a",
        "64k",
    ];
    let encode = |extra: &[&str]| ffmpeg(&[&sources[..], extra].concat());
    encode(&[
        "-f",
        "hls",
        "-hls_time",
        "2",
        "-hls_playlist_type",
        "vod",
        "-hls_segment_filename",
        "seg%d.ts",
        "variant.m3u8",
    ]);
    let mut transport = Vec::new();
    let mut segments = 0;
    while let Ok(segment) = fs::read(root.join(format!("seg{segments}.ts"))) {
        transport.extend(segment);
        segments += 1;
    }
    assert!(segments >= 2, "{segments} segments");
    // Segments muxed one by one restart the continuity counters of the transport stream, and
    // FFmpeg flags the packets at the join as corrupt. The offset undoes the muxer's start delay
    // of 1.4 s, so the timestamps stay continuous.
    ffmpeg(&[
        "-copyts",
        "-i",
        "seg1.ts",
        "-c",
        "copy",
        "-output_ts_offset",
        "-1.4",
        "-f",
        "mpegts",
        "seg1-restarted.ts",
    ]);
    let mut restarted = fs::read(root.join("seg0.ts")).unwrap();
    restarted.extend(fs::read(root.join("seg1-restarted.ts")).unwrap());
    encode(&["-f", "mp4", "right.mp4"]);
    let in_mp4 = fs::read(root.join("right.mp4")).unwrap();
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:sample_rate=48000:duration=4",
        "-c:a",
        "aac",
        "-f",
        "adts",
        "packed.aac",
    ]);
    let packed = fs::read(root.join("packed.aac")).unwrap();

    // (job name, relayed bytes, kind, container of the request, ffprobe's name for the result)
    let jobs = [
        ("a.mp4", &transport, DownloadKind::Video, Some("mp4"), "mp4"),
        (
            "b.mkv",
            &transport,
            DownloadKind::Video,
            Some("mkv"),
            "matroska",
        ),
        ("c.mp4", &in_mp4, DownloadKind::Video, Some("mp4"), "mp4"),
        ("d.m4a", &packed, DownloadKind::Audio, None, "mp4"),
        ("e.mp4", &transport, DownloadKind::File, None, "mpegts"),
        ("f.mp4", &restarted, DownloadKind::Video, Some("mp4"), "mp4"),
    ];
    let control = TransferControl::default();
    let mut failures = Vec::new();
    for (name, bytes, kind, container, expected) in jobs {
        let source = root.join(format!("stream-{name}"));
        fs::write(&source, bytes).unwrap();
        let request = AddRequest {
            url: "https://site.example/master.m3u8".into(),
            kind,
            container: container.map(str::to_owned),
            ..AddRequest::default()
        };
        let output = root.join(name);
        if let Err(error) = place_browser_file(&paths, &request, &source, &output, &control) {
            failures.push(format!("{name}: {error:#}"));
            continue;
        }
        if source.exists() {
            failures.push(format!("{name}: the relayed file is still in the transfer"));
        }
        let container = container_of(&tools, &output);
        if !container.split(',').any(|format| format == expected) {
            failures.push(format!("{name}: holds {container}, not {expected}"));
        }
        if kind == DownloadKind::File {
            if fs::read(&output).unwrap() != *bytes {
                failures.push(format!("{name}: a raw file job changed the bytes"));
            }
        } else if let Err(error) =
            verify_external_result(&paths, &request, &output, &[], Some(4.0), true)
        {
            failures.push(format!("{name}: {error:#}"));
        }
        if name == "c.mp4" && fs::read(&output).unwrap() != *bytes {
            failures.push(format!("{name}: a file in the right container changed"));
        }
    }
    let _ = fs::remove_dir_all(&root);
    assert!(failures.is_empty(), "{failures:#?}");
}

/// yt-dlp starts FFmpeg, FFprobe and Deno from the canonical paths the app gives it, and finds
/// each of them exactly where the path, verbatim prefix included, is at most 259 UTF-16 units:
/// the check refuses the paths yt-dlp cannot use and no others.
#[test]
#[ignore = "needs the installed media tools; run explicitly"]
fn yt_dlp_finds_the_tools_exactly_where_the_check_lets_them_start() {
    let tools = tools::verified_toolchain(&shared_profile(), &TransferControl::default())
        .expect("the installed media tools");
    let root = std::env::temp_dir().join(format!("ssdownload-reach-{}", uuid::Uuid::new_v4()));
    let units = |path: &Path| path.to_string_lossy().encode_utf16().count();
    // A folder under `root` in which a file called `name` has `target` units, the prefix counted.
    let folder = |name: &str, target: usize| {
        let padding = target - units(Path::new(r"\\?\")) - units(&root) - 2 - name.len();
        let folder = PathBuf::from(format!(r"\\?\{}\{}", root.display(), "p".repeat(padding)));
        fs::create_dir_all(&folder).unwrap();
        folder
    };
    let link = |from: &Path, folder: &Path| {
        let to = folder.join(from.file_name().unwrap());
        fs::hard_link(from, &to)
            .or_else(|_| fs::copy(from, &to).map(|_| ()))
            .unwrap_or_else(|error| panic!("{from:?} to {to:?}: {error}"));
        to
    };
    let bin = tools.ffmpeg.parent().unwrap();
    let ffmpeg_in = |ffprobe_units: usize| {
        let folder = folder("ffprobe.exe", ffprobe_units);
        for entry in fs::read_dir(bin).unwrap() {
            link(&entry.unwrap().path(), &folder);
        }
        folder
    };
    let reach = |ffmpeg_folder: &Path, deno: &Path| {
        let output = std::process::Command::new(&tools.yt_dlp)
            .args(["-v", "--ignore-config", "--ffmpeg-location"])
            .arg(ffmpeg_folder)
            .arg("--js-runtimes")
            .arg(format!("deno:{}", deno.display()))
            .output()
            .unwrap();
        let report = String::from_utf8_lossy(&[output.stdout, output.stderr].concat()).into_owned();
        // yt-dlp leaves the line out when it found none of the tools.
        let line = |marker: &str| {
            report
                .lines()
                .find(|line| line.contains(marker))
                .unwrap_or_default()
                .to_owned()
        };
        (line("exe versions:"), line("JS runtimes:"))
    };

    let deno_at = |target: usize| link(&tools.deno, &folder("deno.exe", target));
    let mut reached = Vec::new();
    let mut ffmpeg_folders = Vec::new();
    for ffprobe_units in [259, 260, 261] {
        let ffmpeg_folder = ffmpeg_in(ffprobe_units);
        let (tools_line, _) = reach(&ffmpeg_folder, &tools.deno);
        let ffmpeg = ffmpeg_folder.join("ffmpeg.exe");
        let ffprobe = ffmpeg_folder.join("ffprobe.exe");
        assert_eq!(units(&ffprobe), ffprobe_units);
        let allowed = ensure_helpers_startable(&tools.deno, &ffmpeg, &ffprobe).is_ok();
        reached.push((
            ffprobe_units,
            tools_line.contains("ffmpeg"),
            tools_line.contains("ffprobe"),
            allowed,
        ));
        ffmpeg_folders.push(ffmpeg_folder);
    }
    for deno_units in [259, 260] {
        let deno = deno_at(deno_units);
        let (tools_line, runtimes) = reach(&ffmpeg_folders[0], &deno);
        let allowed = ensure_helpers_startable(&deno, &tools.ffmpeg, &tools.ffprobe).is_ok();
        reached.push((
            deno_units,
            tools_line.contains("ffmpeg"),
            runtimes.contains("deno"),
            allowed,
        ));
    }
    let _ = fs::remove_dir_all(&root);

    // (length of the file, ffmpeg found, the other tool found, the check lets the job start)
    assert_eq!(
        reached,
        [
            (259, true, true, true),
            (260, true, false, false),
            (261, false, false, false),
            (259, true, true, true),
            (260, true, false, false),
        ]
    );
}
