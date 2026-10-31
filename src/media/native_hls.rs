//! App-native download of a pinned, finished HLS source with separate video and audio
//! playlists. Both streams' segments are fetched at once over connections kept from one segment
//! to the next, each segment is retried on its own, a host that answers 429 or 503 gets fewer
//! parallel requests, and the joined streams are copied into one container. Every other source
//! shape stays with yt-dlp.

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
            if !extractor_cookies_are_known(format, request) {
                return None;
            }
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
    let joins = fetch_segments(streams, workspace, request, control, network, progress)?;
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

#[derive(Clone, Copy)]
struct Task {
    stream: usize,
    index: usize,
    attempt: u32,
    not_before: Instant,
}

/// Parallel requests one stream sends to one host: halved when it answers 429 or 503, raised by
/// one after a run of successes, never above the per-host bound. A host that throttles one
/// stream's requests keeps serving the other stream at its full share.
struct Pace {
    limit: usize,
    in_flight: usize,
    successes: usize,
    last_cut: Option<Instant>,
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
    paces: HashMap<(usize, usize), Pace>,
    /// Requests in flight per host across all streams, never above `per_host`.
    busy: Vec<usize>,
    /// Workers that keep a connection to each host between its segments, never above
    /// `per_host`: their connections hold places of the host's connection bound.
    holders: Vec<usize>,
    per_host: usize,
    remaining: usize,
    failure: Option<anyhow::Error>,
}

/// What a worker does next.
enum Next {
    Fetch(Task),
    /// Close the kept connection: the worker has nothing to fetch at once, and an idle
    /// connection would hold a place of its host's bound that another download, or another
    /// worker of this one, may be waiting for.
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
                        not_before: now,
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
                paces: HashMap::new(),
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

