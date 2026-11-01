//! App-native download of a pinned, finished HLS source: separate video and audio playlists, or
//! the one playlist of an audio download. The segments are fetched at once over connections kept
//! from one segment to the next, each segment is retried on its own, the requests to a host that
//! answers 429 or 503 are paced, and the joined streams are copied into one container, or the
//! audio is converted as yt-dlp's audio extraction converts it. Every other source shape stays
//! with yt-dlp.

use super::*;
use std::{
    collections::VecDeque,
    fs::File,
    io::{Seek, SeekFrom},
    sync::{
        atomic::{AtomicU64, Ordering},
        Condvar, Mutex,
    },
};

/// How many refusals (429 or 503) a segment goes again after with its host's next paced request;
/// after each later one it waits as after other failures.
const FREE_REFUSALS: u32 = 2;
/// Tries per segment before the download fails; the joined part is kept for a retry. For a
/// segment its host keeps refusing, however the host answers the other segments, the waits between
/// them span 390 s, as between 30 tries after other failures, since its first `FREE_REFUSALS`
/// refusals go again with the host's next paced request. A segment that fails otherwise waits 15 s
/// longer for each of those tries.
const SEGMENT_ATTEMPTS: u32 = 30 + FREE_REFUSALS;
const MAX_BACKOFF: Duration = Duration::from_secs(15);
/// Longest server-requested pause honoured for one segment.
const MAX_SEGMENT_RETRY_AFTER: Duration = Duration::from_secs(60);
const MAX_REDIRECTS: usize = 5;
/// The total parallelism stays below the global connection budget, which other jobs share.
const MAX_WORKERS: usize = crate::network::GLOBAL_CONNECTION_BUDGET - 2;

pub(super) struct NativeStream {
    role: &'static str,
    format_id: String,
    language: Option<String>,
    headers: Vec<(String, String)>,
    segments: Vec<Url>,
    /// Seconds the playlist declares, when every segment's length is readable.
    length: Option<f64>,
}

impl NativeStream {
    /// `<role>:<segments>` for the job log.
    pub(super) fn summary(&self) -> String {
        format!("{}:{}", self.role, self.segments.len())
    }
}

/// File types whose download yt-dlp's audio extraction keeps as it comes for the best audio (its
/// `COMMON_AUDIO_EXTS`).
const AUDIO_FILE_TYPES: &[&str] = &[
    "aiff", "alac", "flac", "m4a", "mka", "mp3", "ogg", "opus", "wav", "wma",
];

/// Whether yt-dlp's audio extraction can keep the download of a format of file type `ext` as it
/// comes, asked as `conversion` (`None` for the best audio): the best audio of a common audio file
/// type, or a file of the asked type whose codec that type takes unchanged. The app does not keep
/// a download as it comes, so such a download stays with yt-dlp; as the codec is known only once
/// the download is there, every format of the asked type does.
fn kept_as_it_comes(ext: &str, conversion: Option<&str>) -> bool {
    conversion.map_or(AUDIO_FILE_TYPES.contains(&ext), |asked| ext == asked)
}

/// The pinned source as native streams, for a single download whose tracks need no per-track
/// selection: for a video, exactly one video-only and one audio-only finished playlist; for an
/// audio download, one finished playlist of known length that carries audio, alone or with video
/// the conversion drops. `None` leaves the download to yt-dlp.
pub(super) fn native_streams(
    pinned: &PinnedMediaSource,
    request: &AddRequest,
) -> Option<Vec<NativeStream>> {
    let playlists = match request.kind {
        DownloadKind::Video => 2,
        DownloadKind::Audio => 1,
        _ => return None,
    };
    let conversion = audio_conversion(request).ok()?;
    if request.playlist
        || request.live_from_start
        || !request.audio_tracks.is_empty()
        || !request.subtitle_tracks.is_empty()
        || !request.subtitle_languages.is_empty()
        || !pinned.dash_formats.is_empty()
        || pinned.manifests.len() != playlists
    {
        return None;
    }
    let mut streams = pinned
        .manifests
        .iter()
        .map(|manifest| {
            let format = object_with_url(&pinned.inspection, &manifest.marker)?;
            let codec = |key: &str| format.get(key).and_then(Value::as_str);
            let role = match (codec("vcodec"), codec("acodec")) {
                (video, Some("none")) if video != Some("none") => "video",
                (Some("none"), audio) if audio != Some("none") => "audio",
                (_, audio) if playlists == 1 && audio != Some("none") => "audio",
                _ => return None,
            };
            if !extractor_cookies_are_known(format, request)
                || (playlists == 1
                    && codec("ext").is_some_and(|ext| kept_as_it_comes(ext, conversion)))
            {
                return None;
            }
            Some(NativeStream {
                role,
                format_id: codec("format_id")?.to_owned(),
                language: codec("language").map(str::to_owned),
                headers: stream_headers(format, request)?,
                segments: plain_segments(&manifest.body)?,
                length: finished_hls_fragments(&manifest.body).map(|lengths| lengths.iter().sum()),
            })
        })
        .collect::<Option<Vec<_>>>()?;
    if playlists == 1 {
        return (streams[0].role == "audio" && streams[0].length.is_some()).then_some(streams);
    }
    streams.sort_by_key(|stream| stream.role != "video");
    (streams[0].role == "video" && streams[1].role == "audio").then_some(streams)
}

/// The headers yt-dlp sends for the format: the request's own and its Referer, overridden by
/// the format's. Cookies are added per segment address.
fn stream_headers(format: &Value, request: &AddRequest) -> Option<Vec<(String, String)>> {
    let mut headers = Vec::<(String, String)>::new();
    let mut set = |name: &str, value: &str| {
        headers.retain(|(existing, _)| !existing.eq_ignore_ascii_case(name));
        headers.push((name.to_owned(), value.to_owned()));
    };
    for (name, value) in &request.headers {
        set(name, value);
    }
    if let Some(referer) = &request.referer {
        set("Referer", referer);
    }
    if let Some(own) = format.get("http_headers").and_then(Value::as_object) {
        for (name, value) in own {
            let value = value.as_str()?;
            if !name.eq_ignore_ascii_case("cookie") {
                set(name, value);
            }
        }
    }
    let unsafe_text = |text: &str| text.bytes().any(|byte| matches!(byte, 0 | b'\r' | b'\n'));
    (!headers
        .iter()
        .any(|(name, value)| name.is_empty() || unsafe_text(name) || unsafe_text(value)))
    .then_some(headers)
}

/// Whether every cookie yt-dlp holds for the format is one of the browser session's, which the
/// app sends itself. A cookie the extractor received from the site would be missing from the
/// segment requests, so such a source stays with yt-dlp.
fn extractor_cookies_are_known(format: &Value, request: &AddRequest) -> bool {
    let Some(cookies) = format.get("cookies").and_then(Value::as_str) else {
        return true;
    };
    cookies
        .split(';')
        .filter_map(|part| part.split_once('='))
        .map(|(name, _)| name.trim())
        .filter(|name| {
            !["domain", "path", "expires", "version"]
                .iter()
                .any(|attribute| name.eq_ignore_ascii_case(attribute))
        })
        .all(|name| {
            request
                .session_cookies
                .iter()
                .any(|cookie| cookie.name == name)
        })
}

/// Segment addresses of a finished media playlist, its initialization section first. `None`
/// for encryption, byte ranges, discontinuities or any tag this downloader does not know.
fn plain_segments(body: &str) -> Option<Vec<Url>> {
    let mut segments = Vec::new();
    let mut finished = false;
    let mut durations = 0usize;
    let mut initialization = false;
    for line in body.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let Some(tag) = line.strip_prefix('#') else {
            segments.push(web_url(line)?);
            continue;
        };
        let (name, attributes) = tag.split_once(':').unwrap_or((tag, ""));
        match name {
            "EXTM3U"
            | "EXT-X-VERSION"
            | "EXT-X-TARGETDURATION"
            | "EXT-X-MEDIA-SEQUENCE"
            | "EXT-X-PLAYLIST-TYPE"
            | "EXT-X-INDEPENDENT-SEGMENTS"
            | "EXT-X-ALLOW-CACHE"
            | "EXT-X-PROGRAM-DATE-TIME" => {}
            "EXTINF" => durations += 1,
            "EXT-X-ENDLIST" => finished = true,
            "EXT-X-KEY" if attributes.split(',').any(|part| part == "METHOD=NONE") => {}
            "EXT-X-MAP"
                if !initialization && segments.is_empty() && !attributes.contains("BYTERANGE") =>
            {
                let uri = attributes.split_once("URI=\"")?.1.split('"').next()?;
                segments.push(web_url(uri)?);
                initialization = true;
            }
            _ if !name.starts_with("EXT") => {}
            _ => return None,
        }
    }
    (finished && durations > 0 && segments.len() == durations + usize::from(initialization))
        .then_some(segments)
}

fn web_url(value: &str) -> Option<Url> {
    Url::parse(value)
        .ok()
        .filter(|url| matches!(url.scheme(), "http" | "https") && url.username().is_empty())
}

/// Downloads the streams into the workspace and copies them into `media.<container>`, or, for an
/// audio download, converts the audio into `media.<extension of the audio format>`.
pub(super) fn download_streams(
    streams: &[NativeStream],
    workspace: &Path,
    request: &AddRequest,
    tools: &tools::VerifiedTools,
    control: &TransferControl,
    network: &NetworkGovernor,
    progress: &mut dyn FnMut(TransferProgress),
) -> Result<PathBuf> {
    let joins = fetch_segments(streams, workspace, request, control, network, progress)?;
    if request.kind == DownloadKind::Audio {
        return extract_audio(
            &streams[0],
            &joins[0],
            workspace,
            request,
            tools,
            control,
            progress,
        );
    }
    let received = joins.iter().map(Join::stored_bytes).sum::<u64>();
    progress(TransferProgress {
        downloaded: received,
        total: Some(received),
        phase: crate::i18n::ui("Medya işleniyor", "Processing media").into(),
        processing: true,
        ..TransferProgress::default()
    });
    merge(streams, &joins, workspace, request, tools, control)
}

/// Downloads every stream's segments into its joined part.
fn fetch_segments(
    streams: &[NativeStream],
    workspace: &Path,
    request: &AddRequest,
    control: &TransferControl,
    network: &NetworkGovernor,
    progress: &mut dyn FnMut(TransferProgress),
) -> Result<Vec<Join>> {
    validate_scoped_cookies(&request.session_cookies, request.page_url.as_deref())?;
    let per_host = fragment_concurrency(request.connections, network.per_host_limit(), 0)
        .parse::<usize>()
        .unwrap_or(1);
    let joins = streams
        .iter()
        .map(|stream| Join::resume(workspace, stream))
        .collect::<Result<Vec<_>>>()?;
    let shared = Shared::new(streams, &joins, per_host);
    let workers = (per_host * streams.len()).clamp(1, MAX_WORKERS);
    let received = AtomicU64::new(joins.iter().map(Join::stored_bytes).sum());
    let context = WorkerContext {
        streams,
        joins: &joins,
        shared: &shared,
        cookies: &request.session_cookies,
        control,
        network,
        received: &received,
        workers: workers as u64,
    };
    thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| context.work());
        }
        report_until_done(&context, progress);
    });
    if control.is_cancelled() {
        crate::bail_code!(crate::error_codes::TRF_008);
    }
    if control.is_paused() {
        crate::bail_code!(crate::error_codes::TRF_009);
    }
    if let Some(error) = shared.take_failure() {
        return Err(error);
    }
    Ok(joins)
}

fn merge(
    streams: &[NativeStream],
    joins: &[Join],
    workspace: &Path,
    request: &AddRequest,
    tools: &tools::VerifiedTools,
    control: &TransferControl,
) -> Result<PathBuf> {
    let files = streams
        .iter()
        .zip(joins)
        .map(|(stream, join)| crate::browser_transfer::StreamFile {
            path: join.joined.clone(),
            role: stream.role.into(),
            format_id: Some(stream.format_id.clone()),
            language: stream.language.clone(),
        })
        .collect::<Vec<_>>();
    let container = request.container.as_deref().unwrap_or("mkv");
    let output = workspace.join(format!("media.{container}"));
    let staged = workspace.join(format!("media.native.temp.{container}"));
    let _ = fs::remove_file(&staged);
    // At least ten minutes, and enough for a slow disk to copy a large film at 20 MB/s.
    let bytes = joins.iter().map(Join::stored_bytes).sum::<u64>();
    let result = run_capture(
        &tools.ffmpeg,
        &browser_mux_args(request, &files, &staged),
        b"",
        Duration::from_secs((bytes / (20 << 20)).max(10 * 60)),
        control,
    )?;
    if !result.status.success() || !result.stderr.trim().is_empty() {
        let _ = fs::remove_file(&staged);
        bail!(
            "{}",
            crate::i18n::ui_format!(
                "Görüntü ve ses akışları kayıpsız birleştirilemedi: {0}",
                "The video and audio streams could not be merged losslessly: {0}",
                bounded_tail(&result.stderr, 4096)
            )
        );
    }
    fs::rename(&staged, &output)?;
    Ok(output)
}

/// What yt-dlp's audio extraction makes of audio in one codec: the output's file type, FFmpeg's
/// codec options, and whether they encode the audio again or copy it.
#[derive(Debug, PartialEq)]
struct AudioOutput {
    extension: &'static str,
    label: &'static str,
    codec: &'static [&'static str],
    encodes: bool,
}

