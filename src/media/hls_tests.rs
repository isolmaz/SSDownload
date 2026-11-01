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

/// The check a downloaded tool set passes before it replaces the active one also passes with the
/// installed tools, and takes seconds.
#[test]
#[ignore = "needs the installed media tools; run explicitly"]
fn the_installed_tools_pass_the_self_test() {
    let tools = tools::verified_toolchain(&shared_profile(), &TransferControl::default())
        .expect("the installed media tools");
    let scratch =
        std::env::temp_dir().join(format!("ssdownload-selftest-{}", uuid::Uuid::new_v4()));
    let started = Instant::now();
    let result = self_test(&tools, &scratch, &TransferControl::default());
    let elapsed = started.elapsed();
    let _ = fs::remove_dir_all(&scratch);

    result.expect("the self-test of the installed tools");
    assert!(elapsed < Duration::from_secs(30), "{elapsed:?}");
}

/// A tool set that has just been through the self-test, whether it passed or failed, and its
/// scratch folder can be deleted at once: none of the check's processes keeps a file open.
#[test]
#[ignore = "needs the installed media tools; run explicitly"]
fn a_tool_set_can_be_deleted_right_after_its_self_test() {
    let installed = tools::verified_toolchain(&shared_profile(), &TransferControl::default())
        .expect("the installed media tools");
    let ffmpeg_dir = installed.ffmpeg.parent().unwrap();

    // The second set has a tool that cannot do yt-dlp's work, so its self-test fails.
    for (broken, yt_dlp) in [(false, &installed.yt_dlp), (true, &installed.ffprobe)] {
        let root = std::env::temp_dir().join(format!("ssdownload-staged-{}", uuid::Uuid::new_v4()));
        let scratch = root.with_file_name(format!(
            "{}-selftest",
            root.file_name().unwrap().to_string_lossy()
        ));
        let stage = |source: &Path, target: PathBuf| {
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            fs::copy(source, &target).unwrap();
            target
        };
        let staged = tools::VerifiedTools::staged(
            stage(yt_dlp, root.join("yt-dlp").join("yt-dlp.exe")),
            stage(&installed.deno, root.join("deno").join("deno.exe")),
            stage(
                &installed.ffmpeg,
                root.join("ffmpeg").join("bin").join("ffmpeg.exe"),
            ),
            stage(
                &installed.ffprobe,
                root.join("ffmpeg").join("bin").join("ffprobe.exe"),
            ),
        );
        for entry in fs::read_dir(ffmpeg_dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|extension| extension == "dll") {
                stage(
                    &path,
                    root.join("ffmpeg")
                        .join("bin")
                        .join(path.file_name().unwrap()),
                );
            }
        }

        let result = self_test(&staged, &scratch, &TransferControl::default());
        assert_eq!(result.is_err(), broken, "{result:?}");
        let removed = fs::remove_dir_all(&root).and(fs::remove_dir_all(&scratch));

        removed.expect("the folders of the checked set could not be deleted");
        assert!(!root.exists() && !scratch.exists());
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

fn run_ffmpeg(tools: &tools::VerifiedTools, root: &Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new(&tools.ffmpeg)
        .args(["-v", "error", "-y", "-nostdin"])
        .args(args)
        .current_dir(root)
        .output()
        .unwrap()
}

/// A 10 s H.264 video and a 10 s AAC track from the real FFmpeg. The track's ADTS frames are
/// changed before they are muxed into an MP4, so a test decides what the audio looks like.
struct EndFixture {
    root: PathBuf,
    tools: Arc<tools::VerifiedTools>,
    audio: Vec<u8>,
    /// Byte offset of each ADTS frame, then the length of the track.
    frames: Vec<usize>,
}

impl EndFixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("ssdownload-end-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let tools = tools::verified_toolchain(&shared_profile(), &TransferControl::default())
            .expect("the installed media tools");
        let encodes: [&[&str]; 2] = [
            &[
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=320x180:rate=25:duration=10",
                "-c:v",
                "libopenh264",
                "-g",
                "50",
                "video.mp4",
            ],
            &[
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=48000:duration=10",
                "-c:a",
                "aac",
                "-b:a",
                "64k",
                "audio.aac",
            ],
        ];
        for args in encodes {
            assert!(run_ffmpeg(&tools, &root, args).status.success(), "{args:?}");
        }
        let audio = fs::read(root.join("audio.aac")).unwrap();
        let mut frames = Vec::new();
        let mut at = 0;
        while at + 7 <= audio.len() {
            frames.push(at);
            // The 13-bit frame length spans bytes 3 to 5 of the ADTS header.
            at += (usize::from(audio[at + 3] & 3) << 11)
                | (usize::from(audio[at + 4]) << 3)
                | (usize::from(audio[at + 5]) >> 5);
        }
        frames.push(audio.len());
        Self {
            root,
            tools,
            audio,
            frames,
        }
    }

    fn last_frame(&self) -> usize {
        self.frames.len() - 2
    }

    /// The track up to the first 33 payload bytes of frame `index`, as a download that ends
    /// inside that frame has it.
    fn ending_inside(&self, index: usize) -> Vec<u8> {
        self.audio[..self.frames[index] + 40].to_vec()
    }

    /// Replaces the payload of `frames` with noise. Their headers stay, so the track still parses.
    fn add_noise(&self, audio: &mut [u8], frames: std::ops::Range<usize>) {
        let mut state = 0x9e37_79b9_u32;
        for index in frames {
            for byte in &mut audio[self.frames[index] + 7..self.frames[index + 1]] {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                *byte = state as u8;
            }
        }
    }

    /// The video muxed with `audio` in place of the generated track, as `file` (its extension
    /// picks the container).
    fn mux(&self, file: &str, audio: &[u8], extra: &[&str]) -> PathBuf {
        let input = format!("{file}.aac");
        fs::write(self.root.join(&input), audio).unwrap();
        let mut args = vec![
            "-i",
            "video.mp4",
            "-i",
            &input,
            "-map",
            "0:v",
            "-map",
            "1:a",
            "-c",
            "copy",
        ];
        args.extend_from_slice(extra);
        args.push(file);
        let muxed = run_ffmpeg(&self.tools, &self.root, &args);
        assert!(
            muxed.status.success(),
            "{}",
            String::from_utf8_lossy(&muxed.stderr)
        );
        self.root.join(file)
    }

    fn verify(&self, file: &Path, full: bool, info: &MediaInfo) -> Result<()> {
        let request = AddRequest {
            kind: DownloadKind::Video,
            container: Some("mp4".into()),
            full_verification: full,
            ..AddRequest::default()
        };
        self.verify_request(file, &request, info)
    }

    fn verify_request(&self, file: &Path, request: &AddRequest, info: &MediaInfo) -> Result<()> {
        verify_media_output(
            file,
            request,
            info,
            &self.tools,
            &TransferControl::default(),
        )
    }

    /// The source as the extractor describes it: `seconds` long, with the tolerance an HLS
    /// source gets for two-second segments.
    fn source(seconds: f64, hls: bool) -> MediaInfo {
        MediaInfo {
            duration: Some(seconds),
            duration_tolerance: hls.then_some(2.25),
            ..MediaInfo::default()
        }
    }
}

