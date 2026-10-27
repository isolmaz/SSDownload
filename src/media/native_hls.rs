//! App-native download of a pinned, finished HLS source with separate video and audio
//! playlists. Both streams' segments are fetched at once, each segment is retried on its own,
//! a host that answers 429 or 503 gets fewer parallel requests, and the joined streams are
//! copied into one container. Every other source shape stays with yt-dlp.

use super::*;
use std::{
    collections::{HashMap, VecDeque},
    fs::File,
    io::{Seek, SeekFrom},
    sync::{
        atomic::{AtomicU64, Ordering},
        Condvar, Mutex,
    },
};

/// Tries per segment before the download fails; the joined part is kept for a retry.
const SEGMENT_ATTEMPTS: u32 = 30;
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
}

impl NativeStream {
    /// `<role>:<segments>` for the job log.
    pub(super) fn summary(&self) -> String {
        format!("{}:{}", self.role, self.segments.len())
    }
}

/// The pinned source as native streams: exactly one video-only and one audio-only finished
/// playlist, for a single-video request whose tracks need no per-track selection. `None`
/// leaves the download to yt-dlp.
pub(super) fn native_streams(
    pinned: &PinnedMediaSource,
    request: &AddRequest,
) -> Option<Vec<NativeStream>> {
    if request.playlist
        || request.kind != DownloadKind::Video
        || request.live_from_start
        || !request.audio_tracks.is_empty()
        || !request.subtitle_tracks.is_empty()
        || !request.subtitle_languages.is_empty()
        || !pinned.dash_formats.is_empty()
        || pinned.manifests.len() != 2
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
                _ => return None,
            };
            Some(NativeStream {
                role,
                format_id: codec("format_id")?.to_owned(),
                language: codec("language").map(str::to_owned),
                headers: stream_headers(format, request)?,
                segments: plain_segments(&manifest.body)?,
            })
        })
        .collect::<Option<Vec<_>>>()?;
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