/// The output yt-dlp's `--extract-audio` picks for audio in `codec` (as ffprobe names it) asked
/// as `conversion`, `None` for the best audio: the codec's own file type where one takes it
/// unchanged, MP3 otherwise. The quality is the one the app asks yt-dlp for: VBR 0 for MP3,
/// yt-dlp's default (5 of 10) for the rest.
fn audio_output(conversion: Option<&str>, codec: &str) -> AudioOutput {
    let output = |extension, label, codec, encodes| AudioOutput {
        extension,
        label,
        codec,
        encodes,
    };
    let own = match codec {
        "aac" => Some(output(
            "m4a",
            "M4A",
            &["-acodec", "copy", "-bsf:a", "aac_adtstoasc"],
            false,
        )),
        "mp3" => Some(output("mp3", "MP3", &["-acodec", "copy"], false)),
        "opus" => Some(output("opus", "Opus", &["-acodec", "copy"], false)),
        "vorbis" => Some(output("ogg", "Ogg", &["-acodec", "copy"], false)),
        "flac" => Some(output("flac", "FLAC", &["-acodec", "copy"], false)),
        "alac" => Some(output("m4a", "M4A", &["-acodec", "alac"], true)),
        _ => None,
    };
    match (conversion, own) {
        (None, Some(own)) => own,
        (Some(target), Some(own)) if target == codec || (target, codec) == ("m4a", "aac") => own,
        (None, None) => output(
            "mp3",
            "MP3",
            &["-acodec", "libmp3lame", "-q:a", "5.0"],
            true,
        ),
        (Some("m4a"), _) => output("m4a", "M4A", &["-acodec", "aac", "-q:a", "2.05"], true),
        (Some("opus"), _) => output("opus", "Opus", &["-acodec", "libopus"], true),
        (Some("flac"), _) => output("flac", "FLAC", &["-acodec", "flac"], true),
        // MP3, the one format left, which the app asks yt-dlp for at VBR 0.
        (Some(_), _) => output(
            "mp3",
            "MP3",
            &["-acodec", "libmp3lame", "-q:a", "0.0"],
            true,
        ),
    }
}

/// The codec of the first audio stream of `path`, as ffprobe names it.
fn audio_codec(
    path: &Path,
    tools: &tools::VerifiedTools,
    control: &TransferControl,
) -> Result<String> {
    let mut args = [
        "-v",
        "error",
        "-protocol_whitelist",
        "file",
        "-select_streams",
        "a",
        "-show_entries",
        "stream=codec_name",
        "-of",
        "json",
    ]
    .map(String::from)
    .to_vec();
    args.push(path.to_string_lossy().into_owned());
    let probe = run_capture(&tools.ffprobe, &args, b"", Duration::from_secs(60), control)?;
    let value = serde_json::from_slice::<Value>(&probe.stdout).unwrap_or_default();
    value["streams"][0]["codec_name"]
        .as_str()
        .filter(|_| probe.status.success())
        .map(str::to_owned)
        .ok_or_else(|| {
            anyhow!(crate::i18n::ui(
                "İndirilen akışta okunabilir ses yok",
                "The downloaded stream has no readable audio"
            ))
        })
}

/// Converts the joined stream's audio into `media.<extension>` as yt-dlp's audio extraction
/// converts it, dropping any video, and reports how much of the playlist's length FFmpeg has
/// written.
fn extract_audio(
    stream: &NativeStream,
    join: &Join,
    workspace: &Path,
    request: &AddRequest,
    tools: &tools::VerifiedTools,
    control: &TransferControl,
    progress: &mut dyn FnMut(TransferProgress),
) -> Result<PathBuf> {
    let received = join.stored_bytes();
    let length = stream.length.unwrap_or_default();
    let output = audio_output(
        audio_conversion(request)?,
        &audio_codec(&join.joined, tools, control)?,
    );
    let staged = workspace.join(format!("media.native.temp.{}", output.extension));
    let _ = fs::remove_file(&staged);
    let mut args = [
        "-v",
        "error",
        "-nostdin",
        "-protocol_whitelist",
        "file",
        "-i",
    ]
    .map(String::from)
    .to_vec();
    args.push(join.joined.to_string_lossy().into_owned());
    args.push("-vn".into());
    args.extend(output.codec.iter().map(|arg| arg.to_string()));
    args.extend(["-movflags", "+faststart", "-progress", "pipe:1", "-nostats"].map(String::from));
    args.push(staged.to_string_lossy().into_owned());
    progress(conversion_progress(
        &output,
        received,
        Duration::ZERO,
        length,
        Duration::ZERO,
    ));
    let started = Instant::now();
    // At least ten minutes, and as long as the audio plays: an encoder runs many times faster.
    let timeout = Duration::from_secs_f64(length.max(10.0 * 60.0));
    let result = run_ffmpeg_reporting(&tools.ffmpeg, &args, timeout, control, &mut |written| {
        progress(conversion_progress(
            &output,
            received,
            written,
            length,
            started.elapsed(),
        ))
    })
    .map_err(|error| {
        // A conversion that a pause, a cancel or the timeout stopped, or that did not start,
        // leaves no output; the joined stream stays for the next attempt.
        let _ = fs::remove_file(&staged);
        unfinished_conversion(error, timeout)
    })?;
    if !result.status.success() {
        let _ = fs::remove_file(&staged);
        bail!(
            "{}",
            crate::i18n::ui_format!(
                "Ses dönüştürülemedi: {0}",
                "The audio could not be converted: {0}",
                bounded_tail(&result.stderr, 4096)
            )
        );
    }
    let converted = workspace.join(format!("media.{}", output.extension));
    fs::rename(&staged, &converted)?;
    Ok(converted)
}

/// The error of a conversion that FFmpeg did not finish, which names a timeout as the
/// conversion's and not as a media analysis's.
fn unfinished_conversion(error: anyhow::Error, timeout: Duration) -> anyhow::Error {
    if crate::error_codes::code_of(&error) == Some(crate::error_codes::MED_011) {
        anyhow!(crate::i18n::ui_format!(
            "Ses dönüştürmesi {0} saniye içinde tamamlanmadı",
            "The audio conversion did not finish within {0} seconds",
            timeout.as_secs()
        ))
    } else {
        error
    }
}

/// The status of a conversion that has written `written` of `length` seconds of audio, `elapsed`
/// after it started. The bytes stay the downloaded ones, as the conversion downloads nothing: a
/// queue's byte quota counts every byte added to them, and, for a job that reports processing, not
/// the size of the saved file. The time left follows from the share written so far, once a second
/// has passed.
fn conversion_progress(
    output: &AudioOutput,
    received: u64,
    written: Duration,
    length: f64,
    elapsed: Duration,
) -> TransferProgress {
    let share = (written.as_secs_f64() / length).min(1.0);
    let percent = (share * 100.0) as u32;
    TransferProgress {
        processing: true,
        downloaded: received,
        total: Some(received),
        speed: 0,
        eta: (share > 0.0 && elapsed >= Duration::from_secs(1))
            .then(|| (elapsed.as_secs_f64() * (1.0 - share) / share).round() as u64),
        phase: if output.encodes {
            crate::i18n::ui_format!(
                "Ses {0} biçimine dönüştürülüyor: %{1}",
                "Converting the audio to {0}: {1}%",
                output.label,
                percent
            )
        } else {
            crate::i18n::ui_format!(
                "Ses {0} dosyasına kopyalanıyor: %{1}",
                "Copying the audio into {0}: {1}%",
                output.label,
                percent
            )
        },
    }
}

/// One stream's segments joined in playlist order. `<joined>.next` records how many segments
/// and bytes the joined file holds, so a resumed download truncates a half-written append
/// and continues after the last whole segment.
struct Join {
    joined: PathBuf,
    state: Mutex<JoinState>,
}

struct JoinState {
    next: usize,
    length: u64,
    ready: Vec<bool>,
}

impl Join {
    fn resume(workspace: &Path, stream: &NativeStream) -> Result<Self> {
        let joined = workspace.join(format!("media.native-{}.part", stream.role));
        let recorded = fs::read_to_string(index_path(&joined))
            .ok()
            .and_then(|text| {
                let (next, length) = text.trim().split_once(' ')?;
                Some((next.parse::<usize>().ok()?, length.parse::<u64>().ok()?))
            })
            .filter(|(next, length)| {
                *next <= stream.segments.len()
                    && fs::metadata(&joined).is_ok_and(|meta| meta.len() >= *length)
            });
        let (next, length) = recorded.unwrap_or((0, 0));
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&joined)?;
        file.set_len(length)?;
        let mut ready = vec![false; stream.segments.len()];
        for (index, done) in ready.iter_mut().enumerate() {
            let segment = segment_path(&joined, index);
            if index < next {
                let _ = fs::remove_file(&segment);
            } else {
                *done = segment.is_file();
            }
            let _ = fs::remove_file(partial_path(&segment));
        }
        let join = Self {
            joined,
            state: Mutex::new(JoinState {
                next,
                length,
                ready,
            }),
        };
        join.append_ready()?;
        Ok(join)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, JoinState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn stored_bytes(&self) -> u64 {
        let state = self.lock();
        state.length
            + (state.next..state.ready.len())
                .filter(|index| state.ready[*index])
                .filter_map(|index| fs::metadata(segment_path(&self.joined, index)).ok())
                .map(|meta| meta.len())
                .sum::<u64>()
    }

    fn pending(&self) -> Vec<usize> {
        let state = self.lock();
        (state.next..state.ready.len())
            .filter(|index| !state.ready[*index])
            .collect()
    }

    fn mark_ready(&self, index: usize) -> Result<()> {
        self.lock().ready[index] = true;
        self.append_ready()
    }

    /// Appends every finished segment that follows the joined ones, in playlist order.
    fn append_ready(&self) -> Result<()> {
        let mut state = self.lock();
        while state.next < state.ready.len() && state.ready[state.next] {
            let segment = segment_path(&self.joined, state.next);
            let mut target = OpenOptions::new().write(true).open(&self.joined)?;
            target.seek(SeekFrom::Start(state.length))?;
            let copied = std::io::copy(&mut File::open(&segment)?, &mut target)?;
            target.sync_data()?;
            state.next += 1;
            state.length += copied;
            crate::recovery::atomic_write(
                &index_path(&self.joined),
                format!("{} {}", state.next, state.length).as_bytes(),
            )?;
            fs::remove_file(&segment)?;
        }
        Ok(())
    }
}

fn index_path(joined: &Path) -> PathBuf {
    let mut name = joined.as_os_str().to_owned();
    name.push(".next");
    PathBuf::from(name)
}

fn segment_path(joined: &Path, index: usize) -> PathBuf {
    let mut name = joined.as_os_str().to_owned();
    name.push(format!("-Seg{index:06}"));
    PathBuf::from(name)
}

fn partial_path(segment: &Path) -> PathBuf {
    let mut name = segment.as_os_str().to_owned();
    name.push(".tmp");
    PathBuf::from(name)
}

/// The wait before a segment's next attempt after `failures` failures: 1, 2, 4 and 8 s, then
/// `MAX_BACKOFF`.
fn backoff(failures: u32) -> Duration {
    Duration::from_secs(1 << failures.saturating_sub(1).min(4)).min(MAX_BACKOFF)
}

#[derive(Clone, Copy)]
struct Task {
    stream: usize,
    index: usize,
    attempt: u32,
    /// How often the segment's host refused it. Admissions of other segments leave it as it is,
    /// so that a segment its host keeps refusing backs off however the others fare.
    refused: u32,
    not_before: Instant,
    /// When the task was last handed to a worker.
    sent: Instant,
}

/// A refusal keeps this share of a host's paced rate.
const PACE_CUT: f64 = 0.9;
/// Requests per second a paced rate gains with each admitted request, so that it grows by this
/// share every second at any rate, and by this share more for every `PACE_SPEEDUP` since the
/// host's last refusal.
const PACE_GROWTH: f64 = 0.02;
const PACE_SPEEDUP: Duration = Duration::from_secs(5);

/// How the requests to one host are spaced, for every stream together. A host that never answers
/// 429 or 503 gets them as fast as the per-host bound allows. From its first such answer on they
/// start `1 / rate` apart, and one that starts late leaves the next up to a quarter of that in
/// hand, so that workers waking late (at the next timer tick) do not lower the rate. The first
/// answer sets the rate a little below the rate the host admitted requests at until then, counting
/// the requests then under way that it admits after that answer, and every later refusal of a
/// request sent after the last reaction cuts the rate to `PACE_CUT` of itself; the requests
/// already under way at a reaction answer for the rate it replaced, so a burst of refusals cuts
/// once. An admitted request raises the rate while the pace holds requests back. When the host
/// admits none of the requests sent after the last reaction and refuses two of them, the wait
/// before the next request also doubles each time, up to `MAX_BACKOFF`, and until the host admits
/// one again (the lockout) a `Retry-After` holds every request to it. The host is that of the
/// segment's own address: a refusal from a redirect target paces the host that redirected.
struct Pace {
    /// Requests per second, from the host's first 429 or 503 on; never below one per
    /// `MAX_BACKOFF`.
    rate: Option<f64>,
    /// No request to the host starts before this.
    next_start: Instant,
    /// When the first request to the host was sent, and how many of the requests sent before the
    /// first reaction the host admitted.
    first: Option<Instant>,
    admitted: usize,
    /// Seconds from the first request to the first reaction, until the second reaction.
    estimate: Option<f64>,
    /// The last reaction to a refusal, and the answers since to requests sent after it.
    reacted: Option<Instant>,
    admitted_since: usize,
    refused_since: usize,
    /// Reactions in a row that found nothing admitted since the one before.
    lockout: u32,
}

impl Pace {
    fn new(now: Instant) -> Self {
        Self {
            rate: None,
            next_start: now,
            first: None,
            admitted: 0,
            estimate: None,
            reacted: None,
            admitted_since: 0,
            refused_since: 0,
            lockout: 0,
        }
    }

    fn start(&mut self, now: Instant) {
        self.first.get_or_insert(now);
        if let Some(rate) = self.rate {
            let interval = Duration::from_secs_f64(1.0 / rate);
            let slot = self
                .next_start
                .max(now.checked_sub(interval / 4).unwrap_or(now));
            self.next_start = slot + interval;
        }
    }