    /// The next due segment whose host has room for the worker that keeps a connection to
    /// `kept`. Each stream offers its lowest playlist position, so joins advance while later
    /// segments arrive, and the stream furthest behind through its own playlist goes first, so
    /// video and audio finish together and a throttled stream is not left for the end. A worker
    /// that takes a segment of another host gives up its place at `kept`, and one with nothing
    /// to take at once closes its connection instead of waiting with it.
    fn next_task(
        &self,
        streams: &[NativeStream],
        control: &TransferControl,
        kept: Option<usize>,
    ) -> Option<Next> {
        let mut state = self.lock();
        loop {
            if state.remaining == 0 || state.failure.is_some() || control.stop_requested() {
                return None;
            }
            let now = Instant::now();
            let mut earliest: Option<Instant> = None;
            let per_host = state.per_host;
            let mut chosen: Option<(usize, usize, usize)> = None;
            for stream in 0..state.queues.len() {
                for position in 0..state.queues[stream].len() {
                    let task = state.queues[stream][position];
                    if task.not_before > now {
                        earliest =
                            Some(earliest.map_or(task.not_before, |at| at.min(task.not_before)));
                        continue;
                    }
                    let host = self.hosts[stream][task.index];
                    let busy = state.busy[host];
                    let pace = state.paces.entry((stream, host)).or_insert(Pace {
                        limit: per_host,
                        in_flight: 0,
                        successes: 0,
                        last_cut: None,
                    });
                    if busy >= per_host
                        || pace.in_flight >= pace.limit
                        || (kept != Some(host) && state.holders[host] >= per_host)
                    {
                        continue;
                    }
                    let behind = chosen.as_ref().is_none_or(|(other, at, _)| {
                        let other_index = state.queues[*other][*at].index;
                        task.index * streams[*other].segments.len()
                            < other_index * streams[stream].segments.len()
                    });
                    if behind {
                        chosen = Some((stream, position, host));
                    }
                    break;
                }
            }
            if let Some((stream, position, host)) = chosen {
                state.busy[host] += 1;
                if let Some(pace) = state.paces.get_mut(&(stream, host)) {
                    pace.in_flight += 1;
                }
                if kept != Some(host) {
                    state.holders[host] += 1;
                    if let Some(previous) = kept {
                        state.holders[previous] -= 1;
                    }
                }
                return state.queues[stream].remove(position).map(Next::Fetch);
            }
            if let Some(host) = kept {
                state.holders[host] -= 1;
                return Some(Next::Release);
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

    fn settle(&self, stream: usize, host: usize, outcome: Settled) {
        let mut state = self.lock();
        let per_host = state.per_host;
        state.busy[host] = state.busy[host].saturating_sub(1);
        if let Some(pace) = state.paces.get_mut(&(stream, host)) {
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
        while let Some(next) = self.shared.next_task(self.streams, self.control, held) {
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
                fetched = self
                    .network
                    .easy_for_url(&url, self.control)
                    .and_then(|mut easy| self.fetch(&mut easy, &url, stream, &segment));
            }
            match fetched {
                Ok(Fetched::Done) => {
                    self.shared.settle(stream_index, host, Settled::Done);
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
                    let backoff = Duration::from_secs(1 << task.attempt.min(4)).min(MAX_BACKOFF);
                    task.attempt += 1;
                    task.not_before = Instant::now()
                        + retry_after.map_or(backoff, |delay| delay.min(MAX_SEGMENT_RETRY_AFTER));
                    crate::logging::record(
                        crate::logging::Event::warn("media.segment_retry")
                            .host(self.shared.host_names[host].clone())
                            .detail(format!(
                                "{} segment {} deneme {}: {reason}",
                                stream.role, task.index, task.attempt
                            )),
                    );
                    self.shared
                        .settle(stream_index, host, Settled::Retry { task, throttled });
                }
                Ok(Fetched::Retry { status, reason, .. }) => {
                    self.shared.settle(stream_index, host, Settled::Failed);
                    self.shared
                        .fail(segment_failure(stream, task.index, status, &reason));
                }
                Ok(Fetched::Redirect(_)) => {
                    self.shared.settle(stream_index, host, Settled::Failed);
                    self.shared.fail(segment_failure(
                        stream,
                        task.index,
                        None,
                        "too many redirects",
                    ));
                }
                Err(error) => {
                    self.shared.settle(stream_index, host, Settled::Failed);
                    self.shared.fail(error);
                }
            }
        }
    }

    /// `fetch` over the kept connection when it is to the same origin. The connection is kept
    /// for the next segment after a whole answer. After a redirect it closes before the target
    /// gets a connection of its own, so a worker never waits for one while it holds another.
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
        let fetched = self.fetch(&mut easy, url, stream, segment);
        if matches!(
            fetched,
            Ok(Fetched::Done
                | Fetched::Retry {
                    status: Some(_),
                    ..
                })
        ) {
            *kept = Some(Kept { origin, easy });
        }
        fetched
    }

    /// One request for a segment into `<segment>.tmp`, renamed to `segment` once whole.
    fn fetch(
        &self,
        easy: &mut crate::network::GovernedEasy,
        url: &Url,
        stream: &NativeStream,
        segment: &Path,
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

    /// The segment `next_task` hands to a worker that keeps a connection to `kept`.
    fn task_for(shared: &Shared, streams: &[NativeStream], kept: Option<usize>) -> Task {
        match shared.next_task(streams, &TransferControl::default(), kept) {
            Some(Next::Fetch(task)) => task,
            _ => panic!("no segment was handed out"),
        }
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
        let host = "cdn.example";
        let key = (0, 0);
        assert_eq!(shared.host_names, [host]);
        let limit = || shared.lock().paces[&key].limit;

        let tasks = (0..8)
            .map(|_| task_for(&shared, &streams, None))
            .collect::<Vec<_>>();
        // The host has its full share in flight: nothing more is handed out for it.
        {
            let state = shared.lock();
            assert_eq!(state.paces[&key].in_flight, state.paces[&key].limit);
            assert_eq!(state.busy[0], 8);
        }

        // A burst of 429s halves the share once, not once per answer.
        for task in &tasks[..4] {
            shared.settle(
                0,
                0,
                Settled::Retry {
                    task: *task,
                    throttled: true,
                },
            );
        }
        assert_eq!(limit(), 4);
        shared.lock().paces.get_mut(&key).unwrap().last_cut = None;
        shared.settle(
            0,
            0,
            Settled::Retry {
                task: tasks[4],
                throttled: true,
            },
        );
        assert_eq!(limit(), 2);
        // A failed connection is retried without cutting the share.
        shared.settle(
            0,
            0,
            Settled::Retry {
                task: tasks[5],
                throttled: false,
            },
        );
        assert_eq!(limit(), 2);
        // Four successes per allowed request raise the share by one.
        for _ in 0..8 {
            shared.lock().paces.get_mut(&key).unwrap().in_flight += 1;
            shared.settle(0, 0, Settled::Done);
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
    fn streams_on_one_host_advance_together_and_are_throttled_apart() {
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

        // The audio's 429 halves its own share only, and frees the host slot it held.
        shared.settle(
            1,
            host,
            Settled::Retry {
                task: Task {
                    attempt: 1,
                    not_before: Instant::now() + Duration::from_secs(60),
                    ..tasks[1]
                },
                throttled: true,
            },
        );
        {
            let state = shared.lock();
            assert_eq!(state.paces[&(1, host)].limit, 2);
            assert_eq!(state.paces[&(0, host)].limit, 4);
            assert_eq!(state.busy[host], 3);
        }
        // The audio is still furthest behind, so its next segment takes the free slot of the
        // worker that keeps the connection.
        let next = task_for(&shared, &streams, Some(host));
        assert_eq!((next.stream, next.index), (1, 1));
        assert_eq!(shared.lock().busy[host], 4);
        fs::remove_dir_all(root).unwrap();
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
        shared.settle(0, video, Settled::Done);
        shared.settle(1, audio, Settled::Done);
        // The video host has a free request but both of its places are kept: the worker that
        // keeps the audio connection takes the next audio segment, though the video is as far.
        let next = task_for(&shared, &streams, Some(audio));
        assert_eq!((next.stream, next.index), (1, 2));
        assert_eq!(shared.lock().holders, [2, 2]);

        // The video's last segments come in; a worker that keeps that host lets it go then.
        shared.settle(0, video, Settled::Done);
        for index in [2, 3] {
            assert_eq!(task_for(&shared, &streams, Some(video)).index, index);
            shared.settle(0, video, Settled::Done);
        }
        assert!(matches!(
            shared.next_task(&streams, &TransferControl::default(), Some(video)),
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
            0,
            Settled::Retry {
                task: Task {
                    attempt: 1,
                    not_before: Instant::now() + Duration::from_secs(2),
                    ..first
                },
                throttled: false,
            },
        );
        assert!(matches!(
            shared.next_task(&streams, &TransferControl::default(), Some(0)),
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
}
