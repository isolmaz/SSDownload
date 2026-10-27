//! End-to-end HLS downloads through the installed yt-dlp and FFmpeg against a local server.
//! They need the media tools the app installs, so they run explicitly:
//! `cargo test --lib hls_tests -- --ignored`.

use super::*;
use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    sync::{Arc, Mutex},
    thread,
};

struct Fixture {
    root: PathBuf,
    paths: AppPaths,
    master: String,
    requests: Arc<Mutex<HashMap<String, usize>>>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

impl Fixture {
    /// Encodes a `video_seconds` video playlist and an `audio_seconds` audio playlist in 2 s
    /// segments, and serves them with a master playlist. `failing` answers 404 to that file's
    /// first requests.
    fn new(video_seconds: u32, audio_seconds: u32, failing: Option<(&str, usize)>) -> Self {
        let root = std::env::temp_dir().join(format!("ssdownload-hls-{}", uuid::Uuid::new_v4()));
        let site = root.join("site");
        fs::create_dir_all(&site).unwrap();
        let mut paths = AppPaths::new(root.join("profile")).unwrap();
        paths.tools_dir = PathBuf::from(std::env::var("LOCALAPPDATA").unwrap())
            .join("SSDownload")
            .join("tools");
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
        };
        encode(
            &format!("testsrc=size=320x180:rate=25:duration={video_seconds}"),
            &["-c:v", "libopenh264", "-g", "50"],
            "video.m3u8",
            "v%d.ts",
        );
        encode(
            &format!("sine=frequency=440:sample_rate=48000:duration={audio_seconds}"),
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
        let requests = Arc::new(Mutex::new(HashMap::new()));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let master = format!("http://{}/master.m3u8", listener.local_addr().unwrap());
        let failing = failing.map(|(name, count)| (name.to_owned(), count));
        let served = Arc::clone(&requests);
        thread::spawn(move || {
            for stream in listener.incoming().map_while(Result::ok) {
                let (site, served, failing) = (site.clone(), Arc::clone(&served), failing.clone());
                thread::spawn(move || serve(stream, &site, &served, failing.as_ref()));
            }
        });
        Self {
            root,
            paths,
            master,
            requests,
        }
    }

    fn download(&self) -> Result<PathBuf> {
        let request = AddRequest {
            url: self.master.clone(),
            kind: DownloadKind::Video,
            container: Some("mp4".into()),
            expected_audio: Some(true),
            connections: Some(16),
            ..AddRequest::default()
        };
        let output = self.root.join("film.mp4");
        download(
            &self.paths,
            &request,
            &output,
            &TransferControl::default(),
            &NetworkGovernor::new(16),
            &mut |_| {},
            "hls-end-to-end",
        )?;
        Ok(output)
    }

    /// How often each segment the encoder wrote for `prefix` was requested, in playlist order.
    fn requests(&self, prefix: &str) -> Vec<usize> {
        let requests = self.requests.lock().unwrap();
        let segments = fs::read_dir(self.root.join("site"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .filter(|name| name.starts_with(prefix) && name.ends_with(".ts"))
            .count();
        assert!(segments > 0);
        (0..segments)
            .map(|index| {
                requests
                    .get(&format!("{prefix}{index}.ts"))
                    .copied()
                    .unwrap_or(0)
            })
            .collect()
    }

    fn stream_seconds(&self, output: &Path) -> Vec<(String, f64)> {
        let tools = tools::verified_toolchain(&self.paths, &TransferControl::default()).unwrap();
        let probe = std::process::Command::new(&tools.ffprobe)
            .args([
                "-v",
                "error",
                "-show_entries",
                "stream=codec_type,duration",
                "-of",
                "json",
            ])
            .arg(output)
            .output()
            .unwrap();
        let value: Value = serde_json::from_slice(&probe.stdout).unwrap();
        value["streams"]
            .as_array()
            .unwrap()
            .iter()
            .map(|stream| {
                (
                    stream["codec_type"].as_str().unwrap().to_owned(),
                    stream["duration"].as_str().unwrap().parse().unwrap(),
                )
            })
            .collect()
    }
}

fn serve(
    stream: TcpStream,
    site: &Path,
    requests: &Mutex<HashMap<String, usize>>,
    failing: Option<&(String, usize)>,
) {
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
            let mut requests = requests.lock().unwrap();
            let count = requests.entry(name.clone()).or_insert(0);
            *count += 1;
            *count
        };
        let refused = failing.is_some_and(|(file, times)| *file == name && count <= *times);
        let body = (!refused)
            .then(|| fs::read(site.join(&name)).ok())
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

#[test]
#[ignore = "needs the installed media tools; run explicitly"]
fn a_source_with_a_longer_audio_playlist_downloads_once_and_verifies() {
    // The audio playlist runs 8 s past the video's, as streaming sites serve it.
    let fixture = Fixture::new(12, 20, None);
    let output = fixture.download().expect("the download");

    let streams = fixture.stream_seconds(&output);
    assert_eq!(streams.len(), 2, "{streams:?}");
    for (kind, expected) in [("video", 12.0), ("audio", 20.0)] {
        let (_, seconds) = streams.iter().find(|(codec, _)| codec == kind).unwrap();
        assert!((seconds - expected).abs() < 0.5, "{kind} {seconds}");
    }
    assert!(fixture.requests("v").iter().all(|count| *count == 1));
    assert!(fixture.requests("a").iter().all(|count| *count == 1));
}

#[test]
#[ignore = "needs the installed media tools and waits out yt-dlp's fragment retries (~5 min)"]
fn a_lost_audio_fragment_downloads_only_the_audio_again() {
    // yt-dlp tries a fragment 21 times (1 + `--fragment-retries 20`) before it gives the stream
    // up, so the first attempt loses `a5.ts` and the restart gets it.
    let fixture = Fixture::new(12, 20, Some(("a5.ts", 21)));
    let output = fixture.download().expect("the download");

    let streams = fixture.stream_seconds(&output);
    let audio = streams
        .iter()
        .find(|(codec, _)| codec == "audio")
        .unwrap()
        .1;
    assert!((audio - 20.0).abs() < 0.5, "{audio}");
    // The video stream finished before the loss and is not downloaded again.
    assert!(fixture.requests("v").iter().all(|count| *count == 1));
    let audio_requests = fixture.requests("a");
    assert!(audio_requests[5] > 21, "{audio_requests:?}");
    assert!(
        audio_requests
            .iter()
            .enumerate()
            .all(|(index, count)| index == 5 || (1..=2).contains(count)),
        "{audio_requests:?}"
    );
}