    /// Whether the answer to a request sent at `sent` tells about the current rate.
    fn answers_for_rate(&self, sent: Instant) -> bool {
        self.reacted.is_none_or(|reacted| sent >= reacted)
    }

    fn admit(&mut self, sent: Instant, now: Instant) {
        let fresh = self.answers_for_rate(sent);
        self.admitted_since += usize::from(fresh);
        self.lockout = 0;
        let (Some(rate), Some(reacted)) = (&mut self.rate, self.reacted) else {
            self.admitted += 1;
            return;
        };
        if let Some(elapsed) = self.estimate.filter(|_| !fresh) {
            self.admitted += 1;
            *rate = rate.max(PACE_CUT * self.admitted as f64 / elapsed);
        }
        if self.next_start > now {
            let quiet = now.saturating_duration_since(reacted).as_secs_f64();
            *rate += PACE_GROWTH * (1.0 + quiet / PACE_SPEEDUP.as_secs_f64());
        }
    }

    fn refuse(&mut self, sent: Instant, retry_after: Option<Duration>, now: Instant) {
        if self.answers_for_rate(sent) {
            self.refused_since += 1;
            match self.rate {
                None => {
                    let elapsed = now
                        .saturating_duration_since(self.first.unwrap_or(sent))
                        .as_secs_f64()
                        .max(0.001);
                    self.react(PACE_CUT * self.admitted.max(1) as f64 / elapsed, now);
                    self.estimate = Some(elapsed);
                }
                Some(rate) if self.admitted_since > 0 => {
                    self.react(rate * PACE_CUT, now);
                }
                Some(rate) if self.refused_since >= 2 => {
                    self.lockout += 1;
                    let rate = self.react(rate * PACE_CUT, now);
                    let wait = (f64::from(1u32 << self.lockout.min(16)) / rate)
                        .min(MAX_BACKOFF.as_secs_f64());
                    self.next_start = self.next_start.max(now + Duration::from_secs_f64(wait));
                }
                Some(_) => {}
            }
        }
        if let Some(delay) = retry_after.filter(|_| self.lockout > 0) {
            self.next_start = self
                .next_start
                .max(now + delay.min(MAX_SEGMENT_RETRY_AFTER));
        }
    }

    /// Sets the rate, no lower than one request per `MAX_BACKOFF`, and starts a new reaction.
    fn react(&mut self, rate: f64, now: Instant) -> f64 {
        let rate = rate.max(1.0 / MAX_BACKOFF.as_secs_f64());
        self.rate = Some(rate);
        (self.reacted, self.admitted_since, self.refused_since) = (Some(now), 0, 0);
        self.estimate = None;
        rate
    }
}

struct Shared {
    state: Mutex<Schedule>,
    wake: Condvar,
    /// Each segment's host, as an index into `host_names`.
    hosts: Vec<Vec<usize>>,
    host_names: Vec<String>,
}

struct Schedule {
    queues: Vec<VecDeque<Task>>,
    /// Each host's pace.
    paces: Vec<Pace>,
    /// Requests in flight per host across all streams, never above `per_host`.
    busy: Vec<usize>,
    /// Workers that may keep a connection to each host between its segments, never above
    /// `per_host`; while open, such a connection holds a place of the host's connection bound.
    holders: Vec<usize>,
    per_host: usize,
    remaining: usize,
    failure: Option<anyhow::Error>,
}

/// What a worker does next.
enum Next {
    Fetch(Task),
    /// Close the kept connection: the worker has nothing to fetch at once, and its host has no
    /// segment that waits only for the host's pace or another requester waits for a place of the
    /// host. An idle connection would hold a place of its host's bound that another download, or
    /// another worker of this one, may be waiting for.
    Release,
}

impl Shared {
    fn new(streams: &[NativeStream], joins: &[Join], per_host: usize) -> Self {
        let now = Instant::now();
        let queues = joins
            .iter()
            .enumerate()
            .map(|(stream, join)| {
                join.pending()
                    .into_iter()
                    .map(|index| Task {
                        stream,
                        index,
                        attempt: 0,
                        refused: 0,
                        not_before: now,
                        sent: now,
                    })
                    .collect::<VecDeque<_>>()
            })
            .collect::<Vec<_>>();
        let remaining = queues.iter().map(VecDeque::len).sum();
        debug_assert_eq!(streams.len(), queues.len());
        let mut host_names = Vec::<String>::new();
        let hosts = streams
            .iter()
            .map(|stream| {
                stream
                    .segments
                    .iter()
                    .map(|url| {
                        let name = host_key(url);
                        host_names
                            .iter()
                            .position(|known| *known == name)
                            .unwrap_or_else(|| {
                                host_names.push(name);
                                host_names.len() - 1
                            })
                    })
                    .collect()
            })
            .collect::<Vec<Vec<usize>>>();
        Self {
            state: Mutex::new(Schedule {
                queues,
                paces: host_names.iter().map(|_| Pace::new(now)).collect(),
                busy: vec![0; host_names.len()],
                holders: vec![0; host_names.len()],
                per_host,
                remaining,
                failure: None,
            }),
            wake: Condvar::new(),
            hosts,
            host_names,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Schedule> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn take_failure(&self) -> Option<anyhow::Error> {
        self.lock().failure.take()
    }

    fn finished(&self) -> bool {
        let state = self.lock();
        state.remaining == 0 || state.failure.is_some()
    }

    fn fail(&self, error: anyhow::Error) {
        let mut state = self.lock();
        state.failure.get_or_insert(error);
        self.wake.notify_all();
    }

    /// The next due segment (see `Schedule::take`). A worker with nothing to take at once waits
    /// with its connection only while a segment of its host waits for nothing but the host's
    /// pace and nobody else waits for a place of the host; otherwise it closes the connection
    /// instead.
    fn next_task(
        &self,
        streams: &[NativeStream],
        control: &TransferControl,
        network: &NetworkGovernor,
        kept: Option<usize>,
    ) -> Option<Next> {
        let mut state = self.lock();
        loop {
            if state.remaining == 0 || state.failure.is_some() || control.stop_requested() {
                return None;
            }
            let now = Instant::now();
            let (earliest, paced) = match state.take(&self.hosts, streams, kept, now) {
                Ok(task) => return Some(Next::Fetch(task)),
                Err(later) => later,
            };
            if let Some(host) = kept {
                // The governor's lock is taken inside the schedule's here and nowhere the other
                // way round, so the two cannot deadlock.
                if !paced || network.host_has_waiters(&self.host_names[host]) {
                    state.holders[host] -= 1;
                    return Some(Next::Release);
                }
            }
            let wait = earliest
                .map(|at| at.saturating_duration_since(now))
                .unwrap_or(Duration::from_millis(200))
                .clamp(Duration::from_millis(10), Duration::from_millis(200));
            state = self
                .wake
                .wait_timeout(state, wait)
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
    }

    fn settle(&self, host: usize, outcome: Settled) {
        self.lock().settle(host, outcome, Instant::now());
        self.wake.notify_all();
    }
}

impl Schedule {
    /// Hands out at `now` the next due segment whose host has room for the worker that keeps a
    /// connection to `kept` (`hosts` as `Shared::hosts`), and whose host's pace lets a request
    /// start. Each stream offers its lowest playlist position, so joins advance while later
    /// segments arrive, and the stream furthest behind through its own playlist goes first, so
    /// video and audio finish together and a throttled stream is not left for the end. A worker
    /// that takes a segment of another host gives up its place at `kept`. Without a segment to
    /// hand out, tells when the next one is due, and whether a segment of `kept` waits for
    /// nothing but that host's pace.
    fn take(
        &mut self,
        hosts: &[Vec<usize>],
        streams: &[NativeStream],
        kept: Option<usize>,
        now: Instant,
    ) -> Result<Task, (Option<Instant>, bool)> {
        let mut earliest: Option<Instant> = None;
        let per_host = self.per_host;
        let mut chosen: Option<(usize, usize, usize)> = None;
        let mut paced = false;
        for stream in 0..self.queues.len() {
            for position in 0..self.queues[stream].len() {
                let task = self.queues[stream][position];
                let host = hosts[stream][task.index];
                let due = task.not_before.max(self.paces[host].next_start);
                if due > now {
                    paced |=
                        kept == Some(host) && task.not_before <= now && self.busy[host] < per_host;
                    earliest = Some(earliest.map_or(due, |at| at.min(due)));
                    continue;
                }
                if self.busy[host] >= per_host
                    || (kept != Some(host) && self.holders[host] >= per_host)
                {
                    continue;
                }
                let behind = chosen.as_ref().is_none_or(|(other, at, _)| {
                    let other_index = self.queues[*other][*at].index;
                    task.index * streams[*other].segments.len()
                        < other_index * streams[stream].segments.len()
                });
                if behind {
                    chosen = Some((stream, position, host));
                }
                break;
            }
        }
        let Some((stream, position, host)) = chosen else {
            return Err((earliest, paced));
        };
        self.busy[host] += 1;
        self.paces[host].start(now);
        if kept != Some(host) {
            self.holders[host] += 1;
            if let Some(previous) = kept {
                self.holders[previous] -= 1;
            }
        }
        let task = self.queues[stream].remove(position).expect("a queued task");
        Ok(Task { sent: now, ..task })
    }

    /// Takes in what became of a segment handed out to a worker, at `now`. A retried segment
    /// goes back in playlist order. An ordinary retry waits out the backoff of the segment's
    /// failures. A refused one (429 or 503) goes again with the host's next paced request after
    /// its first `FREE_REFUSALS` refusals, and after each later one waits out the backoff of its
    /// refusals beyond those, however the host answers the other segments. A `Retry-After`
    /// replaces the wait of an ordinary or refused retry, but not that backoff.
    fn settle(&mut self, host: usize, outcome: Settled, now: Instant) {
        self.busy[host] = self.busy[host].saturating_sub(1);
        match outcome {
            Settled::Done { sent } => {
                self.paces[host].admit(sent, now);
                self.remaining -= 1;
            }
            Settled::Failed => {}
            Settled::Retry {
                mut task,
                throttled,
                retry_after,
            } => {
                let mut wait = retry_after.map_or(
                    if throttled {
                        Duration::ZERO
                    } else {
                        backoff(task.attempt)
                    },
                    |delay| delay.min(MAX_SEGMENT_RETRY_AFTER),
                );
                if throttled {
                    self.paces[host].refuse(task.sent, retry_after, now);
                    task.refused += 1;
                    if task.refused > FREE_REFUSALS {
                        wait = wait.max(backoff(task.refused - FREE_REFUSALS));
                    }
                }
                task.not_before = now + wait;
                let queue = &mut self.queues[task.stream];
                let position = queue
                    .iter()
                    .position(|queued| queued.index > task.index)
                    .unwrap_or(queue.len());
                queue.insert(position, task);
            }
        }
    }
}

enum Settled {
    /// The segment is whole; its request was sent at `sent`.
    Done {
        sent: Instant,
    },
    /// The segment goes again; its attempts count the one that failed. `throttled` for a 429
    /// or 503 answer, with the answer's `Retry-After`.
    Retry {
        task: Task,
        throttled: bool,
        retry_after: Option<Duration>,
    },
    Failed,
}

fn file_retry(error: &std::io::Error) -> Fetched {
    Fetched::Retry {
        status: None,
        retry_after: None,
        reason: format!("local file: {error}"),
    }
}

fn host_key(url: &Url) -> String {
    crate::network::canonical_host(url).unwrap_or_else(|_| url.host_str().unwrap_or("").into())
}

struct WorkerContext<'a> {
    streams: &'a [NativeStream],
    joins: &'a [Join],
    shared: &'a Shared,
    cookies: &'a [ScopedCookie],
    control: &'a TransferControl,
    network: &'a NetworkGovernor,
    received: &'a AtomicU64,
    workers: u64,
}

enum Fetched {
    Done,
    Redirect(Url),
    Retry {
        status: Option<u32>,
        retry_after: Option<Duration>,
        reason: String,
    },
}

/// A worker's connection to one origin, kept from one segment to the next.
struct Kept {
    origin: url::Origin,
    easy: crate::network::GovernedEasy,
}

impl WorkerContext<'_> {
    fn work(&self) {
        // The host whose connection this worker may keep, and the connection while it is open.
        let mut held = None;
        let mut kept = None;
        while let Some(next) = self
            .shared
            .next_task(self.streams, self.control, self.network, held)
        {
            let mut task = match next {
                Next::Fetch(task) => task,
                Next::Release => {
                    (held, kept) = (None, None);
                    continue;
                }
            };
            let stream_index = task.stream;
            let stream = &self.streams[stream_index];
            let join = &self.joins[stream_index];
            let host = self.shared.hosts[stream_index][task.index];
            if held != Some(host) {
                (held, kept) = (Some(host), None);
            }
            let segment = segment_path(&join.joined, task.index);
            let mut url = stream.segments[task.index].clone();
            let mut fetched = self.fetch_kept(&mut kept, &url, stream, &segment);
            for _ in 0..MAX_REDIRECTS {
                let Ok(Fetched::Redirect(next)) = &fetched else {
                    break;
                };
                url = next.clone();
                let same_origin = url.origin() == stream.segments[task.index].origin();
                fetched = self
                    .network
                    .easy_for_url(&url, self.control)
                    .and_then(|mut easy| {
                        self.fetch(&mut easy, &url, stream, &segment, same_origin)
                    });
            }
            match fetched {
                Ok(Fetched::Done) => {
                    self.shared.settle(host, Settled::Done { sent: task.sent });
                    if let Err(error) = join.mark_ready(task.index) {
                        self.shared.fail(error);
                    }
                }
                Ok(Fetched::Retry {
                    status,
                    retry_after,
                    reason,
                }) if task.attempt + 1 < SEGMENT_ATTEMPTS => {
                    let throttled = matches!(status, Some(429 | 503));
                    if throttled {
                        self.control.mark_throttled();
                    }
                    task.attempt += 1;
                    crate::logging::record(
                        crate::logging::Event::warn("media.segment_retry")
                            .host(self.shared.host_names[host].clone())
                            .detail(format!(
                                "{} segment {} deneme {}: {reason}",
                                stream.role, task.index, task.attempt
                            )),
                    );
                    self.shared.settle(
                        host,
                        Settled::Retry {
                            task,
                            throttled,
                            retry_after,
                        },
                    );
                }
                Ok(Fetched::Retry { status, reason, .. }) => {
                    self.shared.settle(host, Settled::Failed);
                    self.shared
                        .fail(segment_failure(stream, task.index, status, &reason));
                }
                Ok(Fetched::Redirect(_)) => {
                    self.shared.settle(host, Settled::Failed);
                    self.shared.fail(segment_failure(
                        stream,
                        task.index,
                        None,
                        "too many redirects",
                    ));
                }
                Err(error) => {
                    self.shared.settle(host, Settled::Failed);
                    self.shared.fail(error);
                }
            }
        }
    }

    /// `fetch` over the kept connection when it is to the same origin. The connection is kept
    /// for the next segment after a whole answer, unless another requester (another download,
    /// or a worker of this one) waits for a slot of the host: then it closes, so the slot passes
    /// on after every segment. After a redirect it closes before the target gets a connection
    /// of its own, so a worker never waits for one while it holds another.
    fn fetch_kept(
        &self,
        kept: &mut Option<Kept>,
        url: &Url,
        stream: &NativeStream,
        segment: &Path,
    ) -> Result<Fetched> {
        let origin = url.origin();
        let mut easy = match kept.take().filter(|kept| kept.origin == origin) {
            Some(kept) => kept.easy,
            None => self.network.easy_for_url(url, self.control)?,
        };
        let fetched = self.fetch(&mut easy, url, stream, segment, true);
        if matches!(
            fetched,
            Ok(Fetched::Done
                | Fetched::Retry {
                    status: Some(_),
                    ..
                })
        ) && !self.network.host_has_waiters(&host_key(url))
        {
            *kept = Some(Kept { origin, easy });
        }
        fetched
    }

    /// One request for a segment into `<segment>.tmp`, renamed to `segment` once whole. Off the
    /// segment's own origin, which a redirect can lead to, the request carries only the headers
    /// that are safe across origins and no cookies, as a file transfer's does.
    fn fetch(
        &self,
        easy: &mut crate::network::GovernedEasy,
        url: &Url,
        stream: &NativeStream,
        segment: &Path,
        same_origin: bool,
    ) -> Result<Fetched> {
        let partial = partial_path(segment);
        let mut file = match File::create(&partial) {
            Ok(file) => file,
            Err(error) => return Ok(file_retry(&error)),
        };
        // Each request sets its options again on a kept handle; a reset would also remove the
        // handle's socket accounting.
        crate::transfer::configure_easy(easy, url)?;
        easy.low_speed_time(Duration::from_secs(30))?;
        easy.progress(true)?;
        let mut headers = List::new();
        for (name, value) in &stream.headers {
            if crate::transfer::header_is_safe_across_origin(
                &name.trim().to_ascii_lowercase(),
                same_origin,
            ) {
                headers.append(&format!("{name}: {value}"))?;
            }
        }
        let cookie = self
            .cookies
            .iter()
            .filter(|cookie| same_origin && scoped_cookie_applies(cookie, url))
            .map(|cookie| format!("{}={}", cookie.name, cookie.value))
            .collect::<Vec<_>>()
            .join("; ");
        if !cookie.is_empty() {
            headers.append(&format!("Cookie: {cookie}"))?;
        }
        easy.http_headers(headers)?;
        let mut written = 0u64;
        let mut response_headers = Vec::new();
        let mut write_error = None;
        let mut started = Instant::now();
        let mut cost = 0u64;
        let performed = {
            let mut transfer = easy.transfer();
            transfer.header_function(|line| {
                response_headers.push(String::from_utf8_lossy(line).trim().to_owned());
                true
            })?;
            transfer.write_function(|data| {
                if let Err(error) = file.write_all(data) {
                    write_error = Some(error);
                    return Ok(0);
                }
                written += data.len() as u64;
                self.received
                    .fetch_add(data.len() as u64, Ordering::Relaxed);
                crate::transfer::throttle(
                    self.control,
                    self.workers,
                    &mut started,
                    &mut cost,
                    data.len() as u64,
                );
                Ok(data.len())
            })?;
            transfer.progress_function(|_, _, _, _| !self.control.stop_requested())?;
            transfer.perform()
        };
        drop(file);
        let header = |name: &str| {
            response_headers.iter().rev().find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.trim()
                    .eq_ignore_ascii_case(name)
                    .then(|| value.trim().to_owned())
            })
        };
        let status = easy.response_code().unwrap_or(0);
        let whole = performed.is_ok()
            && status == 200
            && written > 0
            && header("content-length")
                .and_then(|length| length.parse::<u64>().ok())
                .is_none_or(|length| length == written);
        let renamed = if whole {
            match fs::rename(&partial, segment) {
                Ok(()) => return Ok(Fetched::Done),
                Err(error) => Some(error),
            }
        } else {
            None
        };
        self.received.fetch_sub(written, Ordering::Relaxed);
        let _ = fs::remove_file(&partial);
        if let Some(error) = write_error.or(renamed) {
            return Ok(file_retry(&error));
        }
        if self.control.stop_requested() {
            return Ok(Fetched::Retry {
                status: None,
                retry_after: None,
                reason: "stopped".into(),
            });
        }
        if let Err(error) = performed {
            return Ok(Fetched::Retry {
                status: None,
                retry_after: None,
                reason: easy.network_error().unwrap_or_else(|| error.to_string()),
            });
        }
        if matches!(status, 301 | 302 | 303 | 307 | 308) {
            return crate::transfer::redirected_url(url, header("location").as_deref())
                .map(Fetched::Redirect);
        }
        Ok(Fetched::Retry {
            status: Some(status),
            retry_after: header("retry-after")
                .as_deref()
                .and_then(crate::transfer::parse_retry_after),
            reason: if status == 200 {
                format!("incomplete body ({written} bytes)")
            } else {
                format!("HTTP {status}")
            },
        })
    }
}