impl Drop for EndFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn decode_failure(full: bool) -> &'static str {
    if full {
        crate::i18n::ui(
            "Tam medya doğrulaması: ses/görüntü baştan sona çözülemedi",
            "Full media verification: the audio/video could not be decoded from start to end",
        )
    } else {
        crate::i18n::ui(
            "Medya doğrulaması: örneklenen başlangıç/orta/bitiş bölümü çözülemedi",
            "Media verification: the sampled start/middle/end section could not be decoded",
        )
    }
}

/// A download whose last segment ends inside an AAC frame plays in full, but decoding that frame
/// reports an error. Where the source's duration confirms the file, the decoded streams stop half
/// a second before their end, so that frame is not judged: in an MP4 and in an MPEG-TS file that
/// does not start at zero.
#[test]
#[ignore = "needs the installed media tools; run explicitly"]
fn a_download_that_ends_inside_its_last_audio_frame_passes_verification() {
    let fixture = EndFixture::new();
    let cut_audio = fixture.ending_inside(fixture.last_frame());
    for extension in ["mp4", "ts"] {
        let whole = fixture.mux(&format!("whole.{extension}"), &fixture.audio, &[]);
        let cut = fixture.mux(&format!("cut.{extension}"), &cut_audio, &[]);

        let decoded = run_ffmpeg(
            &fixture.tools,
            &fixture.root,
            &[
                "-xerror",
                "-sseof",
                "-2",
                "-i",
                &format!("cut.{extension}"),
                "-map",
                "0:a",
                "-f",
                "null",
                "-",
            ],
        );
        assert!(
            !decoded.status.success(),
            "{extension}: the cut frame is a decode error to FFmpeg itself"
        );
        if extension == "ts" {
            let probe = std::process::Command::new(&fixture.tools.ffprobe)
                .args(["-v", "error", "-show_entries", "format=start_time"])
                .args(["-of", "default=nw=1:nk=1"])
                .arg(&cut)
                .output()
                .unwrap();
            let start: f64 = String::from_utf8_lossy(&probe.stdout)
                .trim()
                .parse()
                .unwrap();
            assert!(start > 1.0, "the file starts at {start} s");
        }
        for full in [false, true] {
            let source = EndFixture::source(10.04, false);
            fixture
                .verify(&whole, full, &source)
                .unwrap_or_else(|error| {
                    panic!("{extension}, the intact file, full {full}: {error:#}")
                });
            fixture.verify(&cut, full, &source).unwrap_or_else(|error| {
                panic!("{extension}, the cut frame, full {full}: {error:#}")
            });
        }
    }
}