/// Downloads the streams into the workspace and copies them into `media.<container>`.
pub(super) fn download_streams(
    streams: &[NativeStream],
    workspace: &Path,
    request: &AddRequest,
    tools: &tools::VerifiedTools,
    control: &TransferControl,
    network: &NetworkGovernor,
    progress: &mut dyn FnMut(TransferProgress),
) -> Result<PathBuf> {
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
    progress(TransferProgress {
        downloaded: received.load(Ordering::Relaxed),
        total: Some(received.load(Ordering::Relaxed)),
        phase: crate::i18n::ui("Medya işleniyor", "Processing media").into(),
        ..TransferProgress::default()
    });
    merge(streams, &joins, workspace, request, tools, control)
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
    let result = run_capture(
        &tools.ffmpeg,
        &browser_mux_args(request, &files, &staged),
        b"",
        Duration::from_secs(10 * 60),
        control,
    )?;
    if !result.status.success() || !result.stderr.trim().is_empty() {
        let _ = fs::remove_file(&staged);
        bail!(
            "{}",
            crate::i18n::ui_owned!(
                format!(
                    "Görüntü ve ses akışları kayıpsız birleştirilemedi: {}",
                    bounded_tail(&result.stderr, 4096)
                ),
                format!(
                    "The video and audio streams could not be merged losslessly: {}",
                    bounded_tail(&result.stderr, 4096)
                )
            )
        );
    }
    fs::rename(&staged, &output)?;
    for join in joins {
        join.remove();
    }
    Ok(output)
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
            fs::write(
                index_path(&self.joined),
                format!("{} {}", state.next, state.length),
            )?;
            fs::remove_file(&segment)?;
        }
        Ok(())
    }

    fn remove(&self) {
        let _ = fs::remove_file(&self.joined);
        let _ = fs::remove_file(index_path(&self.joined));
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

#[derive(Clone, Copy)]
struct Task {
    stream: usize,
    index: usize,
    attempt: u32,
    not_before: Instant,
}

/// Parallel requests one host is given: halved when it answers 429 or 503, raised by one after
/// a run of successes, never above the per-host bound.
struct Pace {
    limit: usize,
    in_flight: usize,
    successes: usize,
    last_cut: Option<Instant>,
}

struct Shared {
    state: Mutex<Schedule>,
    wake: Condvar,
}

struct Schedule {
    queues: Vec<VecDeque<Task>>,
    hosts: HashMap<String, Pace>,
    per_host: usize,
    remaining: usize,
    failure: Option<anyhow::Error>,
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
                        not_before: now,
                    })
                    .collect::<VecDeque<_>>()
            })
            .collect::<Vec<_>>();
        let remaining = queues.iter().map(VecDeque::len).sum();
        debug_assert_eq!(streams.len(), queues.len());
        Self {
            state: Mutex::new(Schedule {
                queues,
                hosts: HashMap::new(),
                per_host,
                remaining,
                failure: None,
            }),
            wake: Condvar::new(),
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

    /// The next due segment whose host has room, lowest playlist position first, so joins
    /// advance while later segments arrive.
    fn next_task(&self, streams: &[NativeStream], control: &TransferControl) -> Option<Task> {
        let mut state = self.lock();
        loop {
            if state.remaining == 0 || state.failure.is_some() || control.stop_requested() {
                return None;
            }
            let now = Instant::now();
            let mut earliest: Option<Instant> = None;
            let per_host = state.per_host;
            let mut chosen = None;
            'streams: for stream in 0..state.queues.len() {
                for position in 0..state.queues[stream].len() {
                    let task = state.queues[stream][position];
                    if task.not_before > now {
                        earliest =
                            Some(earliest.map_or(task.not_before, |at| at.min(task.not_before)));
                        continue;
                    }
                    let host = host_key(&streams[task.stream].segments[task.index]);
                    let pace = state.hosts.entry(host).or_insert(Pace {
                        limit: per_host,
                        in_flight: 0,
                        successes: 0,
                        last_cut: None,
                    });
                    if pace.in_flight < pace.limit {
                        pace.in_flight += 1;
                        chosen = Some((stream, position));
                        break 'streams;
                    }
                }
            }
            if let Some((stream, position)) = chosen {
                return state.queues[stream].remove(position);
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

    fn settle(&self, host: &str, outcome: Settled) {
        let mut state = self.lock();
        let per_host = state.per_host;
        if let Some(pace) = state.hosts.get_mut(host) {
            pace.in_flight = pace.in_flight.saturating_sub(1);
            match &outcome {
                Settled::Done => {
                    pace.successes += 1;
                    if pace.limit < per_host && pace.successes >= pace.limit * 4 {
                        pace.limit += 1;
                        pace.successes = 0;
                    }
                }
                Settled::Retry {
                    throttled: true, ..
                } => {
                    // One cut per second: the requests already in flight answer the same burst.
                    if pace
                        .last_cut
                        .is_none_or(|at| at.elapsed() >= Duration::from_secs(1))
                    {
                        pace.limit = (pace.limit / 2).max(1);
                        pace.last_cut = Some(Instant::now());
                    }
                    pace.successes = 0;
                }
                Settled::Retry { .. } | Settled::Failed => {}
            }
        }
        match outcome {
            Settled::Done => state.remaining -= 1,
            Settled::Failed => {}
            Settled::Retry { task, .. } => {
                let queue = &mut state.queues[task.stream];
                let position = queue
                    .iter()
                    .position(|queued| queued.index > task.index)
                    .unwrap_or(queue.len());
                queue.insert(position, task);
            }
        }
        self.wake.notify_all();
    }
}

enum Settled {
    Done,
    Retry { task: Task, throttled: bool },
    Failed,
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

impl WorkerContext<'_> {
    fn work(&self) {
        while let Some(mut task) = self.shared.next_task(self.streams, self.control) {
            let stream = &self.streams[task.stream];
            let join = &self.joins[task.stream];
            let host = host_key(&stream.segments[task.index]);
            let segment = segment_path(&join.joined, task.index);
            let mut url = stream.segments[task.index].clone();
            let mut fetched = self.fetch(&url, stream, &segment);
            for _ in 0..MAX_REDIRECTS {
                let Ok(Fetched::Redirect(next)) = &fetched else {
                    break;
                };
                url = next.clone();
                fetched = self.fetch(&url, stream, &segment);
            }
            match fetched {
                Ok(Fetched::Done) => {
                    self.shared.settle(&host, Settled::Done);
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
                    let backoff = Duration::from_secs(1 << task.attempt.min(4)).min(MAX_BACKOFF);
                    task.attempt += 1;
                    task.not_before = Instant::now()
                        + retry_after.map_or(backoff, |delay| delay.min(MAX_SEGMENT_RETRY_AFTER));
                    crate::logging::record(
                        crate::logging::Event::warn("media.segment_retry")
                            .host(host.clone())
                            .detail(format!(
                                "{} segment {} deneme {}: {reason}",
                                stream.role, task.index, task.attempt
                            )),
                    );
                    self.shared
                        .settle(&host, Settled::Retry { task, throttled });
                }
                Ok(Fetched::Retry { status, reason, .. }) => {
                    self.shared.settle(&host, Settled::Failed);
                    self.shared
                        .fail(segment_failure(stream, task.index, status, &reason));
                }
                Ok(Fetched::Redirect(_)) => {
                    self.shared.settle(&host, Settled::Failed);
                    self.shared.fail(segment_failure(
                        stream,
                        task.index,
                        None,
                        "too many redirects",
                    ));
                }
                Err(error) => {
                    self.shared.settle(&host, Settled::Failed);
                    self.shared.fail(error);
                }
            }
        }
    }

    /// One request for a segment into `<segment>.tmp`, renamed to `segment` once whole.
    fn fetch(&self, url: &Url, stream: &NativeStream, segment: &Path) -> Result<Fetched> {
        let partial = partial_path(segment);
        let mut file = File::create(&partial)?;
        let mut easy = self.network.easy_for_url(url, self.control)?;
        easy.url(url.as_str())?;
        easy.follow_location(false)?;
        easy.fail_on_error(false)?;
        easy.connect_timeout(Duration::from_secs(20))?;
        easy.low_speed_limit(1)?;
        easy.low_speed_time(Duration::from_secs(30))?;
        easy.progress(true)?;
        let mut headers = List::new();
        for (name, value) in &stream.headers {
            headers.append(&format!("{name}: {value}"))?;
        }
        let cookie = self
            .cookies
            .iter()
            .filter(|cookie| scoped_cookie_applies(cookie, url))
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
        if whole {
            fs::rename(&partial, segment)?;
            return Ok(Fetched::Done);
        }
        self.received.fetch_sub(written, Ordering::Relaxed);
        let _ = fs::remove_file(&partial);
        if let Some(error) = write_error {
            return Err(error.into());
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
            if let Some(next) = header("location")
                .and_then(|location| url.join(&location).ok())
                .filter(|next| matches!(next.scheme(), "http" | "https"))
            {
                return Ok(Fetched::Redirect(next));
            }
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
    let message = crate::i18n::ui_owned!(
        format!(
            "{} akışının {}. parçası {SEGMENT_ATTEMPTS} denemede indirilemedi: {reason}",
            if stream.role == "video" {
                "Görüntü"
            } else {
                "Ses"
            },
            index + 1
        ),
        format!(
            "Segment {} of the {} stream failed after {SEGMENT_ATTEMPTS} attempts: {reason}",
            index + 1,
            stream.role
        )
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
        while window.len() > 2 && now.duration_since(window[0].0) > Duration::from_secs(3) {
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
        assert!(!declined(&separate(), &video));
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
        }
    }

    #[test]
    fn a_throttling_host_gets_fewer_requests_and_earns_them_back() {
        let root = std::env::temp_dir().join(format!("ssdownload-pace-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let streams = [one_host_stream(64)];
        let joins = [Join::resume(&root, &streams[0]).unwrap()];
        let shared = Shared::new(&streams, &joins, 8);
        let control = TransferControl::default();
        let host = "cdn.example";
        let limit = || shared.lock().hosts[host].limit;

        let tasks = (0..8)
            .map(|_| shared.next_task(&streams, &control).unwrap())
            .collect::<Vec<_>>();
        // The host has its full share in flight: nothing more is handed out for it.
        {
            let state = shared.lock();
            assert_eq!(state.hosts[host].in_flight, state.hosts[host].limit);
        }

        // A burst of 429s halves the share once, not once per answer.
        for task in &tasks[..4] {
            shared.settle(
                host,
                Settled::Retry {
                    task: *task,
                    throttled: true,
                },
            );
        }
        assert_eq!(limit(), 4);
        shared.lock().hosts.get_mut(host).unwrap().last_cut = None;
        shared.settle(
            host,
            Settled::Retry {
                task: tasks[4],
                throttled: true,
            },
        );
        assert_eq!(limit(), 2);
        // A failed connection is retried without cutting the share.
        shared.settle(
            host,
            Settled::Retry {
                task: tasks[5],
                throttled: false,
            },
        );
        assert_eq!(limit(), 2);
        // Four successes per allowed request raise the share by one.
        for _ in 0..8 {
            shared.lock().hosts.get_mut(host).unwrap().in_flight += 1;
            shared.settle(host, Settled::Done);
        }
        assert_eq!(limit(), 3);
        // Retried segments go back in playlist order, ahead of later ones.
        let order = shared.lock().queues[0]
            .iter()
            .map(|task| task.index)
            .take(6)
            .collect::<Vec<_>>();
        assert_eq!(order, [0, 1, 2, 3, 4, 5]);
        fs::remove_dir_all(root).unwrap();
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
}