fn segment_failure(
    stream: &NativeStream,
    index: usize,
    status: Option<u32>,
    reason: &str,
) -> anyhow::Error {
    let stream_name = if stream.role == "video" {
        crate::i18n::ui("Görüntü", "video")
    } else {
        crate::i18n::ui("Ses", "audio")
    };
    let message = crate::i18n::ui_format!(
        "{1} akışının {0}. parçası {attempts} denemede indirilemedi: {reason}",
        "Segment {0} of the {1} stream failed after {attempts} attempts: {reason}",
        index + 1,
        stream_name,
        attempts = SEGMENT_ATTEMPTS,
        reason = reason
    );
    match status.filter(|status| (400..600).contains(status)) {
        // The engine renews an expired source by the typed status, not by the sentence.
        Some(status) => anyhow!(crate::transfer::HttpStatusError { status }).context(message),
        None => anyhow!(message),
    }
}

/// Reports the combined progress of every stream until the segments are done or the
/// download stops. The total is estimated from the average size of the segments so far.
fn report_until_done(context: &WorkerContext, progress: &mut dyn FnMut(TransferProgress)) {
    let started = Instant::now();
    let mut window = VecDeque::from([(started, context.received.load(Ordering::Relaxed))]);
    loop {
        let finished = context.shared.finished() || context.control.stop_requested();
        let downloaded = context.received.load(Ordering::Relaxed);
        let now = Instant::now();
        window.push_back((now, downloaded));
        // A throttling host answers in bursts seconds apart; a shorter window reads zero between.
        while window.len() > 2 && now.duration_since(window[0].0) > Duration::from_secs(10) {
            window.pop_front();
        }
        let (since, base) = window[0];
        let elapsed = now.duration_since(since).as_secs_f64();
        let speed = if elapsed > 0.0 {
            (downloaded.saturating_sub(base) as f64 / elapsed) as u64
        } else {
            0
        };
        let total = estimated_total(context);
        progress(TransferProgress {
            processing: false,
            downloaded,
            total,
            speed,
            eta: total
                .filter(|total| *total >= downloaded)
                .and_then(|total| (total - downloaded).checked_div(speed)),
            phase: crate::i18n::ui("Medya indiriliyor", "Downloading media").into(),
        });
        if finished {
            return;
        }
        thread::sleep(Duration::from_millis(250));
    }
}