#[test]
#[ignore = "needs the installed media tools; run explicitly"]
fn damage_in_front_of_the_last_audio_frame_still_fails_verification() {
    let fixture = EndFixture::new();
    let last = fixture.last_frame();
    // Frames 180-199 are at 3.8-4.3 s, frames 424-428 at 9.1-9.2 s, 0.8 s before the end.
    let mut middle = fixture.audio.clone();
    fixture.add_noise(&mut middle, 180..200);
    let mut tail = fixture.audio.clone();
    fixture.add_noise(&mut tail, last - 45..last - 40);
    let mut tail_and_cut = fixture.ending_inside(last);
    fixture.add_noise(&mut tail_and_cut, last - 45..last - 40);
    // The audio ends inside frame 450, at 9.6 s, with noise in frames 400-404 (8.5 s) in front.
    let mut early_end = fixture.ending_inside(450);
    fixture.add_noise(&mut early_end, 400..405);

    for (name, audio) in [
        ("middle", middle),
        ("tail", tail),
        ("tail_and_cut", tail_and_cut),
        ("early_end", early_end),
    ] {
        let file = fixture.mux(&format!("{name}.mp4"), &audio, &[]);
        for full in [false, true] {
            let error = fixture
                .verify(&file, full, &EndFixture::source(10.04, true))
                .expect_err(&format!("{name}, full {full}"));
            assert_eq!(
                error.to_string(),
                decode_failure(full),
                "{name}, full {full}"
            );
        }
    }
}

/// The audio of an HLS source can end before its video, and the source's tolerance lets that pass.
/// Where the source's length is within half a second of the audio's own end, the cut frame of
/// that audio is not judged; the same file without a confirming duration is decoded to its end
/// and fails.
#[test]
#[ignore = "needs the installed media tools; run explicitly"]
fn a_stream_that_ends_before_the_others_is_bounded_by_its_own_end() {
    let fixture = EndFixture::new();
    // Frame 436 starts at 9.3 s: the audio ends inside it, the video runs to 10 s. The source is
    // 9.6 s, so each stream ends within half a second of it, and a stop 0.5 s before the
    // video's end would reach the cut frame.
    let audio = fixture.ending_inside(436);
    for extension in ["mp4", "ts"] {
        let file = fixture.mux(&format!("early.{extension}"), &audio, &[]);
        for full in [false, true] {
            fixture
                .verify(&file, full, &EndFixture::source(9.6, true))
                .unwrap_or_else(|error| panic!("{extension}, full {full}: {error:#}"));
            let unconfirmed = MediaInfo {
                duration: None,
                ..EndFixture::source(9.6, true)
            };
            let error = fixture
                .verify(&file, full, &unconfirmed)
                .expect_err(&format!("{extension}, full {full}, no source duration"));
            assert_eq!(
                error.to_string(),
                decode_failure(full),
                "{extension}, full {full}"
            );
        }
    }
}

