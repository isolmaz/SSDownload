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
}

struct Server {
    site: PathBuf,
    requests: Mutex<Vec<String>>,
    failing: Option<(&'static str, usize)>,
    segment_delay: Duration,
}

struct Fixture {
    root: PathBuf,
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
        Self {
            root,
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
        let output = self.root.join("film.mp4");
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