fn estimated_total(context: &WorkerContext) -> Option<u64> {
    let mut total = 0u64;
    for (stream, join) in context.streams.iter().zip(context.joins) {
        let state = join.lock();
        if state.next == state.ready.len() {
            total += state.length;
            continue;
        }
        if state.next == 0 {
            return None;
        }
        total += state.length / state.next as u64 * stream.segments.len() as u64;
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const PLAIN: &str = "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:7\n#EXT-X-MEDIA-SEQUENCE:0\n\
        #EXTINF:6.0,\nhttps://cdn.example/a/0.ts\n#EXTINF:4.0,\nhttps://cdn.example/a/1.ts\n#EXT-X-ENDLIST\n";

    #[test]
    fn a_plain_finished_playlist_lists_its_segments_in_order() {
        let segments = plain_segments(PLAIN).unwrap();
        assert_eq!(
            segments.iter().map(Url::as_str).collect::<Vec<_>>(),
            ["https://cdn.example/a/0.ts", "https://cdn.example/a/1.ts"]
        );
        let fragmented = PLAIN.replace(
            "#EXT-X-MEDIA-SEQUENCE:0\n",
            "#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-MAP:URI=\"https://cdn.example/a/init.mp4\"\n#EXT-X-KEY:METHOD=NONE\n",
        );
        let segments = plain_segments(&fragmented).unwrap();
        assert_eq!(segments[0].as_str(), "https://cdn.example/a/init.mp4");
        assert_eq!(segments.len(), 3);
    }

    #[test]
    fn playlists_the_app_cannot_join_byte_for_byte_stay_with_yt_dlp() {
        let tagged = |tag: &str| PLAIN.replace("#EXTINF:4.0,", &format!("{tag}\n#EXTINF:4.0,"));
        for rejected in [
            PLAIN.replace("#EXT-X-ENDLIST\n", ""),
            tagged("#EXT-X-KEY:METHOD=AES-128,URI=\"https://cdn.example/key\""),
            tagged("#EXT-X-BYTERANGE:1000@0"),
            tagged("#EXT-X-DISCONTINUITY"),
            tagged("#EXT-X-MAP:URI=\"https://cdn.example/a/init.mp4\""),
            tagged("#EXT-X-DATERANGE:ID=\"ad\",START-DATE=\"2026-01-01T00:00:00Z\""),
            PLAIN.replace("https://cdn.example/a/1.ts", "ftp://cdn.example/a/1.ts"),
            PLAIN.replace("#EXTINF:4.0,\n", ""),
        ] {
            assert!(plain_segments(&rejected).is_none(), "{rejected}");
        }
        assert!(plain_segments(&tagged("# a comment")).is_some());
    }

    fn pinned(video: Value, audio: Value) -> PinnedMediaSource {
        PinnedMediaSource {
            inspection: json!({"formats": [video, audio]}),
            manifests: vec![
                PinnedManifest {
                    marker: "pinned-0".into(),
                    body: PLAIN.into(),
                },
                PinnedManifest {
                    marker: "pinned-1".into(),
                    body: PLAIN.into(),
                },
            ],
            dash_formats: Vec::new(),
        }
    }

    fn separate() -> PinnedMediaSource {
        pinned(
            json!({"format_id": "720", "url": "pinned-0", "vcodec": "avc1", "acodec": "none"}),
            json!({"format_id": "aud-tr", "url": "pinned-1", "vcodec": "none", "acodec": null,
                   "language": "tr", "http_headers": {"Referer": "https://site.example/", "Cookie": "x=1"}}),
        )
    }

    #[test]
    fn separate_video_and_audio_playlists_download_natively() {
        let request = AddRequest {
            kind: DownloadKind::Video,
            referer: Some("https://embed.example/".into()),
            headers: [("Origin".to_owned(), "https://embed.example".to_owned())].into(),
            ..AddRequest::default()
        };
        let streams = native_streams(&separate(), &request).unwrap();
        assert_eq!(
            streams.iter().map(|stream| stream.role).collect::<Vec<_>>(),
            ["video", "audio"]
        );
        assert_eq!(streams[1].format_id, "aud-tr");
        assert_eq!(streams[1].language.as_deref(), Some("tr"));
        // The format's own Referer overrides the request's, as yt-dlp sends it; cookies are
        // matched per segment address instead of copied from the format.
        assert_eq!(
            streams[1].headers,
            [
                ("Origin".to_owned(), "https://embed.example".to_owned()),
                ("Referer".to_owned(), "https://site.example/".to_owned()),
            ]
        );
        assert_eq!(streams[0].headers[1].1, "https://embed.example/");
    }

    #[test]
    fn other_source_shapes_and_selections_stay_with_yt_dlp() {
        let video = AddRequest {
            kind: DownloadKind::Video,
            ..AddRequest::default()
        };
        let declined = |source: &PinnedMediaSource, request: &AddRequest| {
            native_streams(source, request).is_none()
        };
        for request in [
            AddRequest {
                kind: DownloadKind::Audio,
                ..AddRequest::default()
            },
            AddRequest {
                playlist: true,
                ..video.clone()
            },
            AddRequest {
                live_from_start: true,
                ..video.clone()
            },
            AddRequest {
                audio_tracks: vec![AudioSelection::default()],
                ..video.clone()
            },
            AddRequest {
                subtitle_languages: vec!["tr".into()],
                ..video.clone()
            },
            AddRequest {
                subtitle_tracks: vec![SubtitleSelection::default()],
                ..video.clone()
            },
        ] {
            assert!(declined(&separate(), &request));
        }
        let mut dash = separate();
        dash.dash_formats.push(PinnedDashFormat {
            entry_index: None,
            format_id: "dash".into(),
        });
        assert!(declined(&dash, &video));
        let mut single = separate();
        single.manifests.pop();
        assert!(declined(&single, &video));
        let muxed = pinned(
            json!({"format_id": "720", "url": "pinned-0", "vcodec": "avc1", "acodec": "mp4a"}),
            json!({"format_id": "aud", "url": "pinned-1", "vcodec": "none", "acodec": "mp4a"}),
        );
        assert!(declined(&muxed, &video));
        let mut encrypted = separate();
        encrypted.manifests[1].body = PLAIN.replace(
            "#EXTINF:6.0,",
            "#EXT-X-KEY:METHOD=AES-128,URI=\"https://cdn.example/key\"\n#EXTINF:6.0,",
        );
        assert!(declined(&encrypted, &video));
        let mut unsafe_header = separate();
        unsafe_header.inspection["formats"][1]["http_headers"]["Referer"] = json!("a\r\nX: y");
        assert!(declined(&unsafe_header, &video));
        // A cookie the extractor got from the site itself is not sent by the app.
        let mut site_cookie = separate();
        site_cookie.inspection["formats"][1]["cookies"] =
            json!("sid=1; Domain=.cdn.example; Path=/; Secure; Expires=1900000000");
        assert!(declined(&site_cookie, &video));
        let session = AddRequest {
            session_cookies: vec![ScopedCookie {
                name: "sid".into(),
                ..ScopedCookie::default()
            }],
            ..video.clone()
        };
        assert!(!declined(&site_cookie, &session));
        assert!(!declined(&separate(), &video));
    }

    fn single(format: Value) -> PinnedMediaSource {
        PinnedMediaSource {
            inspection: json!({ "formats": [format] }),
            manifests: vec![PinnedManifest {
                marker: "pinned-0".into(),
                body: PLAIN.into(),
            }],
            dash_formats: Vec::new(),
        }
    }

    #[test]
    fn an_audio_download_takes_the_one_playlist_that_carries_audio() {
        let audio = AddRequest {
            kind: DownloadKind::Audio,
            audio_format: Some("mp3".into()),
            ..AddRequest::default()
        };
        let rendition = json!({"format_id": "aud", "url": "pinned-0", "ext": "mp4",
                               "vcodec": "none", "acodec": null});
        let muxed = json!({"format_id": "720", "url": "pinned-0", "ext": "mp4",
                           "vcodec": "avc1", "acodec": "mp4a"});
        let taken = |source: &PinnedMediaSource, request: &AddRequest| {
            native_streams(source, request).map(|streams| {
                streams
                    .iter()
                    .map(|stream| (stream.role, stream.length))
                    .collect::<Vec<_>>()
            })
        };
        // An audio rendition, and a playlist that holds the video too, which the conversion
        // drops; both with the length the playlist declares.
        assert_eq!(
            taken(&single(rendition.clone()), &audio),
            Some(vec![("audio", Some(10.0))])
        );
        assert_eq!(
            taken(&single(muxed.clone()), &audio),
            Some(vec![("audio", Some(10.0))])
        );

        // A playlist without audio stays with yt-dlp, as does a playlist whose length cannot be
        // read.
        let mut silent = muxed.clone();
        silent["acodec"] = json!("none");
        assert_eq!(taken(&single(silent), &audio), None);
        let mut unreadable = single(rendition.clone());
        unreadable.manifests[0].body = PLAIN.replace("#EXTINF:4.0,", "#EXTINF:four,");
        assert_eq!(taken(&unreadable, &audio), None);

        // So does a format yt-dlp can keep as it comes: the best audio of a common audio file
        // type, and a file of the asked type. A file of another type is converted alike by both.
        let of_type = |ext: &str| {
            let mut format = rendition.clone();
            format["ext"] = json!(ext);
            single(format)
        };
        let asked = |format: &str| AddRequest {
            audio_format: Some(format.into()),
            ..audio.clone()
        };
        assert_eq!(taken(&of_type("m4a"), &asked("original")), None);
        assert_eq!(taken(&of_type("mp3"), &asked("mp3")), None);
        assert_eq!(taken(&of_type("m4a"), &asked("m4a")), None);
        assert_eq!(
            taken(&of_type("m4a"), &asked("mp3")),
            Some(vec![("audio", Some(10.0))])
        );
        assert_eq!(
            taken(&of_type("mp4"), &asked("original")),
            Some(vec![("audio", Some(10.0))])
        );

        // So does an audio download of a playlist, of a live stream from its start, or with
        // subtitles.
        for request in [
            AddRequest {
                playlist: true,
                ..audio.clone()
            },
            AddRequest {
                live_from_start: true,
                ..audio.clone()
            },
            AddRequest {
                subtitle_languages: vec!["tr".into()],
                ..audio.clone()
            },
            AddRequest {
                subtitle_tracks: vec![SubtitleSelection::default()],
                ..audio.clone()
            },
        ] {
            assert_eq!(taken(&single(rendition.clone()), &request), None);
        }
        // So does an audio download of two playlists, a video download of one, and a download
        // whose kind is decided when it starts.
        assert_eq!(taken(&separate(), &audio), None);
        let video = AddRequest {
            kind: DownloadKind::Video,
            ..AddRequest::default()
        };
        assert_eq!(taken(&single(muxed.clone()), &video), None);
        let automatic = AddRequest {
            kind: DownloadKind::Auto,
            ..AddRequest::default()
        };
        assert_eq!(taken(&single(rendition), &automatic), None);
        assert_eq!(taken(&separate(), &automatic), None);
    }

    #[test]
    fn the_audio_is_converted_as_yt_dlp_converts_it() {
        let options = |conversion: Option<&str>, codec: &str| {
            let output = audio_output(conversion, codec);
            (output.extension, output.codec.join(" "), output.encodes)
        };
        // The options yt-dlp 2026.10.26 ran for an AAC source (`--verbose`, the app's arguments).
        assert_eq!(
            options(Some("mp3"), "aac"),
            ("mp3", "-acodec libmp3lame -q:a 0.0".into(), true)
        );
        for conversion in [None, Some("m4a")] {
            assert_eq!(
                options(conversion, "aac"),
                ("m4a", "-acodec copy -bsf:a aac_adtstoasc".into(), false)
            );
        }
        assert_eq!(
            options(Some("opus"), "aac"),
            ("opus", "-acodec libopus".into(), true)
        );
        assert_eq!(
            options(Some("flac"), "aac"),
            ("flac", "-acodec flac".into(), true)
        );
        // Its `FFmpegExtractAudioPP` for other codecs: a codec its own file type takes is copied
        // for the best audio and for a conversion to that codec; another is encoded, AAC at
        // quality 5 of 10, and the best audio of a codec without a file type becomes MP3 at 5.
        assert_eq!(
            options(None, "opus"),
            ("opus", "-acodec copy".into(), false)
        );
        assert_eq!(
            options(None, "vorbis"),
            ("ogg", "-acodec copy".into(), false)
        );
        assert_eq!(
            options(Some("mp3"), "mp3"),
            ("mp3", "-acodec copy".into(), false)
        );
        assert_eq!(
            options(Some("m4a"), "opus"),
            ("m4a", "-acodec aac -q:a 2.05".into(), true)
        );
        assert_eq!(
            options(Some("m4a"), "alac"),
            ("m4a", "-acodec aac -q:a 2.05".into(), true)
        );
        assert_eq!(
            options(None, "ac3"),
            ("mp3", "-acodec libmp3lame -q:a 5.0".into(), true)
        );
    }

    #[test]
    fn a_conversion_reports_the_share_written_and_the_time_left_but_not_more_bytes() {
        let mp3 = audio_output(Some("mp3"), "aac");
        let report = |written: f64, elapsed: f64| {
            conversion_progress(
                &mp3,
                1000,
                Duration::from_secs_f64(written),
                200.0,
                Duration::from_secs_f64(elapsed),
            )
        };
        let start = report(0.0, 0.0);
        assert_eq!(
            (start.downloaded, start.total, start.speed, start.eta),
            (1000, Some(1000), 0, None)
        );
        assert!(start.processing);
        assert_eq!(start.phase, "Ses MP3 biçimine dönüştürülüyor: %0");
        // A quarter written in 4 s leaves 12 s; under a second in, the rate says too little.
        let quarter = report(50.0, 4.0);
        assert_eq!(quarter.phase, "Ses MP3 biçimine dönüştürülüyor: %25");
        assert_eq!(quarter.eta, Some(12));
        assert_eq!(report(50.0, 0.5).eta, None);
        // FFmpeg's last report can run past the playlist's length, also far past a playlist that
        // declares too little.
        let end = report(200.04, 16.0);
        assert_eq!(end.phase, "Ses MP3 biçimine dönüştürülüyor: %100");
        assert_eq!((end.downloaded, end.eta), (1000, Some(0)));
        let beyond = report(250.0, 20.0);
        assert_eq!(beyond.phase, "Ses MP3 biçimine dönüştürülüyor: %100");
        assert_eq!(beyond.eta, Some(0));
        let copy = conversion_progress(
            &audio_output(None, "aac"),
            1000,
            Duration::from_secs(100),
            200.0,
            Duration::from_secs(1),
        );
        assert_eq!(copy.phase, "Ses M4A dosyasına kopyalanıyor: %50");
    }

    #[test]
    fn a_conversion_that_runs_out_of_time_says_so() {
        // The error `run_capture` gives once FFmpeg has run for its whole timeout.
        let timeout = crate::error_codes::coded(
            crate::error_codes::MED_011,
            "Medya analizi 600 saniye içinde tamamlanmadı",
        );
        assert_eq!(
            unfinished_conversion(timeout, Duration::from_secs(600)).to_string(),
            "Ses dönüştürmesi 600 saniye içinde tamamlanmadı"
        );
        // A pause or cancel keeps its own error.
        let stopped = anyhow!("Kaynak hazırlığı durduruldu");
        assert_eq!(
            unfinished_conversion(stopped, Duration::from_secs(600)).to_string(),
            "Kaynak hazırlığı durduruldu"
        );
    }

    /// The segment `next_task` hands to a worker that keeps a connection to `kept`.
    fn task_for(shared: &Shared, streams: &[NativeStream], kept: Option<usize>) -> Task {
        match next_for(shared, streams, kept) {
            Some(Next::Fetch(task)) => task,
            _ => panic!("no segment was handed out"),
        }
    }

    /// What `next_task` tells a worker that keeps a connection to `kept` while nobody else
    /// waits for a place of any host.
    fn next_for(shared: &Shared, streams: &[NativeStream], kept: Option<usize>) -> Option<Next> {
        shared.next_task(
            streams,
            &TransferControl::default(),
            &NetworkGovernor::new(8),
            kept,
        )
    }

    fn one_host_stream(segments: usize) -> NativeStream {
        NativeStream {
            role: "audio",
            format_id: "a".into(),
            language: None,
            headers: Vec::new(),
            segments: (0..segments)
                .map(|index| Url::parse(&format!("https://cdn.example/{index}.ts")).unwrap())
                .collect(),
            length: None,
        }
    }

    /// Asserts that `rate` is paced at `expected` requests per second.
    fn assert_rate(rate: Option<f64>, expected: f64) {
        let rate = rate.expect("the host is paced");
        assert!(
            (rate - expected).abs() < 1e-9,
            "{rate} instead of {expected}"
        );
    }

    #[test]
    fn a_host_is_paced_from_its_first_refusal_and_a_burst_of_refusals_cuts_once() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let mut pace = Pace::new(start);
        // Until the host refuses one, its requests start as fast as they come.
        for _ in 0..20 {
            pace.start(at(0));
        }
        assert_eq!(pace.next_start, start);
        for index in 0..20 {
            pace.admit(at(0), at(50 * index));
        }

        // A second after the first request, a refusal sets the rate below the 20 per second
        // the host admitted.
        pace.refuse(at(900), None, at(1000));
        assert_rate(pace.rate, PACE_CUT * 20.0);
        // The other requests of that burst answer for the rate it replaced: a refusal leaves it
        // as it is, and one the host admitted counts toward it.
        pace.refuse(at(950), None, at(1010));
        assert_rate(pace.rate, PACE_CUT * 20.0);
        pace.admit(at(960), at(1015));
        assert_rate(pace.rate, PACE_CUT * 21.0);

        // Requests now start one interval apart. One that starts later than its turn leaves the
        // next no more than a quarter of an interval in hand; one on its turn leaves none.
        pace.start(at(1020));
        let interval = Duration::from_secs_f64(1.0 / (PACE_CUT * 21.0));
        assert_eq!(pace.next_start, at(1020) + (interval - interval / 4));
        let turn = pace.next_start;
        pace.start(turn);
        assert_eq!(pace.next_start, turn + interval);
        // An admitted request raises the rate while the pace holds the next one back. A late
        // refusal from the first burst still leaves the rate, and a refusal of a request sent
        // after the cut cuts again.
        pace.admit(at(1020), at(1060));
        let grown = PACE_CUT * 21.0 + PACE_GROWTH * (1.0 + 0.06 / PACE_SPEEDUP.as_secs_f64());
        assert_rate(pace.rate, grown);
        pace.refuse(at(990), None, at(1065));
        assert_rate(pace.rate, grown);
        pace.refuse(at(1030), None, at(1070));
        assert_rate(pace.rate, grown * PACE_CUT);
        // From then on a late answer to the first burst no longer counts, and once the pace no
        // longer holds requests back, an admitted request leaves the rate as it is.
        let free = turn + interval + Duration::from_millis(1);
        pace.admit(at(970), free);
        assert_rate(pace.rate, grown * PACE_CUT);
        pace.admit(at(1071), at(2000));
        assert_rate(pace.rate, grown * PACE_CUT);
    }

    #[test]
    fn a_host_that_admits_nothing_after_a_cut_gets_ever_longer_waits_and_its_retry_after() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let mut pace = Pace::new(start);
        pace.start(at(0));
        for _ in 0..10 {
            pace.admit(at(0), at(100));
        }
        // The first refusal's Retry-After holds its own segment, not the host.
        pace.refuse(at(900), Some(Duration::from_secs(30)), at(1000));
        assert_rate(pace.rate, PACE_CUT * 10.0);
        assert!(pace.next_start <= at(1000));

        // One refusal of a request sent after the cut, with nothing admitted, changes nothing.
        pace.refuse(at(1001), None, at(1050));
        assert_rate(pace.rate, PACE_CUT * 10.0);
        // A second cuts the rate and makes the next request wait two intervals.
        pace.refuse(at(1002), None, at(1060));
        let rate = PACE_CUT * 10.0 * PACE_CUT;
        assert_rate(pace.rate, rate);
        assert_eq!(pace.lockout, 1);
        assert_eq!(
            pace.next_start,
            at(1060) + Duration::from_secs_f64(2.0 / rate)
        );
        // Until the host admits a request again, a Retry-After holds every request to it, also
        // one that answers a request sent before the cut.
        pace.refuse(at(1003), Some(Duration::from_secs(30)), at(1070));
        assert_eq!(pace.next_start, at(1070) + Duration::from_secs(30));

        // Each further lockout doubles the wait, never beyond the safety bound, and the rate
        // stops at one request per bound.
        let mut now = 31_000;
        for _ in 0..60 {
            pace.refuse(at(now), None, at(now + 1));
            pace.refuse(at(now), None, at(now + 2));
            now += 20_000;
        }
        assert_rate(pace.rate, 1.0 / MAX_BACKOFF.as_secs_f64());
        assert!(pace.next_start <= at(now - 20_000 + 2) + MAX_BACKOFF);
        pace.admit(at(now), at(now + 1));
        assert_eq!(pace.lockout, 0);
    }

    #[test]
    fn streams_on_one_host_advance_together_and_share_its_pace() {
        let root = std::env::temp_dir().join(format!("ssdownload-fair-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let streams = [
            NativeStream {
                role: "video",
                ..one_host_stream(8)
            },
            one_host_stream(4),
        ];
        let joins = [
            Join::resume(&root, &streams[0]).unwrap(),
            Join::resume(&root, &streams[1]).unwrap(),
        ];
        let shared = Shared::new(&streams, &joins, 4);
        let host = 0;

        let tasks = (0..4)
            .map(|_| task_for(&shared, &streams, None))
            .collect::<Vec<_>>();
        // Each stream advances through its own playlist at the same rate, ties to the first.
        assert_eq!(
            tasks
                .iter()
                .map(|task| (task.stream, task.index))
                .collect::<Vec<_>>(),
            [(0, 0), (1, 0), (0, 1), (0, 2)]
        );
        assert_eq!(shared.lock().busy[host], 4);

        // The video's 429 paces the host, for the audio too.
        shared.settle(
            host,
            Settled::Retry {
                task: tasks[0],
                throttled: true,
                retry_after: None,
            },
        );
        for task in &tasks[1..] {
            shared.settle(host, Settled::Done { sent: task.sent });
        }
        shared.lock().paces[host].rate = Some(4.0);
        // The video's retry is furthest behind and goes first; the audio's next segment waits
        // for the pace, at least three quarters of its quarter of a second.
        let first = task_for(&shared, &streams, Some(host));
        let due = shared.lock().paces[host].next_start;
        let second = task_for(&shared, &streams, Some(host));
        assert_eq!((first.stream, first.index), (0, 0));
        assert_eq!((second.stream, second.index), (1, 1));
        assert!(due >= first.sent + Duration::from_millis(187));
        assert!(second.sent >= due);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_worker_waiting_only_for_its_hosts_pace_keeps_its_connection_unless_another_waits() {
        let root = std::env::temp_dir().join(format!("ssdownload-paced-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let streams = [one_host_stream(3)];
        let joins = [Join::resume(&root, &streams[0]).unwrap()];
        let shared = Shared::new(&streams, &joins, 2);
        let first = task_for(&shared, &streams, None);
        shared.settle(0, Settled::Done { sent: first.sent });
        let due = Instant::now() + Duration::from_millis(250);
        {
            let mut state = shared.lock();
            state.paces[0].rate = Some(4.0);
            state.paces[0].next_start = due;
        }

        // With nobody else waiting for the host, the worker waits for the pace and keeps its
        // connection.
        let second = task_for(&shared, &streams, Some(0));
        assert!(second.sent >= due);
        assert_eq!(second.index, 1);
        assert_eq!(shared.lock().holders, [1]);
        shared.settle(0, Settled::Done { sent: second.sent });

        // A requester waiting for a place of the host makes it close the connection instead.
        let network = NetworkGovernor::new(1);
        let mut held = network.try_acquire_host("cdn.example".into());
        thread::scope(|scope| {
            let tunnel = scope.spawn(|| {
                network.acquire_host_for_tunnel("cdn.example".into(), Duration::from_secs(5), 0)
            });
            let deadline = Instant::now() + Duration::from_secs(5);
            while !network.host_has_waiters("cdn.example") {
                assert!(Instant::now() < deadline, "the requester does not wait");
                thread::sleep(Duration::from_millis(1));
            }
            shared.lock().paces[0].next_start = Instant::now() + Duration::from_secs(30);
            assert!(matches!(
                shared.next_task(&streams, &TransferControl::default(), &network, Some(0)),
                Some(Next::Release)
            ));
            assert_eq!(shared.lock().holders, [0]);
            held.take();
            assert!(tunnel.join().unwrap().is_some());
        });
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_refused_segment_goes_again_twice_with_the_pace_and_then_backs_off_while_others_pass() {
        let root =
            std::env::temp_dir().join(format!("ssdownload-backoff-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let streams = [one_host_stream(3)];
        let joins = [Join::resume(&root, &streams[0]).unwrap()];
        let shared = Shared::new(&streams, &joins, 8);
        let mut state = shared.lock();
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let take = |state: &mut Schedule, now| {
            state
                .take(&shared.hosts, &streams, None, now)
                .unwrap_or_else(|_| panic!("no segment was handed out"))
        };
        let retry = |task: Task, throttled, retry_after| Settled::Retry {
            task: Task {
                attempt: task.attempt + 1,
                ..task
            },
            throttled,
            retry_after,
        };
        let not_before = |state: &Schedule, index| {
            state.queues[0]
                .iter()
                .find(|task| task.index == index)
                .unwrap()
                .not_before
        };
        let (first, second) = (take(&mut state, at(0)), take(&mut state, at(0)));

        // A refused segment goes again with the host's next paced request after its first two
        // refusals; an ordinary failure waits out the backoff of its attempts.
        state.settle(0, retry(first, true, None), at(50));
        assert_eq!(not_before(&state, 0), at(50));
        let second = Task {
            attempt: 2,
            ..second
        };
        state.settle(0, retry(second, false, None), at(60));
        assert_eq!(not_before(&state, 1), at(60) + Duration::from_secs(4));
        let (first, third) = (take(&mut state, at(60)), take(&mut state, at(110)));
        assert_eq!((first.index, third.index), (0, 2));
        state.settle(0, retry(first, true, None), at(110));
        assert_eq!(not_before(&state, 0), at(110));

        // The host admits another segment, so it is not locked out. The refused segment's third
        // refusal still waits out the backoff of one failure, and the next ones that of two and
        // three, also when their Retry-After is shorter; a longer one holds it longer.
        state.settle(0, Settled::Done { sent: third.sent }, at(160));
        let first = take(&mut state, at(160));
        assert_eq!(first.index, 0);
        state.settle(0, retry(first, true, None), at(210));
        assert_eq!(state.paces[0].lockout, 0);
        assert_eq!(not_before(&state, 0), at(1210));
        let first = take(&mut state, at(1210));
        state.settle(
            0,
            retry(first, true, Some(Duration::from_secs(1))),
            at(1260),
        );
        assert_eq!(not_before(&state, 0), at(3260));
        let first = take(&mut state, at(3260));
        state.settle(
            0,
            retry(first, true, Some(Duration::from_secs(10))),
            at(3310),
        );
        assert_eq!(not_before(&state, 0), at(13_310));
        drop(state);
        fs::remove_dir_all(root).unwrap();
    }

    /// Runs the schedule of one stream of `segments` segments on one host in virtual time, with
    /// `workers` workers, as the workers and `Shared::next_task` do:
    /// - each answer comes `latency` after its request and up to 40 ms later, by a sequence that
    ///   is the same in every run;
    /// - an admitted segment settles 1 ms after its answer, once its file is written;
    /// - a worker with nothing to take waits until a request settles or its wait of 10 to 200 ms
    ///   ends, at the next 15.6 ms timer tick.
    ///
    /// `host` decides each request as it is sent, from the segment, the time since the start and
    /// that of its answer: `Ok` admits it, and `Err` refuses it with a 429 and the `Retry-After`
    /// given. Returns how long the download took and how many requests were refused, or the
    /// segment that used up its attempts and when.
    fn simulate(
        segments: usize,
        workers: usize,
        latency: Duration,
        mut host: impl FnMut(usize, Duration, Duration) -> Result<(), Option<Duration>>,
    ) -> Result<(Duration, usize), (usize, Duration)> {
        let root =
            std::env::temp_dir().join(format!("ssdownload-virtual-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let streams = [one_host_stream(segments)];
        let joins = [Join::resume(&root, &streams[0]).unwrap()];
        let shared = Shared::new(&streams, &joins, workers);
        drop(joins);
        fs::remove_dir_all(root).unwrap();
        let mut state = shared.lock();
        let tick = Duration::from_micros(15_625);
        let start = Instant::now();
        let mut now = start;
        let mut refused = 0;
        let mut seed = 1u32;
        // Each worker's request in flight, with when it settles and what the host answered.
        let mut flying = vec![None; workers];
        while state.remaining > 0 {
            let mut wake = None;
            for slot in flying.iter_mut().filter(|slot| slot.is_none()) {
                match state.take(&shared.hosts, &streams, Some(0), now) {
                    Ok(task) => {
                        seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                        let answer =
                            now + latency + Duration::from_millis(u64::from(seed >> 16) % 41);
                        let verdict = host(task.index, now - start, answer - start);
                        let written = Duration::from_millis(u64::from(verdict.is_ok()));
                        *slot = Some((task, answer + written, verdict));
                    }
                    Err((earliest, _)) => {
                        let wait = earliest
                            .map_or(Duration::from_millis(200), |at| {
                                at.saturating_duration_since(now)
                            })
                            .clamp(Duration::from_millis(10), Duration::from_millis(200));
                        let ticks = (now + wait - start).as_nanos().div_ceil(tick.as_nanos());
                        wake = Some(start + tick * u32::try_from(ticks).unwrap());
                        break;
                    }
                }
            }
            let settles = flying.iter().flatten().map(|(_, at, _)| *at).min();
            now = settles
                .into_iter()
                .chain(wake)
                .min()
                .expect("nothing left to wait for");
            for slot in &mut flying {
                let Some((task, at, verdict)) = slot.filter(|(_, at, _)| *at <= now) else {
                    continue;
                };
                *slot = None;
                let outcome = match verdict {
                    Ok(()) => Settled::Done { sent: task.sent },
                    Err(retry_after) if task.attempt + 1 < SEGMENT_ATTEMPTS => {
                        refused += 1;
                        Settled::Retry {
                            task: Task {
                                attempt: task.attempt + 1,
                                ..task
                            },
                            throttled: true,
                            retry_after,
                        }
                    }
                    Err(_) => return Err((task.index, at - start)),
                };
                state.settle(0, outcome, at);
            }
        }
        Ok((now - start, refused))
    }

    #[test]
    fn a_download_outlasts_a_host_that_refuses_everything_as_long_as_30_tries_back_off() {
        // The backoff between 30 tries, 1, 2, 4 and 8 s and then 15 s, spans 390 s. A segment
        // its host refuses for that long still has a try left after it.
        let outage = Duration::from_secs(390);
        for segments in [1, 40] {
            let refused_until_the_end =
                |_, sent: Duration, _| if sent < outage { Err(None) } else { Ok(()) };
            match simulate(
                segments,
                8,
                Duration::from_millis(50),
                refused_until_the_end,
            ) {
                Ok((took, _)) => assert!(took >= outage),
                Err((index, at)) => {
                    panic!("of {segments}, segment {index} used up its attempts after {at:?}")
                }
            }
        }
    }

    #[test]
    fn a_segment_refused_while_the_others_are_admitted_lasts_as_long_as_30_tries_back_off() {
        // However the host answers the other segments, the attempts of a segment it keeps
        // refusing span the 390 s of the backoff between 30 tries, also when its Retry-After
        // is shorter than the backoff.
        for retry_after in [None, Some(Duration::from_secs(1))] {
            let refused_alone = |index, _, _| if index == 0 { Err(retry_after) } else { Ok(()) };
            let (index, at) = simulate(200, 8, Duration::from_millis(50), refused_alone)
                .expect_err("the host never admits the first segment");
            assert_eq!(index, 0);
            assert!(at >= Duration::from_secs(390), "{retry_after:?}: {at:?}");
        }
    }

    #[test]
    fn a_refusal_paces_only_its_own_host() {
        let root = std::env::temp_dir().join(format!("ssdownload-apart-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let audio = NativeStream {
            segments: (0..4)
                .map(|index| Url::parse(&format!("https://audio.example/{index}.ts")).unwrap())
                .collect(),
            ..one_host_stream(4)
        };
        let streams = [
            NativeStream {
                role: "video",
                ..one_host_stream(4)
            },
            audio,
        ];
        let joins = [
            Join::resume(&root, &streams[0]).unwrap(),
            Join::resume(&root, &streams[1]).unwrap(),
        ];
        let shared = Shared::new(&streams, &joins, 2);
        let (video, audio) = (0, 1);
        let tasks = (0..2)
            .map(|_| task_for(&shared, &streams, None))
            .collect::<Vec<_>>();
        assert_eq!(
            tasks.iter().map(|task| task.stream).collect::<Vec<_>>(),
            [video, audio]
        );
        // The audio host's 429 paces that host alone. The audio's retry is furthest behind and
        // goes first; the video's next request starts at once and leaves the one after it free
        // to start at once too.
        shared.settle(
            audio,
            Settled::Retry {
                task: Task {
                    attempt: 1,
                    ..tasks[1]
                },
                throttled: true,
                retry_after: None,
            },
        );
        let retried = task_for(&shared, &streams, Some(audio));
        assert_eq!((retried.stream, retried.index), (audio, 0));
        let next = task_for(&shared, &streams, Some(video));
        assert_eq!((next.stream, next.index), (video, 1));
        let state = shared.lock();
        assert!(state.paces[audio].rate.is_some());
        assert!(state.paces[video].rate.is_none());
        assert!(state.paces[video].next_start <= next.sent);
        drop(state);
        fs::remove_dir_all(root).unwrap();
    }

    /// What a limited host saw of a download, on the loopback server or in virtual time.
    #[derive(Debug)]
    struct Served {
        admitted: usize,
        refused: usize,
        /// Seconds from the first request's arrival to the last answer.
        span: f64,
        /// Seconds from arrival to answer of every admitted request, added up.
        serving: f64,
    }

    /// Fetches `segments` segments of 2000 bytes over four connections from a loopback server
    /// that answers every request after 40 ms and admits them as `limit` allows. A download that
    /// has not finished after a minute fails the test.
    fn fetch_limited(limit: crate::test_http::RequestLimit, segments: usize) -> Served {
        use crate::test_http::{pattern, Config, Logged, Server, TempDir};
        let server = Server::start(Config {
            size: 2000,
            limit: Some(limit),
            latency: Some(Duration::from_millis(40)),
            ..Config::default()
        });
        let workspace = TempDir::new("native-limited");
        let streams = [NativeStream {
            segments: (0..segments)
                .map(|index| Url::parse(&server.url(&format!("/a/{index}.ts"))).unwrap())
                .collect(),
            ..one_host_stream(0)
        }];
        let request = AddRequest {
            connections: Some(4),
            ..AddRequest::default()
        };
        let (control, network) = (TransferControl::default(), NetworkGovernor::new(8));
        let (sender, receiver) = std::sync::mpsc::channel();
        thread::scope(|scope| {
            scope.spawn(|| {
                let _ = sender.send(fetch_segments(
                    &streams,
                    &workspace.0,
                    &request,
                    &control,
                    &network,
                    &mut |_| {},
                ));
            });
            let fetched = receiver.recv_timeout(Duration::from_secs(60));
            // A stalled download is stopped so that the scope can end.
            control.cancel();
            let joins = fetched.expect("the download stalled").unwrap();
            assert_eq!(
                fs::read(&joins[0].joined).unwrap(),
                pattern(1, 2000).repeat(segments)
            );
        });
        let log = server.log();
        let answered = |entry: &Logged| entry.answered.expect("every request was answered");
        let first = log.iter().map(|entry| entry.arrived).min().unwrap();
        let last = log.iter().map(answered).max().unwrap();
        let admitted = log.iter().filter(|entry| entry.status == 200);
        let served = Served {
            admitted: admitted.clone().count(),
            refused: log.iter().filter(|entry| entry.status != 200).count(),
            span: (last - first).as_secs_f64(),
            serving: admitted
                .map(|entry| (answered(entry) - entry.arrived).as_secs_f64())
                .sum(),
        };
        assert_eq!(served.admitted, segments);
        served
    }

    /// Runs the download `fetch_limited` makes in virtual time (see `simulate`), from a host that
    /// answers every request after at least 40 ms and admits them as `limit` allows.
    fn simulate_limited(limit: &crate::test_http::RequestLimit, segments: usize) -> Served {
        use crate::test_http::{retry_after_seconds, Limit, Limiter};
        let origin = Instant::now();
        let mut limiter = Limiter::new(Some(limit.kind), origin);
        // When the admitted requests in flight are answered, for a limit on requests in flight.
        let mut answers = Vec::new();
        let mut serving = Duration::ZERO;
        let host = |_, sent: Duration, answered: Duration| {
            let before = answers.len();
            answers.retain(|at| *at > sent);
            for _ in answers.len()..before {
                limiter.answered();
            }
            let wait = limiter.admit(limit.kind, origin + sent).err();
            if wait.is_none() {
                serving += answered - sent;
                if matches!(limit.kind, Limit::InFlight { .. }) {
                    answers.push(answered);
                }
            }
            wait.map_or(Ok(()), |wait| {
                Err(limit
                    .retry_after
                    .then(|| Duration::from_secs(retry_after_seconds(wait))))
            })
        };
        let (took, refused) = simulate(segments, 4, Duration::from_millis(40), host)
            .unwrap_or_else(|(index, at)| {
                panic!("segment {index} used up its attempts after {at:?}")
            });
        Served {
            admitted: segments,
            refused,
            span: took.as_secs_f64(),
            serving: serving.as_secs_f64(),
        }
    }

    /// At most 20 requests in any second, the refused ones counted too, so a client that keeps
    /// asking while it is refused stays refused.
    fn sliding_window() -> crate::test_http::RequestLimit {
        crate::test_http::RequestLimit {
            kind: crate::test_http::Limit::SlidingWindow {
                requests: 20,
                window: Duration::from_secs(1),
            },
            status: 429,
            retry_after: false,
        }
    }

    fn two_in_flight() -> crate::test_http::RequestLimit {
        crate::test_http::RequestLimit {
            kind: crate::test_http::Limit::InFlight { requests: 2 },
            status: 503,
            retry_after: true,
        }
    }

    /// 20 requests a second after a burst of 10.
    fn token_bucket() -> crate::test_http::RequestLimit {
        crate::test_http::RequestLimit {
            kind: crate::test_http::Limit::TokenBucket {
                rate: 20.0,
                burst: 10.0,
            },
            status: 429,
            retry_after: true,
        }
    }

    #[test]
    fn a_host_that_counts_refused_requests_toward_its_limit_is_paced_below_it() {
        let served = simulate_limited(&sliding_window(), 100);
        // The window admits at most 20 requests in each second of the download and in the
        // second it ends in.
        let capacity = 20.0 * (served.span + 1.0);
        assert!(served.admitted as f64 >= 0.4 * capacity, "{served:?}");
        assert!(served.refused * 2 <= served.admitted, "{served:?}");
    }

    #[test]
    fn a_host_that_answers_two_requests_at_a_time_refuses_few_of_four_connections() {
        let served = simulate_limited(&two_in_flight(), 100);
        // The share of the download's time the host spent on two admitted requests at once.
        let busy = served.serving / (2.0 * served.span);
        assert!(busy >= 0.4, "{busy} busy, {served:?}");
        assert!(served.refused * 4 <= served.admitted, "{served:?}");
    }

    #[test]
    fn a_host_with_a_token_bucket_refuses_few_requests_and_stays_near_its_rate() {
        let served = simulate_limited(&token_bucket(), 100);
        let capacity = 10.0 + 20.0 * served.span;
        assert!(served.admitted as f64 >= 0.6 * capacity, "{served:?}");
        assert!(served.refused * 2 <= served.admitted, "{served:?}");
    }

    // On a real server the share of its capacity a download uses also depends on how fast this
    // machine gets its requests out, which a busy test runner slows down whatever the pace.
    // These check the refusals only.

    #[test]
    fn a_download_from_a_loopback_server_that_counts_refused_requests_is_seldom_refused() {
        let served = fetch_limited(sliding_window(), 100);
        assert!(served.refused * 2 <= served.admitted, "{served:?}");
    }

    #[test]
    fn a_download_from_a_loopback_server_that_answers_two_requests_at_a_time_is_seldom_refused() {
        let served = fetch_limited(two_in_flight(), 100);
        assert!(served.refused * 4 <= served.admitted, "{served:?}");
    }

    #[test]
    fn a_download_from_a_loopback_server_with_a_token_bucket_is_seldom_refused() {
        let served = fetch_limited(token_bucket(), 100);
        assert!(served.refused * 2 <= served.admitted, "{served:?}");
    }

    #[test]
    fn no_more_workers_keep_a_host_than_its_bound_and_they_let_go_once_it_is_done() {
        let root = std::env::temp_dir().join(format!("ssdownload-kept-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let audio = NativeStream {
            segments: (0..4)
                .map(|index| Url::parse(&format!("https://audio.example/{index}.ts")).unwrap())
                .collect(),
            ..one_host_stream(4)
        };
        let streams = [
            NativeStream {
                role: "video",
                ..one_host_stream(4)
            },
            audio,
        ];
        let joins = [
            Join::resume(&root, &streams[0]).unwrap(),
            Join::resume(&root, &streams[1]).unwrap(),
        ];
        let shared = Shared::new(&streams, &joins, 2);
        let (video, audio) = (0, 1);
        assert_eq!(shared.host_names, ["cdn.example", "audio.example"]);

        let tasks = (0..4)
            .map(|_| task_for(&shared, &streams, None))
            .collect::<Vec<_>>();
        assert_eq!(
            tasks
                .iter()
                .map(|task| (task.stream, task.index))
                .collect::<Vec<_>>(),
            [(0, 0), (1, 0), (0, 1), (1, 1)]
        );
        assert_eq!(shared.lock().holders, [2, 2]);
        shared.settle(
            video,
            Settled::Done {
                sent: tasks[0].sent,
            },
        );
        shared.settle(
            audio,
            Settled::Done {
                sent: tasks[1].sent,
            },
        );
        // The video host has a free request but both of its places are kept: the worker that
        // keeps the audio connection takes the next audio segment, though the video is as far.
        let next = task_for(&shared, &streams, Some(audio));
        assert_eq!((next.stream, next.index), (1, 2));
        assert_eq!(shared.lock().holders, [2, 2]);

        // The video's last segments come in; a worker that keeps that host lets it go then.
        shared.settle(
            video,
            Settled::Done {
                sent: tasks[2].sent,
            },
        );
        for index in [2, 3] {
            let task = task_for(&shared, &streams, Some(video));
            assert_eq!(task.index, index);
            shared.settle(video, Settled::Done { sent: task.sent });
        }
        assert!(matches!(
            next_for(&shared, &streams, Some(video)),
            Some(Next::Release)
        ));
        assert_eq!(shared.lock().holders, [1, 2]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_worker_with_nothing_to_take_at_once_closes_its_connection() {
        let root = std::env::temp_dir().join(format!("ssdownload-idle-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let streams = [one_host_stream(2)];
        let joins = [Join::resume(&root, &streams[0]).unwrap()];
        let shared = Shared::new(&streams, &joins, 2);
        let first = task_for(&shared, &streams, None);
        task_for(&shared, &streams, None);
        assert_eq!(shared.lock().holders, [2]);

        // The first segment waits out its retry backoff and the second is in flight.
        shared.settle(
            0,
            Settled::Retry {
                task: Task {
                    attempt: 1,
                    ..first
                },
                throttled: false,
                retry_after: None,
            },
        );
        // A failed request, unlike a 429 or 503, does not pace the host.
        assert!(shared.lock().paces[0].rate.is_none());
        assert!(matches!(
            next_for(&shared, &streams, Some(0)),
            Some(Next::Release)
        ));
        assert_eq!(shared.lock().holders, [1]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_download_finishes_while_other_connections_hold_part_of_its_host_bound() {
        use crate::test_http::{pattern, Config, Server, TempDir};
        let server = Server::start(Config {
            size: 2000,
            ..Config::default()
        });
        let workspace = TempDir::new("native-idle-release");
        let streams = [NativeStream {
            segments: (0..24)
                .map(|index| Url::parse(&server.url(&format!("/a/{index}.ts"))).unwrap())
                .collect(),
            ..one_host_stream(0)
        }];
        let request = AddRequest {
            connections: Some(3),
            ..AddRequest::default()
        };
        let network = NetworkGovernor::new(4);
        // Another download holds two of the host's four places, so one of the three workers
        // waits for a place that only a connection of this download can free.
        let _elsewhere = [(); 2].map(|_| network.try_acquire_host("127.0.0.1".into()).unwrap());
        let control = TransferControl::default();
        let (sender, receiver) = std::sync::mpsc::channel();

        thread::scope(|scope| {
            scope.spawn(|| {
                let _ = sender.send(fetch_segments(
                    &streams,
                    &workspace.0,
                    &request,
                    &control,
                    &network,
                    &mut |_| {},
                ));
            });
            let fetched = receiver.recv_timeout(Duration::from_secs(20));
            // A stalled download is stopped so that the scope can end.
            control.cancel();
            let joins = fetched.expect("the download stalled").unwrap();
            assert_eq!(
                fs::read(&joins[0].joined).unwrap(),
                pattern(1, 2000).repeat(24)
            );
        });
    }

    /// A stream of `segments` segments of 2000 bytes under `/<name>/` on `server`.
    fn stream_on(server: &crate::test_http::Server, name: &str, segments: usize) -> NativeStream {
        NativeStream {
            segments: (0..segments)
                .map(|index| Url::parse(&server.url(&format!("/{name}/{index}.ts"))).unwrap())
                .collect(),
            ..one_host_stream(0)
        }
    }

    /// Waits until `server` has logged `requests` requests.
    fn wait_for_requests(server: &crate::test_http::Server, requests: usize) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while server.log().len() < requests {
            assert!(Instant::now() < deadline, "the download did not start");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_connection_waiting_for_the_host_gets_a_place_while_a_download_goes_on() {
        use crate::test_http::{pattern, Config, Server, TempDir};
        // A segment takes about 0.1 s, so the download lasts about three seconds.
        let server = Server::start(Config {
            size: 2000,
            throttle: Some(20_000),
            ..Config::default()
        });
        let workspace = TempDir::new("native-waiter");
        let streams = [stream_on(&server, "a", 90)];
        let request = AddRequest {
            connections: Some(3),
            ..AddRequest::default()
        };
        let network = NetworkGovernor::new(4);
        // The download's three connections and this place fill the host's four.
        let _elsewhere = network.try_acquire_host("127.0.0.1".into()).unwrap();
        let control = TransferControl::default();

        thread::scope(|scope| {
            let download = scope.spawn(|| {
                fetch_segments(
                    &streams,
                    &workspace.0,
                    &request,
                    &control,
                    &network,
                    &mut |_| {},
                )
            });
            wait_for_requests(&server, 3);
            let connection = network.easy_for_url_before(
                &Url::parse(&server.url("/other")).unwrap(),
                &TransferControl::default(),
                Instant::now() + Duration::from_secs(2),
            );
            assert!(connection.is_ok(), "no place within 2 s");
            drop(connection);
            let joins = download.join().unwrap().unwrap();
            assert_eq!(
                fs::read(&joins[0].joined).unwrap(),
                pattern(1, 2000).repeat(90)
            );
        });
    }

    #[test]
    fn a_second_download_from_the_host_finishes_while_the_first_goes_on() {
        use crate::test_http::{Config, Server, TempDir};
        // A segment takes about 0.1 s, so the first download lasts about three seconds.
        let server = Server::start(Config {
            size: 2000,
            throttle: Some(20_000),
            ..Config::default()
        });
        let (first_workspace, second_workspace) =
            (TempDir::new("native-first"), TempDir::new("native-second"));
        let (first, second) = ([stream_on(&server, "a", 90)], [stream_on(&server, "b", 6)]);
        let request = AddRequest {
            connections: Some(3),
            ..AddRequest::default()
        };
        let network = NetworkGovernor::new(4);
        // The first download's three connections and this place fill the host's four.
        let _elsewhere = network.try_acquire_host("127.0.0.1".into()).unwrap();
        let control = TransferControl::default();

        thread::scope(|scope| {
            let first_download = scope.spawn(|| {
                fetch_segments(
                    &first,
                    &first_workspace.0,
                    &request,
                    &control,
                    &network,
                    &mut |_| {},
                )
            });
            wait_for_requests(&server, 3);
            let second_download = scope.spawn(|| {
                fetch_segments(
                    &second,
                    &second_workspace.0,
                    &request,
                    &control,
                    &network,
                    &mut |_| {},
                )
            });
            second_download.join().unwrap().unwrap();
            first_download.join().unwrap().unwrap();
        });
        let paths = server
            .log()
            .into_iter()
            .map(|entry| entry.path)
            .collect::<Vec<_>>();
        let last = |prefix: &str| {
            paths
                .iter()
                .rposition(|path| path.starts_with(prefix))
                .unwrap()
        };
        assert!(
            last("/b/") < last("/a/"),
            "the second download ended after the first"
        );
    }

    #[test]
    fn each_worker_keeps_its_connection_from_one_segment_to_the_next() {
        use crate::test_http::{pattern, Config, Server, TempDir};
        // Opening a connection takes as long as a distant server's handshakes.
        let server = Server::start(Config {
            size: 2000,
            connection_delay: Some(Duration::from_millis(50)),
            ..Config::default()
        });
        let workspace = TempDir::new("native-keep-alive");
        let streams = [NativeStream {
            segments: (0..48)
                .map(|index| Url::parse(&server.url(&format!("/a/{index}.ts"))).unwrap())
                .collect(),
            ..one_host_stream(0)
        }];
        let request = AddRequest {
            connections: Some(4),
            ..AddRequest::default()
        };

        let joins = fetch_segments(
            &streams,
            &workspace.0,
            &request,
            &TransferControl::default(),
            &NetworkGovernor::new(8),
            &mut |_| {},
        )
        .unwrap();

        assert_eq!(
            fs::read(&joins[0].joined).unwrap(),
            pattern(1, 2000).repeat(48)
        );
        let log = server.log();
        assert_eq!(log.len(), 48);
        // Four requests at a time, so four connections serve all of them.
        let connections = log
            .iter()
            .map(|entry| entry.connection)
            .collect::<std::collections::HashSet<_>>();
        assert!(connections.len() <= 4, "{} connections", connections.len());
    }

    #[test]
    fn a_resumed_join_drops_a_half_written_append_and_keeps_finished_segments() {
        let root = std::env::temp_dir().join(format!("ssdownload-join-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let stream = one_host_stream(4);
        let joined = root.join("media.native-audio.part");
        // Two segments joined, then an append cut short; segment 3 finished, segment 2 not.
        fs::write(&joined, b"0000111122").unwrap();
        fs::write(index_path(&joined), "2 8").unwrap();
        fs::write(segment_path(&joined, 0), b"stale").unwrap();
        fs::write(segment_path(&joined, 3), b"3333").unwrap();
        fs::write(partial_path(&segment_path(&joined, 2)), b"22").unwrap();

        let join = Join::resume(&root, &stream).unwrap();

        assert_eq!(fs::read(&joined).unwrap(), b"00001111");
        assert_eq!(join.pending(), [2]);
        assert_eq!(join.stored_bytes(), 12);
        assert!(!segment_path(&joined, 0).exists());
        assert!(!partial_path(&segment_path(&joined, 2)).exists());
        fs::write(segment_path(&joined, 2), b"2222").unwrap();
        join.mark_ready(2).unwrap();
        assert_eq!(fs::read(&joined).unwrap(), b"0000111122223333");
        assert_eq!(fs::read_to_string(index_path(&joined)).unwrap(), "4 16");
        assert!(!segment_path(&joined, 3).exists());
        fs::remove_dir_all(root).unwrap();
    }

    /// One worker fetches every segment of `streams` into `workspace`, as the workers of a
    /// download do before the merge; it is stopped after ten seconds. Returns the joins and the
    /// download's failure.
    fn work_alone(
        streams: &[NativeStream],
        cookies: &[ScopedCookie],
        workspace: &Path,
    ) -> (Vec<Join>, Option<anyhow::Error>) {
        let joins = streams
            .iter()
            .map(|stream| Join::resume(workspace, stream).unwrap())
            .collect::<Vec<_>>();
        let shared = Shared::new(streams, &joins, 1);
        let (control, network, received) = (
            TransferControl::default(),
            NetworkGovernor::new(8),
            AtomicU64::new(0),
        );
        let context = WorkerContext {
            streams,
            joins: &joins,
            shared: &shared,
            cookies,
            control: &control,
            network: &network,
            received: &received,
            workers: 1,
        };
        let (sender, receiver) = std::sync::mpsc::channel();
        thread::scope(|scope| {
            scope.spawn(|| {
                context.work();
                let _ = sender.send(());
            });
            let _ = receiver.recv_timeout(Duration::from_secs(10));
            control.cancel();
        });
        let failure = shared.take_failure();
        (joins, failure)
    }

    #[test]
    fn a_segment_redirect_the_transfer_rules_refuse_fails_the_download_at_once() {
        use crate::test_http::{Config, Document, Server, TempDir};
        let source = Server::start(Config {
            size: 2000,
            documents: vec![Document {
                path: "/a/0.ts".into(),
                status: 302,
                headers: vec!["Location: ftp://mirror.example/0.ts".into()],
                body: Vec::new(),
                unframed: false,
            }],
            ..Config::default()
        });
        let streams = [NativeStream {
            segments: vec![Url::parse(&source.url("/a/0.ts")).unwrap()],
            ..one_host_stream(0)
        }];
        let workspace = TempDir::new("native-refused-redirect");

        let (_, failure) = work_alone(&streams, &[], &workspace.0);

        let error = failure.expect("the refused redirect was retried");
        assert!(
            format!("{error:#}").contains(crate::i18n::ui(
                "Güvenli olmayan yönlendirme reddedildi",
                "Unsafe redirect rejected"
            )),
            "{error:#}"
        );
        assert_eq!(source.log().len(), 1);
    }

    fn moved(path: &str, location: String) -> crate::test_http::Document {
        crate::test_http::Document {
            path: path.into(),
            status: 302,
            headers: vec![format!("Location: {location}")],
            body: Vec::new(),
            unframed: false,
        }
    }

    /// The site headers and the session cookie `redirected_stream` sends where they apply.
    const SITE: [(&str, &str); 4] = [
        ("referer", "https://site.example/watch"),
        ("origin", "https://site.example"),
        ("x-site-token", "token"),
        ("cookie", "sid=session"),
    ];

    /// A stream of `segments` with site headers and a session cookie of the loopback host, which
    /// applies to each of its ports.
    fn redirected_stream(segments: Vec<Url>) -> (NativeStream, [ScopedCookie; 1]) {
        let stream = NativeStream {
            headers: [
                ("Referer", "https://site.example/watch"),
                ("Origin", "https://site.example"),
                ("X-Site-Token", "token"),
                ("Accept", "video/mp2t"),
                ("User-Agent", "Player/1.0"),
            ]
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
            .to_vec(),
            segments,
            ..one_host_stream(0)
        };
        let cookie = ScopedCookie {
            name: "sid".into(),
            value: "session".into(),
            domain: "127.0.0.1".into(),
            path: "/".into(),
            host_only: true,
            ..ScopedCookie::default()
        };
        (stream, [cookie])
    }

    #[test]
    fn a_redirected_segment_gets_site_headers_and_cookies_only_on_its_own_origin() {
        use crate::test_http::{pattern, Config, Server, TempDir};
        let logged = [
            "referer",
            "origin",
            "x-site-token",
            "cookie",
            "accept",
            "user-agent",
        ]
        .map(String::from)
        .to_vec();
        let elsewhere = Server::start(Config {
            size: 2000,
            logged_headers: logged.clone(),
            ..Config::default()
        });
        // A port is part of the origin, so the other server is another origin on the same host.
        let source = Server::start(Config {
            size: 2000,
            documents: vec![
                moved("/a/0.ts", elsewhere.url("/b/0.ts")),
                moved("/a/1.ts", "/b/1.ts".into()),
            ],
            logged_headers: logged,
            ..Config::default()
        });
        let (stream, cookies) = redirected_stream(
            ["/a/0.ts", "/a/1.ts"]
                .map(|path| Url::parse(&source.url(path)).unwrap())
                .to_vec(),
        );
        let workspace = TempDir::new("native-redirect");

        let (joins, failure) = work_alone(&[stream], &cookies, &workspace.0);

        if let Some(error) = failure {
            panic!("{error:#}");
        }
        assert_eq!(
            fs::read(&joins[0].joined).unwrap(),
            pattern(1, 2000).repeat(2)
        );
        let neutral = [("accept", "video/mp2t"), ("user-agent", "Player/1.0")];
        let source_log = source.log();
        assert_eq!(
            source_log
                .iter()
                .map(|entry| entry.path.as_str())
                .collect::<Vec<_>>(),
            ["/a/0.ts", "/a/1.ts", "/b/1.ts"]
        );
        for entry in &source_log {
            for (name, value) in SITE.iter().chain(&neutral) {
                assert_eq!(entry.header(name), Some(*value), "{} {name}", entry.path);
            }
        }
        let elsewhere_log = elsewhere.log();
        assert_eq!(elsewhere_log.len(), 1);
        for (name, _) in SITE {
            assert_eq!(elsewhere_log[0].header(name), None, "{name}");
        }
        for (name, value) in neutral {
            assert_eq!(elsewhere_log[0].header(name), Some(value), "{name}");
        }
    }

    #[test]
    fn a_redirect_chain_gets_site_headers_and_cookies_again_only_back_on_the_segments_origin() {
        use crate::test_http::{pattern, Config, Server, TempDir};
        let logged = ["referer", "origin", "x-site-token", "cookie"]
            .map(String::from)
            .to_vec();
        // The source's address is known before it starts, so the other server can send a
        // segment back to it.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let source_origin = format!("http://{}", listener.local_addr().unwrap());
        // The other origin sends one segment on within itself and the other back to the source.
        let elsewhere = Server::start(Config {
            size: 2000,
            documents: vec![
                moved("/b/0.ts", "/c/0.ts".into()),
                moved("/b/1.ts", format!("{source_origin}/a/back/1.ts")),
            ],
            logged_headers: logged.clone(),
            ..Config::default()
        });
        let source = Server::start_on(
            listener,
            Config {
                size: 2000,
                documents: vec![
                    moved("/a/0.ts", elsewhere.url("/b/0.ts")),
                    moved("/a/1.ts", elsewhere.url("/b/1.ts")),
                ],
                logged_headers: logged,
                ..Config::default()
            },
        );
        let (stream, cookies) = redirected_stream(
            ["/a/0.ts", "/a/1.ts"]
                .map(|path| Url::parse(&source.url(path)).unwrap())
                .to_vec(),
        );
        let workspace = TempDir::new("native-redirect-chain");

        let (joins, failure) = work_alone(&[stream], &cookies, &workspace.0);

        if let Some(error) = failure {
            panic!("{error:#}");
        }
        assert_eq!(
            fs::read(&joins[0].joined).unwrap(),
            pattern(1, 2000).repeat(2)
        );
        let paths = |log: &[crate::test_http::Logged]| {
            log.iter()
                .map(|entry| entry.path.clone())
                .collect::<Vec<_>>()
        };
        let source_log = source.log();
        assert_eq!(paths(&source_log), ["/a/0.ts", "/a/1.ts", "/a/back/1.ts"]);
        for entry in &source_log {
            for (name, value) in SITE {
                assert_eq!(entry.header(name), Some(value), "{} {name}", entry.path);
            }
        }
        let elsewhere_log = elsewhere.log();
        assert_eq!(paths(&elsewhere_log), ["/b/0.ts", "/c/0.ts", "/b/1.ts"]);
        for entry in &elsewhere_log {
            for (name, _) in SITE {
                assert_eq!(entry.header(name), None, "{} {name}", entry.path);
            }
        }
    }
}