/// One stream cut short in a download whose other stream is complete is not a complete download,
/// although the source's tolerance accepts the file: the cut stream ends more than half a second
/// before the source's length, so it is decoded to its end and the cut frame fails.
#[test]
#[ignore = "needs the installed media tools; run explicitly"]
fn a_stream_cut_short_in_an_otherwise_complete_download_is_refused() {
    let fixture = EndFixture::new();
    // Frame 390 starts at 8.3 s: the audio ends inside it, 1.7 s before the video.
    let audio = fixture.ending_inside(390);
    for extension in ["mp4", "ts"] {
        let file = fixture.mux(&format!("cut.{extension}"), &audio, &[]);
        for full in [false, true] {
            let error = fixture
                .verify(&file, full, &EndFixture::source(10.04, true))
                .expect_err(&format!("{extension}, full {full}"));
            assert_eq!(
                error.to_string(),
                decode_failure(full),
                "{extension}, full {full}"
            );
        }
    }
}

/// An extractor may report a source of 10.9 s as 10, and an old playlist may list whole-second
/// segments, so only a length with a fractional part confirms the end of a file. The tolerance the
/// source has says how far the streams may differ from a length, not that the length is exact: a
/// file of 9.6 s that ends in a cut frame is decoded to its end against a duration of 10 s with the
/// tolerance of an HLS source, with or without playlists that declare 10.9 s or 10 s. Against a
/// duration of 10.04 s, or playlists that declare 9.64 s and 9.62 s, the same file is within half
/// a second of its source.
#[test]
#[ignore = "needs the installed media tools; run explicitly"]
fn only_a_length_with_a_fraction_confirms_the_end_of_a_file() {
    let fixture = EndFixture::new();
    // Frame 448 starts at 9.56 s: the audio ends inside it, the video where `-t` stops it.
    let file = fixture.mux("rounded.mp4", &fixture.ending_inside(448), &["-t", "9.6"]);
    let declared = |duration: f64, video: f64, audio: f64| MediaInfo {
        declared_video_durations: vec![video],
        declared_audio_durations: vec![audio],
        ..EndFixture::source(duration, true)
    };
    for full in [false, true] {
        for (label, info) in [
            ("10 s", EndFixture::source(10.0, false)),
            ("10 s and a tolerance", EndFixture::source(10.0, true)),
            ("10 s and playlists of 10.9 s", declared(10.0, 10.9, 10.9)),
            ("10 s and playlists of 10 s", declared(10.0, 10.0, 10.0)),
            (
                "10.04 s and playlists of 10.9 s",
                declared(10.04, 10.9, 10.9),
            ),
        ] {
            let error = fixture
                .verify(&file, full, &info)
                .expect_err(&format!("{label}, full {full}"));
            assert_eq!(
                error.to_string(),
                decode_failure(full),
                "{label}, full {full}"
            );
        }
        for (label, info) in [
            ("10.04 s", EndFixture::source(10.04, false)),
            ("10.04 s and a tolerance", EndFixture::source(10.04, true)),
            ("10 s and playlists of 9.64 s", declared(10.0, 9.64, 9.62)),
        ] {
            fixture
                .verify(&file, full, &info)
                .unwrap_or_else(|error| panic!("{label}, full {full}: {error:#}"));
        }
    }
}

/// The video and the audio of an HLS source can come from playlists of different lengths. A stream
/// is confirmed by the length of its own playlist: audio that ends inside a frame at 9.3 s is
/// complete where its playlist declares 9.34 s, and cut short where the playlist declares 10.04 s.
#[test]
#[ignore = "needs the installed media tools; run explicitly"]
fn a_stream_is_confirmed_by_the_length_its_own_playlist_declares() {
    let fixture = EndFixture::new();
    // Frame 436 starts at 9.3 s: the audio ends inside it, the video runs to 10 s.
    let audio = fixture.ending_inside(436);
    let declared = |audio: f64| MediaInfo {
        declared_video_durations: vec![10.04],
        declared_audio_durations: vec![audio],
        ..EndFixture::source(10.04, true)
    };
    for extension in ["mp4", "ts"] {
        let file = fixture.mux(&format!("playlists.{extension}"), &audio, &[]);
        for full in [false, true] {
            fixture
                .verify(&file, full, &declared(9.34))
                .unwrap_or_else(|error| panic!("{extension}, full {full}: {error:#}"));
            let error = fixture
                .verify(&file, full, &declared(10.04))
                .expect_err(&format!(
                    "{extension}, full {full}, a longer audio playlist"
                ));
            assert_eq!(
                error.to_string(),
                decode_failure(full),
                "{extension}, full {full}"
            );
        }
    }
}

/// FFmpeg writes the tag `DURATION` of a Matroska stream as the end time of the stream, so audio
/// that starts a second late and ends in a cut frame at 11 s is confirmed by a source of 11.04 s and
/// its last frame is left out.
#[test]
#[ignore = "needs the installed media tools; run explicitly"]
fn a_matroska_stream_that_starts_late_is_bounded_by_its_end_time() {
    let fixture = EndFixture::new();
    fs::write(
        fixture.root.join("late.aac"),
        fixture.ending_inside(fixture.last_frame()),
    )
    .unwrap();
    let muxed = run_ffmpeg(
        &fixture.tools,
        &fixture.root,
        &[
            "-i",
            "video.mp4",
            "-itsoffset",
            "1",
            "-i",
            "late.aac",
            "-map",
            "0:v",
            "-map",
            "1:a",
            "-c",
            "copy",
            "late.mkv",
        ],
    );
    assert!(
        muxed.status.success(),
        "{}",
        String::from_utf8_lossy(&muxed.stderr)
    );
    let file = fixture.root.join("late.mkv");
    let probe = std::process::Command::new(&fixture.tools.ffprobe)
        .args(["-v", "error", "-show_streams", "-of", "json"])
        .arg(&file)
        .output()
        .unwrap();
    let probe: Value = serde_json::from_slice(&probe.stdout).unwrap();
    let audio = probe["streams"]
        .as_array()
        .unwrap()
        .iter()
        .find(|stream| stream["codec_type"] == "audio")
        .unwrap();
    assert!(
        audio["duration"].is_null() && audio["tags"]["DURATION"].is_string(),
        "the audio has only the tag: {audio}"
    );

    for full in [false, true] {
        fixture
            .verify(&file, full, &EndFixture::source(11.04, true))
            .unwrap_or_else(|error| panic!("full {full}: {error:#}"));
    }
}

/// FFmpeg counts a decode run from the start of the container, which a subtitle that starts a
/// little before the audio and video puts earlier than both. Here the video and audio start at
/// 0.8 s and a cue at 0 sets the start of the container, so a stop half a second before the end of
/// a stream is 0.5 s before it on that timeline. Damage in the audio 0.7 s before its end is
/// decoded and the cut last frame is not.
#[test]
#[ignore = "needs the installed media tools; run explicitly"]
fn decoding_stops_half_a_second_before_the_end_counted_from_the_container_start() {
    let fixture = EndFixture::new();
    fs::write(
        fixture.root.join("cue.srt"),
        "1\n00:00:00,000 --> 00:00:00,500\nhello\n",
    )
    .unwrap();
    // Frames 418-421 start at 8.92-8.98 s of the audio, which is 9.72-9.78 s in the file.
    let clean = fixture.ending_inside(fixture.last_frame());
    let mut damaged = clean.clone();
    fixture.add_noise(&mut damaged, 418..422);
    let mut files = Vec::new();
    for (name, audio) in [("clean", &clean), ("damaged", &damaged)] {
        let input = format!("{name}.aac");
        let output = format!("{name}.mkv");
        fs::write(fixture.root.join(&input), audio).unwrap();
        let muxed = run_ffmpeg(
            &fixture.tools,
            &fixture.root,
            &[
                "-itsoffset",
                "0.8",
                "-i",
                "video.mp4",
                "-itsoffset",
                "0.8",
                "-i",
                &input,
                "-i",
                "cue.srt",
                "-map",
                "0:v",
                "-map",
                "1:a",
                "-map",
                "2:s",
                "-c:v",
                "copy",
                "-c:a",
                "copy",
                "-c:s",
                "srt",
                &output,
            ],
        );
        assert!(
            muxed.status.success(),
            "{}",
            String::from_utf8_lossy(&muxed.stderr)
        );
        files.push(fixture.root.join(output));
    }
    let probe = std::process::Command::new(&fixture.tools.ffprobe)
        .args([
            "-v",
            "error",
            "-show_streams",
            "-show_format",
            "-of",
            "json",
        ])
        .arg(&files[0])
        .output()
        .unwrap();
    let probe: Value = serde_json::from_slice(&probe.stdout).unwrap();
    let start = |value: &Value| numeric_seconds(&value["start_time"]).unwrap();
    let video = probe["streams"]
        .as_array()
        .unwrap()
        .iter()
        .find(|stream| stream["codec_type"] == "video")
        .unwrap();
    assert!(
        start(&probe["format"]) < 0.1 && start(video) > 0.7,
        "the container starts at {}, the video at {}",
        start(&probe["format"]),
        start(video)
    );

    for full in [false, true] {
        fixture
            .verify(&files[0], full, &EndFixture::source(10.04, true))
            .unwrap_or_else(|error| panic!("the clean file, full {full}: {error:#}"));
        let error = fixture
            .verify(&files[1], full, &EndFixture::source(10.04, true))
            .expect_err(&format!("the damaged file, full {full}"));
        assert_eq!(error.to_string(), decode_failure(full), "full {full}");
    }
}

/// A download that is cut short ends inside a frame, and so does a source whose last segment
/// does. Only the source's duration tells them apart, so the end is decoded wherever that does
/// not confirm the file to within half a second: whatever the source's tolerance, for a playlist,
/// a live recording and a source without a duration. The source of 9.04 s confirms the file that
/// ends at 9 s.
#[test]
#[ignore = "needs the installed media tools; run explicitly"]
fn a_download_cut_short_inside_a_frame_is_refused_unless_the_source_confirms_it() {
    let fixture = EndFixture::new();
    // Frame 421 starts at 8.98 s: the audio ends inside it, the video where `-t` stops it.
    let short = fixture.mux("short.mp4", &fixture.ending_inside(421), &["-t", "9"]);
    let playlist = AddRequest {
        kind: DownloadKind::Video,
        playlist: true,
        ..AddRequest::default()
    };
    let live = MediaInfo {
        is_live: true,
        ..EndFixture::source(10.04, true)
    };

    // The default tolerance of half a second sees a second missing on its own.
    let missing = crate::i18n::ui_format!(
        "Medya doğrulaması: çıktı süresi kaynakla eşleşmiyor (erken bitiş toleransı {tolerance:.3} sn; geç bitiş {0:.3} sn)",
        "Media verification: the output duration does not match the source (early-end tolerance {tolerance:.3} s; late end {0:.3} s)",
        1.5,
        tolerance = 0.5
    );
    for full in [false, true] {
        let error = fixture
            .verify(&short, full, &EndFixture::source(10.0, false))
            .expect_err("a second shorter than the source");
        assert_eq!(error.to_string(), missing, "full {full}");

        for (label, info) in [
            ("an HLS source's tolerance", EndFixture::source(10.04, true)),
            ("no source duration", MediaInfo::default()),
            ("a live recording", live.clone()),
        ] {
            let error = fixture
                .verify(&short, full, &info)
                .expect_err(&format!("{label}, full {full}"));
            assert_eq!(
                error.to_string(),
                decode_failure(full),
                "{label}, full {full}"
            );
        }
        let error = fixture
            .verify_request(
                &short,
                &AddRequest {
                    full_verification: full,
                    ..playlist.clone()
                },
                &EndFixture::source(10.04, true),
            )
            .expect_err("a playlist");
        assert_eq!(
            error.to_string(),
            decode_failure(full),
            "a playlist, full {full}"
        );

        fixture
            .verify(&short, full, &EndFixture::source(9.04, false))
            .unwrap_or_else(|error| panic!("the same file when its source ends there: {error:#}"));
    }
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
