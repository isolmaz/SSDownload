use crate::{
    model::{AddRequest, TransferControl, TransferProgress, TransferResult},
    network::{GovernedEasy, NetworkGovernor},
};
use anyhow::{anyhow, bail, Context, Result};
use curl::easy::List;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    cell::RefCell,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering},
        mpsc, Arc, Condvar, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use url::Url;

const MAX_REDIRECTS: usize = 5;
const MIN_SEGMENT_SIZE: u64 = 2 * 1024 * 1024;
const MAX_SEGMENT_SIZE: u64 = 8 * 1024 * 1024;
const MIN_SPLIT_INTERVAL: Duration = Duration::from_millis(25);
const MAX_RANGE_ATTEMPTS: u8 = 3;
const RANGE_WORKER_IDLE_TIMEOUT: Duration = Duration::from_millis(200);
/// How long a lease has to flow before its rate is taken for the rate it keeps. The bytes of the
/// first moments of a transfer come unevenly (a new connection ramps up, writes arrive in
/// bursts), and a lease cut on a rate seen over less than this is cut on noise. It is longer
/// than the 200 ms between two looks of the scheduler at its leases, so that a rate spans more
/// than one look. Waiting for it delays a cut by this long and the end of the download by about
/// half as much.
const MIN_RATE_SAMPLE: Duration = Duration::from_millis(250);
/// How often a worker that waits for a renewal looks at a stop request.
const RENEWAL_POLL: Duration = Duration::from_millis(100);
const IO_BUFFER: usize = 1024 * 1024;
const STATE_VERSION: u8 = 3;
// A lease is shorter than `MAX_SEGMENT_SIZE + MIN_SEGMENT_SIZE`, and an `Allowance` counts the
// bytes of a lease in 32 bits.
const _: () = assert!(MAX_SEGMENT_SIZE + MIN_SEGMENT_SIZE <= u32::MAX as u64);

/// When written data is made durable and the resume state is updated: after `interval`, or
/// once `bytes` are waiting, whichever comes first. A crash or power loss costs about that
/// much transfer. A cycle is a flush of the part file and an atomic write of the state (two
/// more syncs), so it is spaced out in time; the byte limit bounds the data a cycle has to
/// flush and the transfer a crash can cost on a fast link. Pausing, stopping, a lost connection
/// and the end of the transfer run a cycle at once.
#[derive(Debug, Clone, Copy)]
struct Cadence {
    interval: Duration,
    bytes: u64,
}

impl Cadence {
    const DEFAULT: Self = Self {
        interval: Duration::from_secs(5),
        bytes: 64 * 1024 * 1024,
    };

    fn current() -> Self {
        #[cfg(test)]
        if let Some(cadence) = TEST_CADENCE.with(std::cell::Cell::get) {
            return cadence;
        }
        Self::DEFAULT
    }

    /// `since` is when the oldest of the `waiting` bytes was written.
    fn due(self, since: Instant, waiting: u64) -> bool {
        waiting > 0 && (waiting >= self.bytes || since.elapsed() >= self.interval)
    }
}

/// When the next periodic cycle of a download is due. A periodic cycle that fails leaves the
/// last good record in place and is tried again after an interval.
struct Pace {
    cadence: Cadence,
    /// When the oldest byte that is in no cycle yet was written.
    since: Instant,
    failed: Option<Instant>,
}

impl Pace {
    fn new() -> Self {
        Self {
            cadence: Cadence::current(),
            since: Instant::now(),
            failed: None,
        }
    }

    fn restart(&mut self) {
        self.since = Instant::now();
    }

    fn fail(&mut self) {
        self.failed = Some(Instant::now());
    }

    /// `waiting` is the number of bytes written that are in no cycle yet.
    fn due(&mut self, waiting: u64) -> bool {
        if waiting == 0 {
            self.restart();
        }
        let retrying = self
            .failed
            .is_some_and(|at| at.elapsed() < self.cadence.interval);
        !retrying && self.cadence.due(self.since, waiting)
    }
}

/// What the cycles of a download did and in which order ("counted", "flushed", "writer
/// flushed", "recorded" or "failed"), kept for tests.
#[derive(Clone, Default)]
struct Steps(
    #[cfg(test)] Arc<std::sync::Mutex<Vec<&'static str>>>,
    #[cfg(test)] Duration,
);

impl Steps {
    #[cfg(test)]
    fn current() -> Self {
        Self(
            TEST_STEPS.with(Clone::clone),
            TEST_PAUSE.with(std::cell::Cell::get),
        )
    }

    #[cfg(not(test))]
    fn current() -> Self {
        Self::default()
    }

    #[cfg(test)]
    fn note(&self, step: &'static str) {
        self.0.lock().unwrap().push(step);
    }

    #[cfg(not(test))]
    fn note(&self, _step: &'static str) {}

    /// Holds a cycle between its flush and its record, so that a test can end the transfer
    /// while a cycle is still running.
    #[cfg(test)]
    fn pause(&self) {
        thread::sleep(self.1);
    }

    #[cfg(not(test))]
    fn pause(&self) {}
}

/// Where the workers of a download wrote, as the offset and length of each write, kept for
/// tests.
#[derive(Clone, Default)]
struct Writes(#[cfg(test)] Arc<std::sync::Mutex<Vec<(u64, u64)>>>);

impl Writes {
    #[cfg(test)]
    fn current() -> Self {
        Self(TEST_WRITES.with(Clone::clone))
    }

    #[cfg(not(test))]
    fn current() -> Self {
        Self::default()
    }

    #[cfg(test)]
    fn note(&self, offset: u64, length: u64) {
        self.0.lock().unwrap().push((offset, length));
    }

    #[cfg(not(test))]
    fn note(&self, _offset: u64, _length: u64) {}
}

#[cfg(test)]
thread_local! {
    /// Replaces the cadence of the downloads that the current test thread starts.
    static TEST_CADENCE: std::cell::Cell<Option<Cadence>> = const { std::cell::Cell::new(None) };
    /// The steps of the cycles of the downloads that the current test thread ran.
    static TEST_STEPS: Arc<std::sync::Mutex<Vec<&'static str>>> = Arc::default();
    /// The writes of the workers of the downloads that the current test thread ran.
    static TEST_WRITES: Arc<std::sync::Mutex<Vec<(u64, u64)>>> = Arc::default();
    /// How long the cycles of the downloads that the current test thread starts wait between
    /// their flush and their record.
    static TEST_PAUSE: std::cell::Cell<Duration> = const { std::cell::Cell::new(Duration::ZERO) };
    /// How long the range workers of the downloads that the current test thread starts wait,
    /// after they receive a range, before they request it.
    static TEST_START_DELAY: std::cell::Cell<Duration> = const { std::cell::Cell::new(Duration::ZERO) };
    /// The sources of the digests that verified the files the current test thread finished.
    static TEST_VERIFIED: RefCell<Vec<digest::Source>> = const { RefCell::new(Vec::new()) };
}

/// What a cycle does with the record once the part file is durable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Record {
    /// A record that cannot be written fails the cycle.
    Required,
    /// The record is tried; when it cannot be written the previous one stays valid.
    Attempted,
    /// No record: the part file is about to be published and the record deleted.
    Skipped,
}

/// The handles and the record that a cycle works on.
struct Disk {
    /// A handle that no writer uses, so that a flush through it holds back no write.
    flush: File,
    /// Every handle that writes the part file.
    writers: Vec<Arc<File>>,
    state_path: PathBuf,
    /// Added to a failed flush; the error of a failed record write is passed on as it is.
    flush_context: Option<&'static str>,
    steps: Steps,
}

impl Disk {
    /// Flushes through the flush handle, which drains most of the dirty data without holding
    /// back a writer, and then through every writer handle, so that no handle that wrote a byte
    /// the record will claim goes unflushed.
    fn flush(&self) -> Result<()> {
        let describe = |error: std::io::Error| match self.flush_context {
            Some(context) => anyhow!(error).context(context),
            None => anyhow!(error),
        };
        self.flush.sync_all().map_err(describe)?;
        self.steps.note("flushed");
        for writer in &self.writers {
            writer.sync_all().map_err(describe)?;
            self.steps.note("writer flushed");
        }
        Ok(())
    }

    /// Flushes, and only then writes `state`. Returns whether the record was written.
    fn flush_then_record(&self, state: &ResumeState, record: Record) -> Result<bool> {
        if let Err(error) = self.flush() {
            self.steps.note("failed");
            return Err(error);
        }
        if record == Record::Skipped {
            return Ok(false);
        }
        self.steps.pause();
        match write_state(&self.state_path, state) {
            Ok(()) => {
                self.steps.note("recorded");
                Ok(true)
            }
            Err(error) => {
                self.steps.note("failed");
                if record == Record::Required {
                    Err(error)
                } else {
                    Ok(false)
                }
            }
        }
    }
}

/// The flush-and-record step of a checkpoint. A periodic one runs on a helper thread, so that
/// neither the writers nor the loop that schedules them wait for the disk, and at most one runs
/// at a time; the ones that must succeed run on the caller's thread after the running one.
/// `covers` is what the running cycle will have made durable and recorded.
struct Cycle<T> {
    disk: Arc<Disk>,
    running: Option<(thread::JoinHandle<Result<()>>, T)>,
}

impl<T> Cycle<T> {
    fn new(
        flush: File,
        writers: Vec<Arc<File>>,
        state_path: &Path,
        flush_context: Option<&'static str>,
    ) -> Self {
        Self {
            disk: Arc::new(Disk {
                flush,
                writers,
                state_path: state_path.to_path_buf(),
                flush_context,
                steps: Steps::current(),
            }),
            running: None,
        }
    }

    fn state_path(&self) -> &Path {
        &self.disk.state_path
    }

    fn note(&self, step: &'static str) {
        self.disk.steps.note(step);
    }

    fn busy(&self) -> bool {
        self.running.is_some()
    }

    fn covers(&self) -> Option<&T> {
        self.running.as_ref().map(|(_, covers)| covers)
    }

    /// Starts a periodic cycle. A thread that cannot be created is a cycle that failed, and is
    /// returned like one.
    fn start(&mut self, state: ResumeState, covers: T) -> Option<(T, Result<()>)> {
        debug_assert!(self.running.is_none(), "one cycle runs at a time");
        let disk = self.disk.clone();
        let spawned = thread::Builder::new()
            .name("checkpoint".into())
            .spawn(move || disk.flush_then_record(&state, Record::Required).map(|_| ()));
        match spawned {
            Ok(handle) => {
                self.running = Some((handle, covers));
                None
            }
            Err(error) => Some(report_periodic(covers, Err(anyhow!(error)))),
        }
    }

    /// The outcome of the running cycle, if it has ended.
    fn poll(&mut self) -> Option<(T, Result<()>)> {
        let ended = self
            .running
            .as_ref()
            .is_some_and(|(handle, _)| handle.is_finished());
        if ended {
            self.wait()
        } else {
            None
        }
    }

    /// The outcome of the running cycle, once it has ended.
    fn wait(&mut self) -> Option<(T, Result<()>)> {
        let (handle, covers) = self.running.take()?;
        let result = handle
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        Some(report_periodic(covers, result))
    }

    /// Runs a cycle on this thread, for the checkpoints that must succeed. Returns whether the
    /// record was written.
    fn run(&self, state: &ResumeState, record: Record) -> Result<bool> {
        self.disk.flush_then_record(state, record)
    }
}

impl<T> Drop for Cycle<T> {
    /// No cycle outlives the transfer: a record written after the transfer ended could
    /// describe a part file that is already gone.
    fn drop(&mut self) {
        if let Some((handle, _)) = self.running.take() {
            let _ = handle.join();
        }
    }
}

/// The outcome of a periodic cycle. A failure does not end the transfer, so it is only logged.
fn report_periodic<T>(covers: T, result: Result<()>) -> (T, Result<()>) {
    if let Err(error) = &result {
        crate::logging::record(
            crate::logging::Event::warn("transfer.checkpoint_failed")
                .outcome(crate::logging::Outcome::Failed)
                .detail(format!("{error:#}")),
        );
    }
    (covers, result)
}

/// How the requests of an attempt are bound to bytes that an earlier attempt left on disk.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Retained {
    /// No earlier bytes: a response only must not contradict the validators it is compared with.
    #[default]
    None,
    /// Earlier bytes, and something other than the response already vouches for the record:
    /// this attempt's probe carries an equal validator, or a supplied SHA-256 guards the result.
    Vouched,
    /// Earlier bytes: each ranged response has to carry a validator equal to the record's.
    Unvouched,
    /// Earlier bytes, and this attempt's probe reports an ETag other than the recorded one:
    /// only the recorded strong ETag, carried by the ranged response itself, proves the file.
    Disputed,
}

#[derive(Debug, Clone)]
pub(crate) struct RemoteInfo {
    url: Url,
    credential_origin: (String, String, u16),
    total: Option<u64>,
    etag: Option<String>,
    last_modified: Option<String>,
    retained: Retained,
    content_type: Option<String>,
    /// The answer that `content_type` belongs to was sent as an attachment.
    attachment: bool,
    filename: Option<String>,
    /// A redirect led to `url`, so a web page answering there is a sign-in or interstitial page
    /// and not the file. `download` clears it for a requested name that is a page's.
    refuse_page: bool,
    /// The SHA-256 (hex) that the answer to the probe published in `Repr-Digest`.
    repr_digest: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct ResponseHeaders {
    status: u32,
    location: Option<String>,
    etag: Option<String>,
    last_modified: Option<String>,
    content_type: Option<String>,
    attachment: bool,
    filename: Option<String>,
    content_length: Option<u64>,
    content_range: Option<ContentRange>,
    retry_after: Option<Duration>,
    headers_complete: bool,
    http1: bool,
    chunked: bool,
    /// The values of every `Repr-Digest` line, joined.
    repr_digest: Option<String>,
}

#[derive(Debug, Clone, Copy)]
struct ContentRange {
    start: u64,
    end: u64,
    total: Option<u64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
struct ByteRange {
    /// Inclusive byte offset.
    start: u64,
    /// Exclusive byte offset.
    end: u64,
}

impl ByteRange {
    fn len(self) -> u64 {
        self.end.saturating_sub(self.start)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct ResumeState {
    version: u8,
    /// Version 2 used this URL digest as part of identity. Version 3 retains it
    /// only for explicit, safe migration of old state.
    url: String,
    etag: Option<String>,
    last_modified: Option<String>,
    total: Option<u64>,
    segmented: bool,
    connections: u8,
    checksum: Option<String>,
    /// Durably accepted, nonoverlapping ranges in the sparse part file.
    completed: Vec<ByteRange>,
    /// One-connection transfers: the leading bytes of the part file that were written and then
    /// flushed to disk; the file can be longer. Absent in records of earlier releases, whose
    /// resume point is the length of the part file. Earlier releases ignore the field.
    #[serde(skip_serializing_if = "Option::is_none")]
    synced: Option<u64>,
}

impl Default for ResumeState {
    fn default() -> Self {
        Self {
            version: 0,
            url: String::new(),
            etag: None,
            last_modified: None,
            total: None,
            segmented: false,
            connections: 1,
            checksum: None,
            completed: Vec::new(),
            synced: None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ExpectedResponse {
    Full,
    Range {
        start: u64,
        end: Option<u64>,
        total: Option<u64>,
    },
}

#[derive(Debug)]
enum AttemptFailure {
    Interrupted,
    RangeRejected,
    /// A ranged request answered with a full body (200): the server does not honour ranges.
    RangeIgnored,
    ResourceChanged,
    RetryableStatus {
        status: u32,
        retry_after: Option<Duration>,
    },
    RetryableTransport(anyhow::Error),
    Other(anyhow::Error),
}

impl From<anyhow::Error> for AttemptFailure {
    fn from(value: anyhow::Error) -> Self {
        Self::Other(value)
    }
}

#[derive(Debug)]
struct RetryableStatusError {
    status: u32,
    retry_after: Option<Duration>,
}

impl std::fmt::Display for RetryableStatusError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.retry_after {
            Some(delay) => write!(
                formatter,
                "{}",
                crate::i18n::ui_format!(
                    "Sunucu HTTP {0} yanıtını verdi (Retry-After {1} sn)",
                    "The server returned an HTTP {0} response (Retry-After {1} s)",
                    self.status,
                    delay.as_secs()
                )
            ),
            None => write!(
                formatter,
                "{}",
                crate::i18n::ui_format!(
                    "Sunucu HTTP {0} yanıtını verdi",
                    "The server returned an HTTP {0} response",
                    self.status
                )
            ),
        }
    }
}

impl std::error::Error for RetryableStatusError {}

fn retryable_status_error(status: u32, retry_after: Option<Duration>) -> anyhow::Error {
    anyhow!(RetryableStatusError {
        status,
        retry_after,
    })
}

pub(crate) fn retry_after(error: &anyhow::Error) -> Option<Duration> {
    error.chain().find_map(|cause| {
        cause
            .downcast_ref::<RetryableStatusError>()
            .and_then(|status| status.retry_after)
    })
}

/// A final non-success HTTP status. The engine classifies it by `status`, never by the
/// localized sentence, so retry and source-renewal decisions do not depend on the
/// interface language.
#[derive(Debug)]
pub(crate) struct HttpStatusError {
    pub(crate) status: u32,
}

impl std::fmt::Display for HttpStatusError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{}",
            crate::i18n::ui_format!(
                "Sunucu HTTP {0} yanıtını verdi",
                "The server returned an HTTP {0} response",
                self.status
            )
        )
    }
}

impl std::error::Error for HttpStatusError {}

/// Partial data whose source identity is unproven or changed. `download_http` and
/// `download_ftp` discard it and restart from zero. The error reaches the engine only when
/// the transfer was stopped or the restarted attempt fails the same way; the engine treats it
/// as a source-renewal candidate, and a renewed source that proves the identity re-binds the
/// kept data.
#[derive(Debug)]
pub(crate) struct PreservedPartialError {
    message: &'static str,
    /// The failing request was a ranged one that the server answered with a full body.
    ranges_ignored: bool,
}

impl std::fmt::Display for PreservedPartialError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for PreservedPartialError {}

fn preserved_partial(message: &'static str) -> anyhow::Error {
    anyhow!(PreservedPartialError {
        message,
        ranges_ignored: false
    })
}

fn preserved_partial_after(failure: &AttemptFailure, message: &'static str) -> anyhow::Error {
    anyhow!(PreservedPartialError {
        message,
        ranges_ignored: matches!(failure, AttemptFailure::RangeIgnored)
    })
}

fn ranges_ignored(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<PreservedPartialError>()
            .is_some_and(|partial| partial.ranges_ignored)
    })
}

/// A file of this many bytes did not match the digest its source publishes, the bytes were
/// discarded and the one re-download of the job is now used up: `download` fetches the file
/// again from the beginning. It is the only error of a first pass that asks for another pass; a
/// pass whose record says the re-download is used up publishes the file when its bytes are the
/// first pass's and ends with `SSD-TRF-003` otherwise.
#[derive(Debug)]
struct DigestRetry(u64);

impl std::fmt::Display for DigestRetry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(crate::i18n::ui(
            "Özet eşleşmedi; dosya baştan indiriliyor",
            "The digest did not match; downloading the file again",
        ))
    }
}

impl std::error::Error for DigestRetry {}

/// The HTTP status carried anywhere in the error chain.
pub(crate) fn http_status(error: &anyhow::Error) -> Option<u32> {
    error.chain().find_map(|cause| {
        cause
            .downcast_ref::<HttpStatusError>()
            .map(|value| value.status)
            .or_else(|| {
                cause
                    .downcast_ref::<RetryableStatusError>()
                    .map(|value| value.status)
            })
    })
}

/// Whether the chain carries a kept partial file awaiting a proven source.
pub(crate) fn preserves_partial(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.downcast_ref::<PreservedPartialError>().is_some())
}

/// The bytes of `output` that a later attempt builds on: the ranges the resume state records
/// for a segmented download, the recorded length of the part file for a sequential one. Bytes
/// that were received but the state does not record are not counted. `None` while the record
/// exists but cannot be read, when the amount is unknown.
pub(crate) fn retained_bytes(output: &Path) -> Option<u64> {
    let path = state_path(output);
    if unreadable_state(&path).is_some() {
        return None;
    }
    Some(match read_state(&path) {
        Some(state) if state.segmented => state.completed.iter().map(|range| range.len()).sum(),
        Some(state) => resume_point(
            &state,
            fs::metadata(part_path(output)).map_or(0, |meta| meta.len()),
        ),
        None => 0,
    })
}

/// The length of the part file that a one-connection transfer continues from: the flushed
/// length its record states, or the whole file when the record states none (it was written by
/// an earlier release).
fn resume_point(state: &ResumeState, part_bytes: u64) -> u64 {
    state
        .synced
        .map_or(part_bytes, |synced| synced.min(part_bytes))
}

/// Cuts the part file back to `length`, the point a resumed one-connection transfer continues
/// from, and flushes what remains, so the record written next claims only bytes on disk.
fn settle_part(part: &Path, length: u64) -> Result<()> {
    let file = OpenOptions::new().write(true).open(part).with_context(|| {
        crate::i18n::ui_format!(
            "Geçici dosya açılamadı: {0}",
            "The temporary file could not be opened: {0}",
            part.display()
        )
    })?;
    file.set_len(length)?;
    file.sync_all().context(crate::i18n::ui(
        "Kısmi dosya diske yazılamadı",
        "The partial file could not be written to disk",
    ))
}

/// The part file of a one-connection transfer together with its record. A cycle flushes the
/// part file through a second handle and through the sink's own, and only then records the
/// length reached, so `synced` never claims a byte that was not written before the flush.
/// Periodic cycles run beside the transfer and, when one fails, leave the last record in place;
/// `checkpoint` waits for them, has to flush, and writes the record as its `Record` asks.
struct SequentialSink {
    file: Arc<File>,
    cycle: Cycle<u64>,
    state: ResumeState,
    /// Where this transfer began in the part file.
    start: u64,
    written: u64,
    /// The part of `written` that the record covers.
    recorded: u64,
    pace: Pace,
}

impl SequentialSink {
    fn new(
        file: File,
        part: &Path,
        state_path: &Path,
        state: ResumeState,
        start: u64,
    ) -> Result<Self> {
        let flush = OpenOptions::new().write(true).open(part).with_context(|| {
            crate::i18n::ui_format!(
                "Geçici dosya açılamadı: {0}",
                "The temporary file could not be opened: {0}",
                part.display()
            )
        })?;
        let file = Arc::new(file);
        Ok(Self {
            cycle: Cycle::new(flush, vec![file.clone()], state_path, None),
            file,
            state,
            start,
            written: 0,
            recorded: 0,
            pace: Pace::new(),
        })
    }

    /// Bytes written that no cycle covers yet.
    fn waiting(&self) -> u64 {
        self.written - self.cycle.covers().copied().unwrap_or(self.recorded)
    }

    fn write(&mut self, data: &[u8]) -> std::io::Result<()> {
        self.settle();
        if self.waiting() == 0 {
            self.pace.restart();
        }
        (&*self.file).write_all(data)?;
        self.written += data.len() as u64;
        if !self.cycle.busy() && self.pace.due(self.waiting()) {
            self.begin();
        }
        Ok(())
    }

    /// Replaces the record's identity, for a transfer that has written nothing yet.
    fn adopt(&mut self, state: ResumeState) -> Result<()> {
        write_state(self.cycle.state_path(), &state)?;
        self.state = state;
        Ok(())
    }

    fn record_of(&self, written: u64) -> ResumeState {
        ResumeState {
            synced: Some(self.start + written),
            ..self.state.clone()
        }
    }

    /// Starts a periodic cycle for everything written so far.
    fn begin(&mut self) {
        self.cycle.note("counted");
        self.pace.restart();
        let record = self.record_of(self.written);
        if let Some(ended) = self.cycle.start(record, self.written) {
            self.absorb(ended);
        }
    }

    /// Takes in the outcome of a periodic cycle that has ended. A failed one changes nothing:
    /// the bytes stay waiting and the next cycle covers them.
    fn settle(&mut self) {
        if let Some(ended) = self.cycle.poll() {
            self.absorb(ended);
        }
    }

    fn absorb(&mut self, (written, result): (u64, Result<()>)) {
        if result.is_ok() {
            self.recorded = written;
            self.state.synced = Some(self.start + written);
        } else {
            self.pace.fail();
        }
    }

    /// Makes everything written so far durable, after any cycle in progress, and records it as
    /// `record` says. Fails when the flush fails, or when a required record cannot be written.
    fn checkpoint(&mut self, record: Record) -> Result<()> {
        if let Some(ended) = self.cycle.wait() {
            self.absorb(ended);
        }
        if self.written == self.recorded {
            return Ok(());
        }
        self.cycle.note("counted");
        let written = self.written;
        if self.cycle.run(&self.record_of(written), record)? {
            self.recorded = written;
            self.state.synced = Some(self.start + written);
            self.pace.restart();
        }
        Ok(())
    }
}

pub(crate) fn has_artifacts(output: &Path) -> bool {
    output.exists()
        || crate::media::has_partial_output(output)
        || part_path(output).exists()
        || state_path(output).exists()
        || (0..16).any(|index| segment_path(output, index).exists())
}

pub(crate) fn validate_source_url(value: &str) -> Result<Url> {
    let url = Url::parse(value.trim()).context(crate::i18n::ui(
        "Kaynak adresi geçerli değil",
        "The source URL is not valid",
    ))?;
    match url.scheme() {
        "http" | "https" | "ftp" => {}
        _ => bail!(
            "{}",
            crate::i18n::ui(
                "Yalnızca HTTP, HTTPS ve FTP adresleri desteklenir",
                "Only HTTP, HTTPS and FTP addresses are supported",
            )
        ),
    }
    if url.host_str().is_none() {
        bail!(
            "{}",
            crate::i18n::ui(
                "Kaynak adresinde sunucu adı yok",
                "The source address has no host name",
            )
        );
    }
    if matches!(url.scheme(), "http" | "https")
        && (!url.username().is_empty() || url.password().is_some())
    {
        bail!(
            "{}",
            crate::i18n::ui(
                "HTTP adresindeki kullanıcı bilgileri desteklenmez; yetkilendirme başlığı kullanın",
                "Credentials in HTTP addresses are not supported; use the authorization header",
            )
        );
    }
    Ok(url)
}

/// Auto jobs are sent to the media pipeline only on an explicit media URL or
/// an actual HTTP content type. A failed HEAD does not turn a normal file into
/// media based on a guess.
pub(crate) fn prepare_auto(
    request: &AddRequest,
    control: &TransferControl,
    network: &NetworkGovernor,
) -> Result<(bool, Option<RemoteInfo>)> {
    let url = validate_source_url(&request.url)?;
    // A selected stream, audio, subtitle, container, or playlist must stay in
    // the media pipeline so its mux and verification contract cannot be skipped.
    if request_requires_media_pipeline(request) || media_extension(&url) {
        return Ok((true, None));
    }
    if url.scheme() == "ftp" {
        return Ok((false, None));
    }
    let info = probe_http(request, url, control, network)?;
    let media = info
        .content_type
        .as_deref()
        .map(is_media_content_type)
        .unwrap_or(false);
    Ok((media, Some(info)))
}

#[derive(Debug)]
pub(crate) struct PreparedSourceRefresh {
    state_path: Option<PathBuf>,
    state: Option<ResumeState>,
}

/// Performs the cancellable remote identity check without changing retained work.
/// The actor must revalidate its refresh grant before calling `stage_source_refresh`.
pub(crate) fn prepare_source_refresh(
    previous: &AddRequest,
    next: &AddRequest,
    output: &Path,
    restart: bool,
    control: &TransferControl,
    network: &NetworkGovernor,
) -> Result<PreparedSourceRefresh> {
    if request_requires_media_pipeline(previous) || request_requires_media_pipeline(next) {
        bail!(
            "{}",
            crate::i18n::ui(
                "Seçili medya akışı doğrudan dosya yenileme yoluna geçirilemez",
                "The selected media stream cannot use the direct file-refresh path",
            )
        );
    }
    if control.stop_requested() {
        bail!(
            "{}",
            crate::i18n::ui("Kaynak yenileme durduruldu", "Source refresh stopped",)
        );
    }
    if restart
        || !part_path(output).exists()
            && !state_path(output).exists()
            && !legacy_segments_exist(output)
    {
        return Ok(PreparedSourceRefresh {
            state_path: None,
            state: None,
        });
    }
    let mut state = read_state(&state_path(output)).ok_or_else(|| {
        anyhow!(
            "{}",
            crate::i18n::ui(
                "Kısmi dosyaların kaynak kimliği kaydı yok; eserler korundu",
                "The partial files have no source identity record; the artifacts were preserved"
            )
        )
    })?;
    let next_url = validate_source_url(&next.url)?;
    if !matches!(next_url.scheme(), "http" | "https") {
        bail!(
            "{}",
            crate::i18n::ui(
                "Yenilenen doğrudan kaynak için güçlü HTTP içerik kimliği gerekli",
                "Refreshing a direct source requires a strong HTTP content identity",
            )
        );
    }
    let remote = probe_http(next, next_url, control, network)?;
    if control.stop_requested() {
        bail!(
            "{}",
            crate::i18n::ui("Kaynak yenileme durduruldu", "Source refresh stopped",)
        );
    }
    if state.total != remote.total {
        bail!(
            "{}",
            crate::i18n::ui(
                "Yenilenen kaynağın boyutu değişti; kısmi dosyalar korundu",
                "The refreshed source changed size; partial files were kept",
            )
        );
    }
    let same_strong_etag =
        state
            .etag
            .as_deref()
            .zip(remote.etag.as_deref())
            .and_then(|(old, new)| {
                if is_strong_etag(old) && is_strong_etag(new) {
                    Some(old == new)
                } else {
                    None
                }
            });
    let previous_checksum = normalized_checksum(previous.checksum.as_deref());
    let next_checksum = normalized_checksum(next.checksum.as_deref());
    let same_checksum = state.version == STATE_VERSION
        && previous_checksum.is_some()
        && previous_checksum == next_checksum
        && state.checksum == next_checksum;
    if same_strong_etag == Some(false) || (same_strong_etag != Some(true) && !same_checksum) {
        bail!("{}",
            crate::i18n::ui(
                "Yenilenen kaynağın güçlü ETag veya tam SHA-256 kimliği eşleşmedi; kısmi dosyalar korundu",
                "The refreshed source's strong ETag or full SHA-256 identity did not match; partial files were kept",
            ));
    }
    state.url = source_url_digest(&remote.url);
    state.etag = remote.etag;
    state.last_modified = remote.last_modified;
    state.checksum = next_checksum;
    Ok(PreparedSourceRefresh {
        state_path: Some(state_path(output)),
        state: Some(state),
    })
}

/// Applies only the bounded local state update prepared by `prepare_source_refresh`.
pub(crate) fn stage_source_refresh(prepared: &PreparedSourceRefresh) -> Result<bool> {
    match (&prepared.state_path, &prepared.state) {
        (Some(path), Some(state)) => {
            write_state(path, state)?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

pub(crate) fn download(
    request: &AddRequest,
    output: &Path,
    control: &TransferControl,
    network: &NetworkGovernor,
    connections: u8,
    prepared: Option<RemoteInfo>,
    progress: &mut dyn FnMut(TransferProgress),
) -> Result<TransferResult> {
    let url = validate_source_url(&request.url)?;
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent).with_context(|| {
            crate::i18n::ui_format!(
                "Hedef klasör oluşturulamadı: {0}",
                "The destination folder could not be created: {0}",
                parent.display()
            )
        })?;
    }
    if control.stop_requested() {
        bail!(
            "{}",
            crate::i18n::ui("İndirme durduruldu", "Download stopped",)
        );
    }

    if url.scheme() == "ftp" {
        download_ftp(request, output, url, control, network, progress)
    } else {
        let again = url.clone();
        match download_http_from(
            request,
            url,
            output,
            control,
            network,
            connections,
            prepared,
            progress,
        ) {
            Err(error) => match error.downcast_ref::<DigestRetry>().map(|retry| retry.0) {
                Some(bytes) => {
                    control.mark_restarted();
                    progress(TransferProgress {
                        processing: false,
                        downloaded: 0,
                        total: Some(bytes),
                        speed: 0,
                        eta: None,
                        phase: error.to_string(),
                    });
                    download_http_from(
                        request,
                        again,
                        output,
                        control,
                        network,
                        connections,
                        None,
                        progress,
                    )
                }
                None => Err(error),
            },
            done => done,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn download_http_from(
    request: &AddRequest,
    url: Url,
    output: &Path,
    control: &TransferControl,
    network: &NetworkGovernor,
    connections: u8,
    prepared: Option<RemoteInfo>,
    progress: &mut dyn FnMut(TransferProgress),
) -> Result<TransferResult> {
    let mut remote = prepared
        .map(Ok)
        .unwrap_or_else(|| probe_http(request, url, control, network))?;
    if is_html_name(output) {
        remote.refuse_page = false;
    }
    if remote.refuse_page && is_web_page(remote.content_type.as_deref(), remote.attachment) {
        return Err(page_instead_of_file());
    }
    download_http(
        request,
        output,
        remote,
        control,
        network,
        connections.clamp(1, 16),
        progress,
    )
}

/// Resumes retained work only when the server proves the file is unchanged. When it cannot
/// (no validators, a changed resource, a Range request answered with a full body) the
/// partial is discarded and the download restarts from zero, so two versions are never mixed.
fn download_http(
    request: &AddRequest,
    output: &Path,
    remote: RemoteInfo,
    control: &TransferControl,
    network: &NetworkGovernor,
    connections: u8,
    progress: &mut dyn FnMut(TransferProgress),
) -> Result<TransferResult> {
    match download_http_resuming(
        request,
        output,
        remote.clone(),
        control,
        network,
        connections,
        false,
        progress,
    ) {
        Err(error) if preserves_partial(&error) && !control.stop_requested() => {
            let ranges_ignored = ranges_ignored(&error);
            restart_from_zero(output, remote.total, control, progress);
            download_http_resuming(
                request,
                output,
                remote,
                control,
                network,
                connections,
                ranges_ignored,
                progress,
            )
        }
        other => other,
    }
}

/// Drops every retained part of `output` and reports that the transfer starts over from byte zero.
fn restart_from_zero(
    output: &Path,
    total: Option<u64>,
    control: &TransferControl,
    progress: &mut dyn FnMut(TransferProgress),
) {
    control.mark_restarted();
    remove_if_exists(&part_path(output));
    remove_if_exists(&state_path(output));
    cleanup_legacy_segments(output);
    progress(TransferProgress {
        processing: false,
        downloaded: 0,
        total,
        speed: 0,
        eta: None,
        phase: crate::i18n::ui(
            "Kaynak kimliği kanıtlanamadı; baştan başlıyor",
            "The source identity could not be proven; restarting from the beginning",
        )
        .into(),
    });
}

#[allow(clippy::too_many_arguments)]
fn download_http_resuming(
    request: &AddRequest,
    output: &Path,
    mut remote: RemoteInfo,
    control: &TransferControl,
    network: &NetworkGovernor,
    connections: u8,
    ranges_ignored: bool,
    progress: &mut dyn FnMut(TransferProgress),
) -> Result<TransferResult> {
    let existing_state = read_state(&state_path(output));
    let probe = remote.clone();
    // Retained bytes are bound to the record written with them, not to what the advisory probe
    // reports now: the ranged requests below send the record's validator in `If-Range` and
    // accept only a `Content-Range` with the record's total. Each such response must also
    // carry a validator equal to the record's, unless this attempt's probe or a supplied
    // SHA-256 already vouches for the record; a probe that reports another ETag disputes it
    // (`retention`).
    if let Some(state) = &existing_state {
        remote.total = state.total;
        remote.etag = state.etag.clone();
        remote.last_modified = state.last_modified.clone();
    }
    // Dynamic ranges are reusable only with content identity, not merely a URL,
    // length, or timestamp. A caller-supplied full SHA-256 is proof only when
    // it does not conflict with present strong validators; mixed bytes can never
    // be published unless the final digest matches.
    let mut stable = has_resume_identity(&remote, request.checksum.as_deref());
    let segment_count_for = |remote: &RemoteInfo| {
        // Sockets beyond the per-host cap would only wait for a permit while holding a range.
        let allowed = u64::from(connections).min(u64::from(network.per_host_limit().max(1)));
        // A size the probe did not report is no reason to rule out ranges.
        remote.total.map_or(allowed, |total| {
            (total / MIN_SEGMENT_SIZE).max(1).min(allowed)
        }) as u8
    };
    let mut segment_count = segment_count_for(&remote);

    // With nothing on disk there is no identity to protect: the ranged probe, not the advisory
    // HEAD, describes the file that the following requests fetch.
    let fresh =
        existing_state.is_none() && !part_path(output).exists() && !legacy_segments_exist(output);
    let sequential_resume = existing_state.as_ref().is_some_and(|state| {
        state.version == STATE_VERSION && !state.segmented && part_path(output).exists()
    });
    let empty_dynamic_allocation = existing_state.as_ref().is_some_and(|state| {
        state.version == STATE_VERSION
            && state.segmented
            && state.completed.is_empty()
            && state_matches(
                state,
                &remote,
                true,
                state.connections,
                normalized_checksum(request.checksum.as_deref()).as_deref(),
            )
    });
    let reusable_ranges = existing_state
        .as_ref()
        .map(|state| {
            let has_completed = !state.completed.is_empty()
                || (state.version == 2
                    && (part_path(output).exists() || legacy_segments_exist(output)));
            has_completed
                && (state.version == 2 || state.segmented)
                && (state_matches(
                    state,
                    &remote,
                    state.segmented,
                    state.connections,
                    normalized_checksum(request.checksum.as_deref()).as_deref(),
                ) || legacy_state_matches(state, &remote))
        })
        .unwrap_or(false);
    if let Some(state) = existing_state
        .as_ref()
        .filter(|_| reusable_ranges || sequential_resume)
    {
        remote.retained = retention(state, &remote, &probe, request.checksum.as_deref());
    }
    // A fresh download sends the probe unless HEAD reported a size too small to split or an
    // earlier ranged request already showed that the server ignores ranges; its answer
    // supplies the total and the identity that HEAD may lack.
    let probe_wanted = if fresh {
        !ranges_ignored && segment_count > 1
    } else {
        !sequential_resume && stable && remote.total.is_some() && segment_count > 1
    };
    let range_supported = if reusable_ranges {
        true
    } else if probe_wanted {
        match verify_range_support(request, &remote, fresh, control, network) {
            Ok(Some(headers)) => {
                if fresh {
                    remote.total = headers.content_range.and_then(|range| range.total);
                    remote.etag = headers.etag;
                    remote.last_modified = headers.last_modified;
                    remote.repr_digest = headers
                        .repr_digest
                        .as_deref()
                        .and_then(digest::repr_sha256)
                        .or(remote.repr_digest.take());
                    segment_count = segment_count_for(&remote);
                    stable = has_resume_identity(&remote, request.checksum.as_deref());
                }
                segment_count > 1 && stable
            }
            Ok(None) => false,
            Err(AttemptFailure::ResourceChanged) => {
                return Err(preserved_partial(crate::i18n::ui(
                    "Kaynak kimliği değişti; doğrulanmış parçalar kullanılamaz, sonraki deneme indirmeyi baştan başlatır",
                    "The source identity changed; the verified parts cannot be reused and the next attempt restarts the download from the beginning",
                )))
            }
            Err(AttemptFailure::Other(error)) => return Err(error),
            // Probe transport and status failures leave range support unproven, so the
            // download continues over a single connection.
            Err(_) => false,
        }
    } else {
        false
    };
    if !sequential_resume && stable && remote.total.is_some() && range_supported {
        match download_segmented(
            request,
            output,
            &remote,
            control,
            network,
            segment_count,
            progress,
        ) {
            Ok(result) => return Ok(result),
            Err(
                AttemptFailure::RangeRejected
                | AttemptFailure::RangeIgnored
                | AttemptFailure::ResourceChanged,
            ) if read_state(&state_path(output)).is_some_and(|state| {
                    state.version == STATE_VERSION
                        && state.segmented
                        && state.completed.is_empty()
                        && state_matches(
                            &state,
                            &remote,
                            true,
                            connections,
                            normalized_checksum(request.checksum.as_deref()).as_deref(),
                        )
                }) =>
            {
                // No range was durably accepted, so truncating the allocated sparse
                // file cannot discard resumable data or combine different resources. A server
                // whose validators differ from one request to the next ends up here.
                return download_http_single(
                    request, output, &remote, control, network, progress, false,
                );
            }
            Err(failure @ (AttemptFailure::RangeRejected | AttemptFailure::RangeIgnored)) => {
                return Err(preserved_partial_after(&failure, crate::i18n::ui(
                    "Sunucu byte aralığını reddetti; doğrulanmış parçalar kullanılamaz, sonraki deneme indirmeyi baştan başlatır",
                    "The server rejected the byte range; the verified parts cannot be reused and the next attempt restarts the download from the beginning",
                )))
            }
            Err(AttemptFailure::ResourceChanged) => {
                return Err(preserved_partial(crate::i18n::ui(
                    "Kaynak kimliği değişti; doğrulanmış parçalar kullanılamaz, sonraki deneme indirmeyi baştan başlatır",
                    "The source identity changed; the verified parts cannot be reused and the next attempt restarts the download from the beginning",
                )))
            }
            Err(AttemptFailure::RetryableStatus {
                status,
                retry_after,
            }) => return Err(retryable_status_error(status, retry_after)),
            Err(AttemptFailure::RetryableTransport(error)) | Err(AttemptFailure::Other(error)) => {
                return Err(error)
            }
            Err(AttemptFailure::Interrupted) => bail!(
                "{}",
                crate::i18n::ui("İndirme durduruldu", "Download stopped")
            ),
        }
    }

    if empty_dynamic_allocation {
        return download_http_single(request, output, &remote, control, network, progress, false);
    }
    download_http_single(request, output, &remote, control, network, progress, true)
}

fn download_http_single(
    request: &AddRequest,
    output: &Path,
    remote: &RemoteInfo,
    control: &TransferControl,
    network: &NetworkGovernor,
    progress: &mut dyn FnMut(TransferProgress),
    permit_resume: bool,
) -> Result<TransferResult> {
    let part = part_path(output);
    let state_path = state_path(output);
    let previous = read_state(&state_path);
    let part_bytes = fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    let restart_empty_dynamic = !permit_resume
        && remote.total == Some(part_bytes)
        && previous.as_ref().is_some_and(|state| {
            state.version == STATE_VERSION
                && state.segmented
                && state.completed.is_empty()
                && state_matches(
                    state,
                    remote,
                    true,
                    state.connections,
                    normalized_checksum(request.checksum.as_deref()).as_deref(),
                )
        });
    let offset = if restart_empty_dynamic {
        0
    } else {
        previous
            .as_ref()
            .map_or(part_bytes, |state| resume_point(state, part_bytes))
    };
    let resumable = permit_resume
        && offset > 0
        && previous
            .as_ref()
            .map(|state| {
                state_matches(
                    state,
                    remote,
                    false,
                    1,
                    normalized_checksum(request.checksum.as_deref()).as_deref(),
                )
            })
            .unwrap_or(false)
        && remote.total.map(|total| offset <= total).unwrap_or(true);
    if !resumable && offset > 0 {
        if let Some(error) = previous
            .is_none()
            .then(|| unreadable_state(&state_path))
            .flatten()
        {
            return Err(error);
        }
        return Err(preserved_partial(crate::i18n::ui(
            "Kısmi dosyanın kaynak kimliği kanıtlanamadı; dosya kullanılamaz, sonraki deneme indirmeyi baştan başlatır",
            "The partial file's source identity could not be proven; it cannot be reused and the next attempt restarts the download from the beginning",
        )));
    }
    if resumable {
        settle_part(&part, offset)?;
    }
    if remote.total == Some(offset) && offset > 0 && resumable {
        return finalize_http(
            request,
            remote,
            None,
            network,
            &final_name(output, request, remote, None),
            &part,
            &state_path,
            offset,
            control,
        );
    }
    if restart_empty_dynamic {
        let file = OpenOptions::new()
            .write(true)
            .open(&part)
            .with_context(|| {
                crate::i18n::ui_format!(
                    "Geçici dosya açılamadı: {0}",
                    "The temporary file could not be opened: {0}",
                    part.display()
                )
            })?;
        file.set_len(0)?;
        file.sync_all()?;
    }

    write_state(&state_path, &single_state(request, remote, offset))?;

    let original = validate_source_url(&request.url)?;
    let mut current = remote.url.clone();
    let original_origin = remote.credential_origin.clone();
    let mut redirects = 0usize;
    let mut retries = 0u8;
    let mut refollowed = false;
    let mut asked = remote.clone();
    loop {
        let expected = if offset > 0 {
            ExpectedResponse::Range {
                start: offset,
                end: None,
                total: remote.total,
            }
        } else {
            ExpectedResponse::Full
        };
        let attempt = http_to_file(
            request,
            &current,
            &original_origin,
            &part,
            &state_path,
            offset,
            expected,
            &asked,
            control,
            network,
            1,
            progress,
        );
        match attempt {
            Ok((headers, written)) => {
                if is_redirect(headers.status) {
                    if redirects == MAX_REDIRECTS {
                        bail!(
                            "{}",
                            crate::i18n::ui(
                                "Çok fazla HTTP yönlendirmesi",
                                "Too many HTTP redirects",
                            )
                        );
                    }
                    redirects += 1;
                    current = redirected_url(&current, headers.location.as_deref())?;
                    asked.refuse_page = !is_html_name(output);
                    continue;
                }
                let bytes = offset.saturating_add(written);
                // A length that the response states is exact. A body that only the closing
                // connection ends can be cut short unnoticed, so it has to reach the length
                // the probe reported.
                let (total, exact) = match expected_total(expected, &headers, remote) {
                    Some(total) => (Some(total), true),
                    None if close_delimited(&headers) => (remote.total, false),
                    None => (None, true),
                };
                if let Some(total) = total {
                    if bytes < total || (exact && bytes > total) {
                        bail!(
                            "{}",
                            crate::i18n::ui_format!("Aktarım eksik kaldı: {bytes}/{total} bayt", "Transfer incomplete: {bytes}/{total} bytes", bytes = bytes, total = total)
                        );
                    }
                }
                return finalize_http(
                    request,
                    remote,
                    Some(&headers),
                    network,
                    &final_name(output, request, remote, Some(&headers)),
                    &part,
                    &state_path,
                    bytes,
                    control,
                );
            }
            Err(
                failure @ (AttemptFailure::RangeRejected
                | AttemptFailure::RangeIgnored
                | AttemptFailure::ResourceChanged),
            ) if offset > 0 => {
                return Err(preserved_partial_after(&failure, crate::i18n::ui(
                    "Sürdürme yanıtı kaynak kimliğini kanıtlamadı; kısmi dosya kullanılamaz, sonraki deneme indirmeyi baştan başlatır",
                    "The resume response did not prove the source identity; the partial file cannot be reused and the next attempt restarts the download from the beginning",
                )))
            }
            Err(AttemptFailure::RetryableStatus {
                status,
                retry_after,
            }) if retries + 1 < MAX_RANGE_ATTEMPTS => {
                let delay = retry_after.unwrap_or_else(|| range_retry_delay(retries));
                progress(TransferProgress {
                    processing: false,
                    downloaded: offset,
                    total: expected_total(expected, &ResponseHeaders::default(), remote),
                    speed: 0,
                    eta: None,
                    phase: crate::i18n::ui_format!("HTTP {status}; {0} sn sonra tek bağlantıyla yeniden denenecek", "HTTP {status}; retrying single connection in {0} s", delay.as_secs(), status = status),
                });
                wait_for_retry(control, delay)?;
                retries += 1;
            }
            Err(AttemptFailure::RetryableStatus {
                status,
                retry_after,
            }) => return Err(retryable_status_error(status, retry_after)),
            Err(AttemptFailure::RetryableTransport(_)) if retries + 1 < MAX_RANGE_ATTEMPTS => {
                let delay = range_retry_delay(retries);
                progress(TransferProgress {
                    processing: false,
                    downloaded: offset,
                    total: expected_total(expected, &ResponseHeaders::default(), remote),
                    speed: 0,
                    eta: None,
                    phase: crate::i18n::ui_format!("Geçici ağ hatası; {0} sn sonra tek bağlantıyla yeniden denenecek", "Transient network error; retrying single connection in {0} s", delay.as_secs()),
                });
                wait_for_retry(control, delay)?;
                retries += 1;
            }
            Err(AttemptFailure::Interrupted) => bail!(
                "{}",
                crate::i18n::ui("İndirme durduruldu", "Download stopped")
            ),
            // Nothing is written between two requests of this loop, so one request for the
            // original address is all the progress rule allows.
            Err(AttemptFailure::Other(error))
                if !refollowed && current != original && lost_to_expiry(&error) =>
            {
                refollowed = true;
                match refollow(request, &original, &current, &asked, control, network) {
                    Some(fresh) => current = fresh,
                    None if control.stop_requested() => bail!(
                        "{}",
                        crate::i18n::ui("İndirme durduruldu", "Download stopped")
                    ),
                    None => return Err(error),
                }
            }
            Err(AttemptFailure::RetryableTransport(error)) | Err(AttemptFailure::Other(error)) => {
                return Err(error)
            }
            Err(
                AttemptFailure::RangeRejected
                | AttemptFailure::RangeIgnored
                | AttemptFailure::ResourceChanged,
            ) => {
                bail!(
                    "{}",
                    crate::i18n::ui(
                        "Sunucu geçersiz veya değişmiş bir yanıt gönderdi",
                        "The server sent an invalid or changed response",
                    )
                )
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn http_to_file(
    request: &AddRequest,
    url: &Url,
    original_origin: &(String, String, u16),
    path: &Path,
    state: &Path,
    offset: u64,
    expected: ExpectedResponse,
    remote: &RemoteInfo,
    control: &TransferControl,
    network: &NetworkGovernor,
    divisor: u64,
    progress: &mut dyn FnMut(TransferProgress),
) -> std::result::Result<(ResponseHeaders, u64), AttemptFailure> {
    let mut easy = network
        .easy_for_url(url, control)
        .map_err(AttemptFailure::Other)?;
    configure_easy(&mut easy, url)?;
    easy.follow_location(false).map_err(anyhow::Error::from)?;
    if let ExpectedResponse::Range { start, end, .. } = expected {
        let range = end
            .map(|last| format!("{start}-{last}"))
            .unwrap_or_else(|| format!("{start}-"));
        easy.range(&range).map_err(anyhow::Error::from)?;
    }
    let headers_list = build_headers(
        request,
        url,
        original_origin,
        match expected {
            ExpectedResponse::Range { .. } => range_validator(remote),
            _ => None,
        },
    )?;
    easy.http_headers(headers_list)
        .map_err(anyhow::Error::from)?;

    let mut file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .read(true)
        .open(path)
        .with_context(|| {
            crate::i18n::ui_format!(
                "Geçici dosya açılamadı: {0}",
                "The temporary file could not be opened: {0}",
                path.display()
            )
        })
        .map_err(AttemptFailure::Other)?;
    if offset == 0 {
        file.set_len(0)
            .map_err(|e| AttemptFailure::Other(e.into()))?;
    }
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| AttemptFailure::Other(e.into()))?;
    let mut sink = SequentialSink::new(
        file,
        path,
        state,
        single_state(request, remote, offset),
        offset,
    )
    .map_err(AttemptFailure::Other)?;

    let response = Rc::new(RefCell::new(ResponseHeaders::default()));
    let callback_response = response.clone();
    let write_response = response.clone();
    let io_error = Rc::new(RefCell::new(None::<std::io::Error>));
    let write_error = io_error.clone();
    let started = Instant::now();
    let mut last_report = Instant::now();
    let mut last_bytes = offset;
    let mut throttle_started = Instant::now();
    let mut throttle_cost = 0u64;

    easy.progress(true).map_err(anyhow::Error::from)?;
    let perform_result = {
        let mut transfer = easy.transfer();
        transfer
            .header_function(move |line| {
                parse_header_line(&mut callback_response.borrow_mut(), line);
                true
            })
            .map_err(anyhow::Error::from)?;
        transfer
            .write_function(|data| {
                if control.stop_requested() {
                    return Ok(0);
                }
                if !response_allows_remote_body(&write_response.borrow(), expected, remote)
                    || page_refused(&write_response.borrow(), remote)
                {
                    return Ok(0);
                }
                if sink.written == 0 && matches!(expected, ExpectedResponse::Full) {
                    // The state has to describe the bytes before the first one is written.
                    let adopted = adopt_full_response(remote, &write_response.borrow());
                    if adopted.etag != remote.etag
                        || adopted.last_modified != remote.last_modified
                        || adopted.total != remote.total
                    {
                        if let Err(error) = sink.adopt(single_state(request, &adopted, 0)) {
                            *write_error.borrow_mut() = Some(std::io::Error::other(error));
                            return Ok(0);
                        }
                    }
                }
                if let Err(error) = sink.write(data) {
                    *write_error.borrow_mut() = Some(error);
                    return Ok(0);
                }
                throttle(
                    control,
                    divisor,
                    &mut throttle_started,
                    &mut throttle_cost,
                    data.len() as u64,
                );
                let now = Instant::now();
                if now.duration_since(last_report) >= Duration::from_millis(200) {
                    let elapsed = now.duration_since(last_report).as_secs_f64().max(0.001);
                    let current = offset.saturating_add(sink.written);
                    let speed = ((current.saturating_sub(last_bytes)) as f64 / elapsed) as u64;
                    let total = expected_total(expected, &write_response.borrow(), remote);
                    progress(TransferProgress {
                        processing: false,
                        downloaded: current,
                        total,
                        speed,
                        eta: eta(total, current, speed),
                        phase: crate::i18n::ui("İndiriliyor", "Downloading").into(),
                    });
                    last_report = now;
                    last_bytes = current;
                }
                Ok(data.len())
            })
            .map_err(anyhow::Error::from)?;
        transfer
            .progress_function(|_, _, _, _| !control.stop_requested())
            .map_err(anyhow::Error::from)?;
        transfer.perform()
    };

    let headers = response.borrow().clone();
    if control.stop_requested() {
        sink.checkpoint(Record::Required).map_err(|error| {
            AttemptFailure::Other(error.context(crate::i18n::ui(
                "Duraklatılan kısmi dosya diske yazılamadı",
                "The paused partial file could not be written to disk",
            )))
        })?;
        return Err(AttemptFailure::Interrupted);
    }
    if let Some(error) = io_error.borrow_mut().take() {
        return Err(AttemptFailure::Other(anyhow!(error).context(
            crate::i18n::ui(
                "Dosyaya yazılamadı; disk dolu veya erişim engellendi",
                "The file could not be written; the disk is full or access was denied",
            ),
        )));
    }
    if is_redirect(headers.status) {
        return Ok((headers, 0));
    }
    if matches!(headers.status, 429 | 503) {
        control.mark_throttled();
        return Err(AttemptFailure::RetryableStatus {
            status: headers.status,
            retry_after: headers.retry_after,
        });
    }
    if matches!(headers.status, 401 | 403 | 404 | 408 | 410)
        || is_transient_server_error(headers.status)
    {
        return Err(AttemptFailure::Other(anyhow!(HttpStatusError {
            status: headers.status
        })));
    }
    if headers.status == 0 {
        return match perform_result {
            Err(error) => Err(AttemptFailure::RetryableTransport(
                governed_transport_error(
                    &easy,
                    error,
                    crate::i18n::ui(
                        "Ağ aktarımı tamamlanamadı",
                        "The network transfer did not complete",
                    ),
                ),
            )),
            Ok(()) => Err(AttemptFailure::RangeRejected),
        };
    }
    if page_refused(&headers, remote) {
        return Err(AttemptFailure::Other(page_instead_of_file()));
    }
    if identity_changed(&headers, expected, remote) {
        return Err(AttemptFailure::ResourceChanged);
    }
    if !response_allows_body(&headers, expected) {
        return Err(range_refusal(&headers));
    }
    if let Err(error) = perform_result {
        let error = governed_transport_error(
            &easy,
            error,
            crate::i18n::ui(
                "Ağ aktarımı tamamlanamadı",
                "The network transfer did not complete",
            ),
        );
        if sink.written > 0 {
            sink.checkpoint(Record::Attempted).map_err(|sync_error| {
                AttemptFailure::Other(sync_error.context(crate::i18n::ui(
                    "Kısmi dosya diske yazılamadı",
                    "The partial file could not be written to disk",
                )))
            })?;
            return Err(AttemptFailure::Other(error.context(crate::i18n::ui(
                "Aktarım kesildi; kısmi dosya korundu",
                "The transfer was interrupted; the partial file was kept",
            ))));
        }
        return Err(AttemptFailure::RetryableTransport(error));
    }
    sink.checkpoint(Record::Skipped).map_err(|error| {
        AttemptFailure::Other(error.context(crate::i18n::ui(
            "Geçici dosya yazılamadı",
            "The temporary file could not be written",
        )))
    })?;
    let written = sink.written;
    let current = offset.saturating_add(written);
    let elapsed = started.elapsed().as_secs_f64().max(0.001);
    let speed = (written as f64 / elapsed) as u64;
    let total = expected_total(expected, &headers, remote);
    progress(TransferProgress {
        processing: false,
        downloaded: current,
        total,
        speed,
        eta: eta(total, current, speed),
        phase: crate::i18n::ui("İndiriliyor", "Downloading").into(),
    });
    Ok((headers, written))
}

#[derive(Debug, Clone, Copy)]
struct RangeWork {
    range: ByteRange,
    attempts: u8,
}

enum RangeCommand {
    /// A range for the worker, and the moment the scheduler handed it over, from which the wait
    /// for its first byte counts.
    Download(RangeWork, Instant),
    Stop,
}

enum RangeEvent {
    Complete {
        worker: usize,
        work: RangeWork,
        result: std::result::Result<(), AttemptFailure>,
    },
}

/// The rate at which `bytes` arrived in `elapsed`; `None` while no byte has arrived or no time
/// has passed, when there is nothing to compare.
fn bytes_per_second(bytes: u64, elapsed: Duration) -> Option<u64> {
    let seconds = elapsed.as_secs_f64();
    (bytes > 0 && seconds > 0.0).then(|| (bytes as f64 / seconds) as u64)
}

/// What an `Allowance` holds at one moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Standing {
    /// Bytes the worker has claimed from the front of the lease.
    claimed: u64,
    /// The size of the lease, which a cut shortens.
    size: u64,
}

impl Standing {
    fn pack(self) -> u64 {
        (self.claimed << 32) | self.size
    }

    fn unpack(word: u64) -> Self {
        Self {
            claimed: word >> 32,
            size: word & u64::from(u32::MAX),
        }
    }

    /// Bytes of the lease that the worker has not claimed.
    fn to_come(self) -> u64 {
        self.size - self.claimed
    }

    /// The bytes at the back of the lease that a cut hands to another worker: half of those not
    /// claimed.
    fn back(self) -> u64 {
        self.to_come() / 2
    }

    /// The size to cut the lease to so that its back half goes to another worker. `None` when
    /// that half would be smaller than `MIN_SEGMENT_SIZE`, the smallest piece the scheduler
    /// cuts from a range that no worker has taken (`range_can_split`): a request for less is
    /// not worth its round trip, and the worker that keeps the front keeps at least as much.
    fn cut_size(self) -> Option<u64> {
        let back = self.back();
        (back >= MIN_SEGMENT_SIZE).then(|| self.size - back)
    }
}

/// The part of a lease that its worker may still write, in one word so that the worker
/// claiming bytes and the scheduler cutting the back off the lease are ordered by a single
/// compare-and-swap: a byte belongs to the worker or to the range cut off, never to both,
/// whichever comes first. A worker claims a chunk before it writes it, so the chunk in flight
/// counts as claimed, and it never writes beyond the size at the moment it claims. The high
/// half of the word is the bytes claimed, the low half the size; both count from the start of
/// the lease. Beside the word it holds how long the first byte of the lease took to come, from
/// which the scheduler tells how fast the lease runs once its bytes flow, how much of that wait
/// passed before the request that brought the byte was sent, and whether that request opened a
/// connection.
struct Allowance {
    word: AtomicU64,
    /// How long the worker waited for the first byte of the lease, in nanoseconds plus one; 0
    /// until that byte comes.
    first_byte: AtomicU64,
    /// How long after the worker was handed the lease the request that brought its first byte
    /// was sent, in nanoseconds.
    sent: AtomicU64,
    /// Whether the request for the lease opened a connection instead of reusing one.
    connected: AtomicBool,
}

impl Allowance {
    fn new() -> Self {
        Self {
            word: AtomicU64::new(0),
            first_byte: AtomicU64::new(0),
            sent: AtomicU64::new(0),
            connected: AtomicBool::new(false),
        }
    }

    /// Starts a lease of `size` bytes for the worker, which is idle.
    fn open(&self, size: u64) {
        let size = u32::try_from(size).expect("a lease is shorter than 4 GiB");
        let standing = Standing {
            claimed: 0,
            size: u64::from(size),
        };
        self.first_byte.store(0, Ordering::Release);
        self.sent.store(0, Ordering::Release);
        self.connected.store(false, Ordering::Release);
        self.word.store(standing.pack(), Ordering::Release);
    }

    fn view(&self) -> Standing {
        Standing::unpack(self.word.load(Ordering::Acquire))
    }

    /// Notes that the first byte of the lease came `wait` after the worker was handed it. The
    /// worker calls it before it claims that byte, so a scheduler that sees the byte claimed
    /// sees the wait too.
    fn note_first_byte(&self, wait: Duration) {
        let nanos = u64::try_from(wait.as_nanos()).unwrap_or(u64::MAX - 1);
        self.first_byte.store(nanos + 1, Ordering::Release);
    }

    /// How long the worker waited for the first byte; `None` until it came.
    fn first_byte_wait(&self) -> Option<Duration> {
        match self.first_byte.load(Ordering::Acquire) {
            0 => None,
            stored => Some(Duration::from_nanos(stored - 1)),
        }
    }

    /// Notes that the request that brought the first byte of the lease was sent `after` the
    /// worker was handed the lease. The worker calls it before `note_first_byte`.
    fn note_request(&self, after: Duration) {
        let nanos = u64::try_from(after.as_nanos()).unwrap_or(u64::MAX);
        self.sent.store(nanos, Ordering::Release);
    }

    /// How long the request itself took to get the first byte, out of the `wait` since the
    /// lease was handed over. What passed before the request was sent (a worker that starts
    /// late, an expired address that is renewed) is not part of what another request waits for.
    fn answer_wait(&self, wait: Duration) -> Duration {
        wait.saturating_sub(Duration::from_nanos(self.sent.load(Ordering::Acquire)))
    }

    /// Notes that the request for the lease opened a connection.
    fn note_connection(&self) {
        self.connected.store(true, Ordering::Release);
    }

    fn opened_connection(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    /// Claims up to `wanted` bytes from the front of what is left and returns how many.
    fn take(&self, wanted: u64) -> u64 {
        let mut word = self.word.load(Ordering::Acquire);
        loop {
            let seen = Standing::unpack(word);
            let granted = wanted.min(seen.to_come());
            if granted == 0 {
                return 0;
            }
            let next = Standing {
                claimed: seen.claimed + granted,
                ..seen
            };
            match self.word.compare_exchange_weak(
                word,
                next.pack(),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return granted,
                Err(current) => word = current,
            }
        }
    }

    /// Cuts the lease to `size` if it still stands as `seen`. `false` when the worker has
    /// claimed more, or the lease was cut, since `seen` was read.
    fn cut(&self, seen: Standing, size: u64) -> bool {
        debug_assert!(seen.claimed <= size && size < seen.size);
        let cut = Standing { size, ..seen };
        self.word
            .compare_exchange(seen.pack(), cut.pack(), Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}

/// How a worker received the last range of at least `MIN_SEGMENT_SIZE` it finished: how long
/// its request took to get the first byte, and the bytes per second from that byte on. A
/// shorter range can arrive in one burst, which shows a rate that no lasting transfer has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Delivery {
    wait: Duration,
    rate: u64,
}

impl Delivery {
    /// Whether a worker that received its last range this way is expected to have the back half
    /// of `lease` before the worker of the lease has all it has left at `lease_rate` bytes per
    /// second: `wait + back / rate < to_come / lease_rate`, multiplied through by both rates and
    /// by a second in nanoseconds so that a rate of 0 needs no division: a worker at 0 never
    /// wins, and any other wins against a lease at 0.
    fn gets_the_back_half_sooner(self, lease: Standing, lease_rate: u64) -> bool {
        const NANOS: u128 = 1_000_000_000;
        let (rate, lease_rate) = (u128::from(self.rate), u128::from(lease_rate));
        // The right side is below 2^127, so a left side that saturates is never smaller.
        let late = self
            .wait
            .as_nanos()
            .saturating_mul(rate)
            .saturating_mul(lease_rate)
            .saturating_add(u128::from(lease.back()) * lease_rate * NANOS);
        late < u128::from(lease.to_come()) * rate * NANOS
    }
}

/// A range handed to a worker, of which the first `taken` bytes are accounted for: they are in
/// the resume state, in the cycle that is running, or waiting for the next one. `range` ends
/// where the worker's allowance does, so a lease that was cut covers only the part it keeps.
struct Lease {
    range: ByteRange,
    taken: u64,
    /// When the range was handed to the worker, from which the wait for its first byte counts.
    started: Instant,
    /// How many times this range has failed before, which the range cut off from it inherits.
    attempts: u8,
}

impl Lease {
    /// How long the bytes of the lease have been flowing at `now`: the time since the range was
    /// handed out, less the `wait` for its first byte.
    fn flowing(&self, wait: Duration, now: Instant) -> Duration {
        now.saturating_duration_since(self.started)
            .saturating_sub(wait)
    }

    /// The part of the range that is not accounted for.
    fn rest(&self) -> Option<ByteRange> {
        let rest = ByteRange {
            start: self.range.start + self.taken,
            end: self.range.end,
        };
        (rest.len() > 0).then_some(rest)
    }
}

/// What a running cycle makes durable: the record it writes, and the ranges that record adds to
/// the previous one.
struct Snapshot {
    completed: Vec<ByteRange>,
    fresh: Vec<ByteRange>,
}

/// What the workers have written to the part file and what the resume state records of it.
///
/// A worker publishes how many bytes of its lease it has written, after each write returns. A
/// cycle starts by reading those counts, then flushes the part file, and only then records the
/// ranges the counts describe; bytes written after the counts were read are not claimed. The
/// state therefore lists only bytes that were written and then flushed.
///
/// Flushed means that every handle that wrote a recorded byte has been flushed. Each worker
/// writes through its own handle and the ledger keeps all of them open until the transfer ends,
/// so a handle is never closed with a recorded byte unflushed, whether its lease finished,
/// dropped or is still running; the cycle flushes each of them before it writes the record. The
/// record does not rest on a flush through one handle covering what another handle wrote.
///
/// The cycle flushes first through one more handle that no worker uses, on a helper thread
/// (`Cycle`), and that flush only keeps the stalls short: a flush holds back every other write
/// on its handle until it returns, about 30 ms for 64 MiB on an SSD and longer on slower disks.
/// Where a flush writes back the dirty data of the whole file, as on NTFS, it drains almost
/// everything and the flushes of the worker handles that follow hold their workers back for
/// well under a millisecond. Where a flush covers only the data of its own handle, each worker
/// waits for the flush of its own data and the record stays correct.
///
/// A running lease can be cut (`split`). Its range then ends where its worker's allowance does,
/// so the record never lists a byte of the lease beyond the cut, and the range cut off is a new
/// lease for an idle worker, with nothing recorded.
struct Checkpoints {
    cycle: Cycle<Snapshot>,
    /// The record's identity; `completed` is filled in at each write.
    state: ResumeState,
    total: u64,
    /// Ranges made durable by a cycle, sorted and merged. The record lists all of them except
    /// those of an end cycle whose record could not be written; the previous record stays then.
    completed: Vec<ByteRange>,
    /// Ranges that are written but in no cycle yet: finished leases and the written part of
    /// leases that dropped.
    unrecorded: Vec<ByteRange>,
    leases: Vec<Option<Lease>>,
    written: Vec<Arc<AtomicU64>>,
    allowances: Vec<Arc<Allowance>>,
    /// How each worker received the last range of at least `MIN_SEGMENT_SIZE` it finished;
    /// `None` before the first.
    deliveries: Vec<Option<Delivery>>,
    /// When each worker finished its last range; `None` before the first.
    idle_since: Vec<Option<Instant>>,
    /// How long the first byte took to come for the latest range that opened a connection;
    /// `None` before the first.
    connection_wait: Option<Duration>,
    pace: Pace,
}

impl Checkpoints {
    fn new(
        flush: File,
        writers: Vec<Arc<File>>,
        state_path: &Path,
        state: ResumeState,
        completed: Vec<ByteRange>,
        total: u64,
    ) -> Self {
        let workers = writers.len();
        Self {
            cycle: Cycle::new(
                flush,
                writers,
                state_path,
                Some(crate::i18n::ui(
                    "Aralık diske yazılamadı",
                    "The range could not be written to disk",
                )),
            ),
            state,
            total,
            completed,
            unrecorded: Vec::new(),
            leases: (0..workers).map(|_| None).collect(),
            written: (0..workers).map(|_| Arc::new(AtomicU64::new(0))).collect(),
            allowances: (0..workers).map(|_| Arc::new(Allowance::new())).collect(),
            deliveries: vec![None; workers],
            idle_since: vec![None; workers],
            connection_wait: None,
            pace: Pace::new(),
        }
    }

    /// The counter that `worker` raises as it writes its lease.
    fn counter(&self, worker: usize) -> Arc<AtomicU64> {
        self.written[worker].clone()
    }

    /// The allowance that bounds what `worker` writes of its lease.
    fn allowance(&self, worker: usize) -> Arc<Allowance> {
        self.allowances[worker].clone()
    }

    fn assign(&mut self, worker: usize, work: RangeWork, now: Instant) {
        // The worker is idle, so nothing writes its counter or claims from its allowance while
        // they are reset.
        self.written[worker].store(0, Ordering::Release);
        self.allowances[worker].open(work.range.len());
        self.leases[worker] = Some(Lease {
            range: work.range,
            taken: 0,
            started: now,
            attempts: work.attempts,
        });
    }

    /// Cuts the back half off a running lease for the `idle` worker that fetched its last range
    /// fastest, takes that worker out of `idle` and returns it with the range cut off. A lease
    /// is cut only when it has flowed for `MIN_RATE_SAMPLE` since its first byte, so that its
    /// rate is known, and the idle worker is expected to have the back half before the lease's
    /// own worker would reach the end of its range: the idle worker waits for its first byte as
    /// long as its request for its last range took and then fetches the half at the rate of
    /// that range, while the lease's worker fetches the whole remainder at the rate the lease
    /// has had since its first byte. An idle worker that finished its last range
    /// `RANGE_WORKER_IDLE_TIMEOUT` ago or earlier has closed its socket and has to open a
    /// connection for the half, so it waits at least as long as the request of the latest range
    /// that opened one took. Of the leases that qualify, the one with the most bytes still to
    /// come is cut; none is when none has at least twice `MIN_SEGMENT_SIZE` to come, or no idle
    /// worker has finished a range of that size yet, and `idle` is left as it is. The lease
    /// keeps the front: its worker writes no byte beyond the cut, however many are already in
    /// flight, and the cut is never before the last byte it has claimed.
    fn split(&mut self, idle: &mut Vec<usize>, now: Instant) -> Option<(usize, RangeWork)> {
        let (thief, delivery) = idle
            .iter()
            .filter_map(|&worker| Some((worker, self.deliveries[worker]?)))
            .max_by_key(|&(_, delivery)| delivery.rate)?;
        let closed = self.idle_since[thief]
            .is_some_and(|since| now.saturating_duration_since(since) >= RANGE_WORKER_IDLE_TIMEOUT);
        let delivery = match self.connection_wait {
            Some(setup) if closed => Delivery {
                wait: delivery.wait.max(setup),
                ..delivery
            },
            _ => delivery,
        };
        loop {
            let (worker, seen, size) = self
                .leases
                .iter()
                .enumerate()
                .filter_map(|(worker, lease)| {
                    let lease = lease.as_ref()?;
                    let seen = self.allowances[worker].view();
                    let size = seen.cut_size()?;
                    let wait = self.allowances[worker].first_byte_wait()?;
                    let flowed = lease.flowing(wait, now);
                    if flowed < MIN_RATE_SAMPLE {
                        return None;
                    }
                    let rate = bytes_per_second(seen.claimed, flowed)?;
                    delivery
                        .gets_the_back_half_sooner(seen, rate)
                        .then_some((worker, seen, size))
                })
                .max_by_key(|&(_, seen, _)| seen.to_come())?;
            if !self.allowances[worker].cut(seen, size) {
                // The worker claimed more in the meantime; look again.
                continue;
            }
            let lease = self.leases[worker].as_mut()?;
            let back = ByteRange {
                start: lease.range.start + size,
                end: lease.range.end,
            };
            lease.range.end = back.start;
            let work = RangeWork {
                range: back,
                attempts: lease.attempts,
            };
            idle.retain(|&candidate| candidate != thief);
            return Some((thief, work));
        }
    }

    fn recorded_bytes(&self) -> u64 {
        self.completed.iter().map(|range| range.len()).sum()
    }

    /// Bytes written that no cycle covers yet: finished and dropped leases, and the untaken
    /// part of the others.
    fn waiting(&self) -> u64 {
        let finished = self.unrecorded.iter().map(|range| range.len()).sum::<u64>();
        let running = self
            .leases
            .iter()
            .zip(&self.written)
            .filter_map(|(lease, written)| {
                let lease = lease.as_ref()?;
                Some(written.load(Ordering::Acquire).saturating_sub(lease.taken))
            })
            .sum::<u64>();
        finished + running
    }

    fn downloaded(&self) -> u64 {
        let in_cycle = self.cycle.covers().map_or(0, |snapshot| {
            snapshot.fresh.iter().map(|range| range.len()).sum()
        });
        self.recorded_bytes() + in_cycle + self.waiting()
    }

    /// Whether a periodic cycle should start now.
    fn due(&mut self) -> bool {
        if self.cycle.busy() {
            return false;
        }
        let waiting = self.waiting();
        self.pace.due(waiting)
    }

    /// The lease of `worker` ended with every byte written.
    fn finish(&mut self, worker: usize, now: Instant) {
        let Some(lease) = self.leases[worker].take() else {
            return;
        };
        self.idle_since[worker] = Some(now);
        if let Some(wait) = self.allowances[worker].first_byte_wait() {
            let answer = self.allowances[worker].answer_wait(wait);
            if self.allowances[worker].opened_connection() {
                self.connection_wait = Some(answer);
            }
            let length = lease.range.len();
            if length >= MIN_SEGMENT_SIZE {
                if let Some(rate) = bytes_per_second(length, lease.flowing(wait, now)) {
                    self.deliveries[worker] = Some(Delivery { wait: answer, rate });
                }
            }
        }
        if let Some(rest) = lease.rest() {
            self.unrecorded.push(rest);
        }
    }

    /// The lease of `worker` ended early. What it wrote waits for the next cycle, and the range
    /// that is returned for another try starts where the lease stopped.
    fn abandon(&mut self, worker: usize) -> Option<ByteRange> {
        let lease = self.leases[worker].take()?;
        let written = self.written[worker]
            .load(Ordering::Acquire)
            .max(lease.taken);
        if written > lease.taken {
            self.unrecorded.push(ByteRange {
                start: lease.range.start + lease.taken,
                end: lease.range.start + written,
            });
        }
        let rest = ByteRange {
            start: lease.range.start + written,
            end: lease.range.end,
        };
        (rest.len() > 0).then_some(rest)
    }

    /// Reads how much every lease has written and takes it, with the ranges that wait, into a
    /// record for the next cycle. `None` when nothing is waiting.
    fn snapshot(&mut self) -> std::result::Result<Option<(ResumeState, Snapshot)>, AttemptFailure> {
        let mut fresh = self.unrecorded.clone();
        let mut counted = Vec::new();
        for (worker, lease) in self.leases.iter().enumerate() {
            let Some(lease) = lease else { continue };
            let written = self.written[worker].load(Ordering::Acquire);
            if written > lease.taken {
                fresh.push(ByteRange {
                    start: lease.range.start + lease.taken,
                    end: lease.range.start + written,
                });
                counted.push((worker, written));
            }
        }
        if fresh.is_empty() {
            self.pace.restart();
            return Ok(None);
        }
        self.cycle.note("counted");
        let mut all = self.completed.clone();
        all.extend(fresh.iter().copied());
        let completed = normalize_completed(&all, self.total)?;
        self.unrecorded.clear();
        for (worker, written) in counted {
            if let Some(lease) = &mut self.leases[worker] {
                lease.taken = written;
            }
        }
        self.pace.restart();
        let record = ResumeState {
            completed: completed.clone(),
            ..self.state.clone()
        };
        Ok(Some((record, Snapshot { completed, fresh })))
    }

    /// Starts a periodic cycle beside the transfer. It fails only when the ranges cannot be
    /// merged into a record.
    fn begin(&mut self) -> std::result::Result<(), AttemptFailure> {
        if let Some((record, snapshot)) = self.snapshot()? {
            if let Some(ended) = self.cycle.start(record, snapshot) {
                let _ = self.absorb(ended);
            }
        }
        Ok(())
    }

    /// Takes in the outcome of a cycle. A failed one leaves the last record in place and its
    /// ranges wait for the next cycle.
    fn absorb(&mut self, (snapshot, result): (Snapshot, Result<()>)) -> Result<()> {
        match result {
            Ok(()) => {
                self.completed = snapshot.completed;
                Ok(())
            }
            Err(error) => {
                self.unrecorded.extend(snapshot.fresh);
                self.pace.fail();
                Err(error)
            }
        }
    }

    /// Takes in a periodic cycle that has ended; its failure does not end the transfer.
    fn poll(&mut self) {
        if let Some(ended) = self.cycle.poll() {
            let _ = self.absorb(ended);
        }
    }

    /// Makes everything written so far durable, after any cycle in progress, and records it as
    /// `record` says. No worker may be writing.
    fn checkpoint(&mut self, record: Record) -> std::result::Result<(), AttemptFailure> {
        if let Some(ended) = self.cycle.wait() {
            let _ = self.absorb(ended);
        }
        let Some((state, snapshot)) = self.snapshot()? else {
            return Ok(());
        };
        let result = self.cycle.run(&state, record).map(|_| ());
        self.absorb((snapshot, result))
            .map_err(AttemptFailure::Other)
    }
}

/// Writes all of `data` at `offset` without relying on the file position.
fn write_all_at(file: &File, mut data: &[u8], mut offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !data.is_empty() {
        let written = file.seek_write(data, offset)?;
        if written == 0 {
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        data = &data[written..];
        offset += written as u64;
    }
    Ok(())
}

fn download_segmented(
    request: &AddRequest,
    output: &Path,
    remote: &RemoteInfo,
    control: &TransferControl,
    network: &NetworkGovernor,
    count: u8,
    progress: &mut dyn FnMut(TransferProgress),
) -> std::result::Result<TransferResult, AttemptFailure> {
    // This is the connection allocation granted by the engine. Dynamic workers
    // never exceed it, even if this private entry point is reused later.
    let count = count.clamp(1, 16);
    let total = remote.total.ok_or_else(|| {
        AttemptFailure::Other(anyhow!(
            "{}",
            crate::i18n::ui("Dosya boyutu bilinmiyor", "The file size is unknown")
        ))
    })?;
    let part = part_path(output);
    let state_path = state_path(output);
    let mut state = prepare_dynamic_state(request, output, remote, total, count)?;
    let completed = normalize_completed(&state.completed, total)?;
    state.completed = completed.clone();
    write_state(&state_path, &state).map_err(AttemptFailure::Other)?;

    let mut pending = missing_ranges(total, &completed);
    let baseline = completed.iter().map(|range| range.len()).sum::<u64>();
    crate::output::require_space(output, total.saturating_sub(baseline))
        .map_err(AttemptFailure::Other)?;
    if baseline == total {
        return finalize_http(
            request,
            remote,
            None,
            network,
            &final_name(output, request, remote, None),
            &part,
            &state_path,
            total,
            control,
        )
        .map_err(AttemptFailure::Other);
    }

    let worker_count = usize::from(count.max(1));
    let lease_size = total
        .div_ceil(u64::from(count.max(1)))
        .clamp(MIN_SEGMENT_SIZE, MAX_SEGMENT_SIZE);
    // Every worker writes through its own handle, the checkpoints keep all of them to flush
    // them, and flush through one more.
    let open_part = || {
        OpenOptions::new()
            .write(true)
            .open(&part)
            .with_context(|| {
                crate::i18n::ui_format!(
                    "Geçici dosya açılamadı: {0}",
                    "The temporary file could not be opened: {0}",
                    part.display()
                )
            })
            .map_err(AttemptFailure::Other)
    };
    let flush_file = open_part()?;
    let worker_files = (0..worker_count)
        .map(|_| open_part().map(Arc::new))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut ledger = Checkpoints::new(
        flush_file,
        worker_files.clone(),
        &state_path,
        state,
        completed,
        total,
    );
    let (event_tx, event_rx) = mpsc::sync_channel::<RangeEvent>(worker_count * 2);
    // Raised when the scheduler gives up, so workers stop their current range at once
    // instead of finishing it for nobody.
    let abort = Arc::new(AtomicBool::new(false));
    // Ranges handed to workers and not yet reported complete; the speed limit is split
    // among them rather than among the planned connections.
    let busy = Arc::new(AtomicUsize::new(0));
    let mut senders = Vec::with_capacity(worker_count);
    let mut handles = Vec::with_capacity(worker_count);
    let writes = Writes::current();
    #[cfg(test)]
    let start_delay = TEST_START_DELAY.with(std::cell::Cell::get);
    let target = Arc::new(Target::new(
        validate_source_url(&request.url)
            .ok()
            .filter(|original| *original != remote.url),
        remote.url.clone(),
    ));
    for (worker, file) in worker_files.into_iter().enumerate() {
        let (command_tx, command_rx) = mpsc::sync_channel::<RangeCommand>(1);
        senders.push(command_tx);
        let events = event_tx.clone();
        let request = request.clone();
        let remote = remote.clone();
        let written = ledger.counter(worker);
        let allowance = ledger.allowance(worker);
        let control = control.clone();
        let abort = abort.clone();
        let busy = busy.clone();
        let writes = writes.clone();
        let target = target.clone();
        let network = NetworkGovernor::clone(network);
        handles.push(thread::spawn(move || {
            let mut downloader = DynamicRangeWorker {
                easy: None,
                easy_generation: 0,
                network,
                request: &request,
                remote: &remote,
                target: &target,
                file: &file,
                written: &written,
                allowance: &allowance,
                control: &control,
                abort: &abort,
                busy: &busy,
                writes: &writes,
            };
            loop {
                let command = match command_rx.recv_timeout(RANGE_WORKER_IDLE_TIMEOUT) {
                    Ok(command) => command,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        // A scheduler can leave a completed worker idle while another
                        // worker waits for its first socket. Release this worker's
                        // pooled socket, but retain the handle for consecutive ranges.
                        drop(downloader.easy.take());
                        continue;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                };
                let RangeCommand::Download(work, handed) = command else {
                    break;
                };
                #[cfg(test)]
                thread::sleep(start_delay);
                let result = downloader.download(work.range, handed);
                if events
                    .send(RangeEvent::Complete {
                        worker,
                        work,
                        result,
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
    }
    drop(event_tx);

    let started = Instant::now();
    let mut idle = (0..worker_count).rev().collect::<Vec<_>>();
    let mut active = 0usize;
    let mut effective_workers = worker_count;
    let mut retry_not_before = Instant::now();
    let mut last_split: Option<Instant> = None;
    let mut successful_since_failure = 0usize;
    let mut last_time = Instant::now();
    let mut last_downloaded = baseline;
    let outcome = 'scheduler: loop {
        if control.stop_requested() {
            break Err(AttemptFailure::Interrupted);
        }

        let now = Instant::now();
        while active < effective_workers && !idle.is_empty() {
            if now < retry_not_before {
                break;
            }
            let split_recently = last_split
                .map(|last| last.elapsed() < MIN_SPLIT_INTERVAL)
                .unwrap_or(false);
            let (worker, work) = if pending.is_empty() {
                // No range is left to hand out, so the fastest idle worker takes the back of a
                // running lease that it is expected to finish sooner than that lease's worker.
                if split_recently {
                    break;
                }
                let Some(taken) = ledger.split(&mut idle, now) else {
                    break;
                };
                last_split = Some(Instant::now());
                taken
            } else {
                let needs_split = pending
                    .iter()
                    .map(|work| work.range.len())
                    .max()
                    .is_some_and(|length| range_can_split(length, lease_size));
                if needs_split && split_recently {
                    break;
                }
                let work = take_largest_range(&mut pending, lease_size);
                if needs_split {
                    last_split = Some(Instant::now());
                }
                (idle.pop().expect("idle worker disappeared"), work)
            };
            ledger.assign(worker, work, now);
            busy.fetch_add(1, Ordering::AcqRel);
            if senders[worker]
                .send(RangeCommand::Download(work, now))
                .is_err()
            {
                break 'scheduler Err(AttemptFailure::Other(anyhow!(
                    "{}",
                    crate::i18n::ui(
                        "İndirme işçisi beklenmedik biçimde kapandı",
                        "A download worker closed unexpectedly",
                    )
                )));
            }
            active += 1;
        }

        if pending.is_empty() && active == 0 {
            break Ok(());
        }

        let now = Instant::now();
        let split_wait = last_split
            .map(|last| MIN_SPLIT_INTERVAL.saturating_sub(now.duration_since(last)))
            .unwrap_or(Duration::ZERO);
        let retry_wait = retry_not_before.saturating_duration_since(now);
        // With no range left to hand out, the next cut of a running lease is what an idle worker
        // waits for, and only the split interval can hold it back.
        let can_dispatch = active < effective_workers
            && !idle.is_empty()
            && (!pending.is_empty() || !split_wait.is_zero());
        let wait = if can_dispatch {
            // A live worker must not turn the split interval into a 200 ms
            // dispatch delay; wake as soon as the next eligible lease may start.
            split_wait.max(retry_wait).min(Duration::from_millis(200))
        } else {
            Duration::from_millis(200)
        };
        match event_rx.recv_timeout(wait) {
            Ok(RangeEvent::Complete {
                worker,
                work,
                result,
            }) => {
                active = active.saturating_sub(1);
                busy.fetch_sub(1, Ordering::AcqRel);
                idle.push(worker);
                match result {
                    Ok(()) => {
                        ledger.finish(worker, Instant::now());
                        successful_since_failure += 1;
                        if effective_workers < worker_count
                            && successful_since_failure >= effective_workers.max(1) * 2
                        {
                            effective_workers += 1;
                            successful_since_failure = 0;
                        }
                    }
                    Err(AttemptFailure::RetryableStatus {
                        status,
                        retry_after,
                    }) if work.attempts + 1 < MAX_RANGE_ATTEMPTS => {
                        let retry_after =
                            retry_after.unwrap_or_else(|| range_retry_delay(work.attempts));
                        let Some(not_before) = Instant::now().checked_add(retry_after) else {
                            break 'scheduler Err(AttemptFailure::Other(anyhow!(
                                "{}",
                                crate::i18n::ui(
                                    "Retry-After süresi desteklenen zaman aralığını aşıyor",
                                    "The Retry-After duration exceeds the supported time range",
                                )
                            )));
                        };
                        pending.extend(ledger.abandon(worker).map(|range| RangeWork {
                            range,
                            attempts: work.attempts + 1,
                        }));
                        effective_workers = (effective_workers / 2).max(1);
                        successful_since_failure = 0;
                        retry_not_before = retry_not_before.max(not_before);
                        progress(TransferProgress {
                            processing: false,
                            downloaded: ledger.downloaded(),
                            total: Some(total),
                            speed: 0,
                            eta: None,
                            phase: crate::i18n::ui_format!(
                                "HTTP {status}; {0} sn sonra {1} bağlantıyla yeniden denenecek",
                                "HTTP {status}; retrying in {0} s using {1} connections",
                                retry_after.as_secs(),
                                effective_workers,
                                status = status
                            ),
                        });
                    }
                    Err(AttemptFailure::RetryableTransport(_error))
                        if work.attempts + 1 < MAX_RANGE_ATTEMPTS =>
                    {
                        let retry_after = range_retry_delay(work.attempts);
                        let Some(not_before) = Instant::now().checked_add(retry_after) else {
                            break 'scheduler Err(AttemptFailure::Other(anyhow!(
                                "{}",
                                crate::i18n::ui(
                                    "Geri çekilme süresi desteklenen zaman aralığını aşıyor",
                                    "The retry backoff duration exceeds the supported time range",
                                )
                            )));
                        };
                        pending.extend(ledger.abandon(worker).map(|range| RangeWork {
                            range,
                            attempts: work.attempts + 1,
                        }));
                        effective_workers = (effective_workers / 2).max(1);
                        successful_since_failure = 0;
                        retry_not_before = retry_not_before.max(not_before);
                        progress(TransferProgress {
                            processing: false,
                            downloaded: ledger.downloaded(),
                            total: Some(total),
                            speed: 0,
                            eta: None,
                            phase: crate::i18n::ui_format!(
                                "Geçici ağ hatası; {0} sn sonra {1} bağlantıyla yeniden denenecek",
                                "Transient network error; retrying in {0} s using {1} connections",
                                retry_after.as_secs(),
                                effective_workers
                            ),
                        });
                    }
                    Err(AttemptFailure::RetryableTransport(error)) => {
                        break 'scheduler Err(AttemptFailure::Other(error))
                    }
                    Err(error) => break Err(error),
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                break Err(AttemptFailure::Other(anyhow!(
                    "{}",
                    crate::i18n::ui(
                        "Tüm indirme işçileri beklenmedik biçimde kapandı",
                        "All download workers closed unexpectedly",
                    )
                )))
            }
        }

        ledger.poll();
        if ledger.due() {
            if let Err(error) = ledger.begin() {
                break Err(error);
            }
        }

        let now = Instant::now();
        if now.duration_since(last_time) >= Duration::from_millis(200) {
            let downloaded = ledger.downloaded();
            let elapsed = now.duration_since(last_time).as_secs_f64().max(0.001);
            let speed = ((downloaded.saturating_sub(last_downloaded)) as f64 / elapsed) as u64;
            progress(TransferProgress {
                processing: false,
                downloaded: downloaded.min(total),
                total: Some(total),
                speed,
                eta: eta(Some(total), downloaded.min(total), speed),
                // Shown after the state: "Downloading — 8 connections".
                phase: if effective_workers == 1 {
                    crate::i18n::ui("1 bağlantı", "1 connection").to_string()
                } else {
                    crate::i18n::ui_format!("{0} bağlantı", "{0} connections", effective_workers)
                },
            });
            last_time = now;
            last_downloaded = downloaded;
        }
    };

    if outcome.is_err() {
        abort.store(true, Ordering::Release);
    }
    for sender in &senders {
        let _ = sender.try_send(RangeCommand::Stop);
    }
    drop(senders);
    // Nothing reads events any more: a worker blocked on a full channel with its final
    // result would otherwise never let the join below return.
    drop(event_rx);
    for handle in handles {
        if handle.join().is_err() && outcome.is_ok() {
            return Err(AttemptFailure::Other(anyhow!(
                "{}",
                crate::i18n::ui(
                    "İndirme iş parçacığı beklenmedik biçimde kapandı",
                    "The download thread closed unexpectedly",
                )
            )));
        }
    }
    // Every writer has stopped, so the counts are final. A pause, a stop or a failure keeps
    // what was written, and the record is what lets a resume use it; a source that rejected a
    // range, ignored ranges or changed is about to be discarded, and only what earlier
    // checkpoints recorded stands. The end of the download tries the record, so that a stop
    // before the file is published keeps what was received, and goes on without it: the record
    // is deleted when the file is published.
    let settled = match &outcome {
        Err(
            AttemptFailure::RangeRejected
            | AttemptFailure::RangeIgnored
            | AttemptFailure::ResourceChanged,
        ) => Ok(()),
        Ok(()) => ledger.checkpoint(Record::Attempted),
        Err(_) => ledger.checkpoint(Record::Required),
    };
    if let Err(failure) = outcome {
        return Err(match (failure, settled) {
            (AttemptFailure::Interrupted, Err(error)) => error,
            (failure, _) => failure,
        });
    }
    settled?;

    if ledger.completed
        != vec![ByteRange {
            start: 0,
            end: total,
        }]
    {
        return Err(AttemptFailure::Other(anyhow!(
            "{}",
            crate::i18n::ui(
                "Kalıcı aralık haritasında boşluk kaldı",
                "Gaps remained in the permanent range map",
            )
        )));
    }
    // The workers have ended, so dropping the ledger closes every handle on the part file before
    // it is renamed.
    drop(ledger);
    progress(TransferProgress {
        processing: false,
        downloaded: total,
        total: Some(total),
        speed: ((total.saturating_sub(baseline)) as f64
            / started.elapsed().as_secs_f64().max(0.001)) as u64,
        eta: Some(0),
        phase: crate::i18n::ui("Tamamlanıyor", "Completing").into(),
    });
    cleanup_legacy_segments(output);
    finalize_http(
        request,
        remote,
        None,
        network,
        &final_name(output, request, remote, None),
        &part,
        &state_path,
        total,
        control,
    )
    .map_err(AttemptFailure::Other)
}

/// The address that the requests of a segmented download go to. When it was reached through the
/// redirects of the address the download was asked for, it can expire (a signed storage address,
/// for example); a worker whose request it refused with 401, 403 or 410 asks `renew` for a fresh
/// one, which is what asking the original address again yields.
struct Target {
    /// The address the download was asked for; `None` when the download goes to it directly.
    original: Option<Url>,
    state: Mutex<TargetState>,
    /// Signalled when a renewal ends.
    renewed: Condvar,
    /// One more than the newest generation from which a byte was written, 0 before any.
    delivered: AtomicU32,
}

struct TargetState {
    url: Url,
    /// How many times the target has been renewed.
    generation: u32,
    /// Renewing failed or was refused, so the refusals that follow stand as they are.
    spent: bool,
    /// A worker is asking the original address; the lock is not held meanwhile.
    renewing: bool,
}

impl Target {
    fn new(original: Option<Url>, url: Url) -> Self {
        Self {
            original,
            state: Mutex::new(TargetState {
                url,
                generation: 0,
                spent: false,
                renewing: false,
            }),
            renewed: Condvar::new(),
            delivered: AtomicU32::new(0),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, TargetState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The address to request now and its generation. It never waits: a worker that is refused
    /// by an address that is being replaced gives its connection up in `renew`.
    fn current(&self) -> (Url, u32) {
        let state = self.lock();
        (state.url.clone(), state.generation)
    }

    /// Notes that a byte of generation `generation` has been written.
    fn note_delivery(&self, generation: u32) {
        self.delivered.fetch_max(generation + 1, Ordering::AcqRel);
    }

    /// Whether the worker whose request to generation `failed` was refused should send it again.
    /// A newer generation is used as it is. Otherwise one worker at a time makes the next one
    /// with `follow`, without the lock, and the others wait for its result, giving up when
    /// `stopped` says so: the first generation is renewed on its first refusal, a later one only
    /// after a byte was written from it, so a source whose fresh addresses are refused at once
    /// ends the transfer with the refusal. A failed renewal is final. The caller holds no
    /// connection while it waits.
    fn renew(
        &self,
        failed: u32,
        stopped: impl Fn() -> bool,
        follow: impl FnOnce(&Url, &Url) -> Option<Url>,
    ) -> bool {
        let Some(original) = &self.original else {
            return false;
        };
        let mut state = self.lock();
        loop {
            if state.generation != failed {
                return true;
            }
            if state.spent || stopped() {
                return false;
            }
            if !state.renewing {
                break;
            }
            state = self
                .renewed
                .wait_timeout(state, RENEWAL_POLL)
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
        if failed > 0 && self.delivered.load(Ordering::Acquire) <= failed {
            state.spent = true;
            return false;
        }
        state.renewing = true;
        let refused = state.url.clone();
        drop(state);
        let ending = RenewalEnd(self);
        let fresh = follow(original, &refused);
        let mut state = self.lock();
        match &fresh {
            Some(url) => {
                state.url = url.clone();
                state.generation += 1;
            }
            None => state.spent = true,
        }
        drop(state);
        drop(ending);
        fresh.is_some()
    }
}

/// Ends a renewal however it ends: the workers that wait for it are woken, and a renewal that
/// panicked counts as failed.
struct RenewalEnd<'a>(&'a Target);

impl Drop for RenewalEnd<'_> {
    fn drop(&mut self) {
        let mut state = self.0.lock();
        state.renewing = false;
        if thread::panicking() {
            state.spent = true;
        }
        drop(state);
        self.0.renewed.notify_all();
    }
}
struct DynamicRangeWorker<'a> {
    easy: Option<GovernedEasy>,
    /// The generation of the target that `easy` was made for.
    easy_generation: u32,
    network: NetworkGovernor,
    request: &'a AddRequest,
    remote: &'a RemoteInfo,
    target: &'a Target,
    /// This worker's own write handle on the part file.
    file: &'a File,
    /// Bytes of the current lease written so far, read by the scheduler's checkpoints.
    written: &'a AtomicU64,
    /// How much of the current lease this worker may write; the scheduler can cut it short.
    allowance: &'a Allowance,
    control: &'a TransferControl,
    abort: &'a AtomicBool,
    busy: &'a AtomicUsize,
    writes: &'a Writes,
}

impl DynamicRangeWorker<'_> {
    fn download(
        &mut self,
        range: ByteRange,
        handed: Instant,
    ) -> std::result::Result<(), AttemptFailure> {
        loop {
            let (url, generation) = self.target.current();
            let error = match self.request_range(range, &url, generation, handed) {
                Err(AttemptFailure::Other(error)) if lost_to_expiry(&error) => error,
                other => return other,
            };
            // The refused handle holds a connection slot that asking the original address may
            // need.
            self.easy = None;
            let (request, remote, control, network) =
                (self.request, self.remote, self.control, &self.network);
            let renewed = self.target.renew(
                generation,
                || control.stop_requested(),
                |original, failed| refollow(request, original, failed, remote, control, network),
            );
            if !renewed {
                return Err(if control.stop_requested() {
                    AttemptFailure::Interrupted
                } else {
                    AttemptFailure::Other(error)
                });
            }
        }
    }

    /// `handed` is when the scheduler handed the range to the worker, the moment the lease counts
    /// its wait for the first byte from. The time a worker takes to start is part of that wait:
    /// counted from the request, it would be taken for time in which the lease was flowing, and
    /// the lease would look slow. When the request was sent is noted apart
    /// (`Allowance::note_request`), because a worker that is expected to make a request of its
    /// own does not wait for the time before it.
    fn request_range(
        &mut self,
        range: ByteRange,
        url: &Url,
        generation: u32,
        handed: Instant,
    ) -> std::result::Result<(), AttemptFailure> {
        let abort = self.abort;
        if abort.load(Ordering::Acquire) {
            return Err(AttemptFailure::Interrupted);
        }
        if self.easy_generation != generation {
            self.easy = None;
            self.easy_generation = generation;
        }
        if self.easy.is_none() {
            let easy = self
                .network
                .easy_for_url(url, self.control)
                .map_err(AttemptFailure::Other)?;
            self.easy = Some(easy);
        }
        let easy = self.easy.as_mut().expect("range worker handle missing");
        let request = self.request;
        let remote = self.remote;
        let (file, counter) = (self.file, self.written);
        let (allowance, writes) = (self.allowance, self.writes);
        let (control, target) = (self.control, self.target);
        let busy = self.busy;
        // Do not reset a governed handle: libcurl reset clears its socket callbacks.
        // Reapplying request options preserves callback accounting and lets
        // immediately assigned ranges reuse the connection.
        configure_easy(easy, url)?;
        easy.range(&format!("{}-{}", range.start, range.end - 1))
            .map_err(anyhow::Error::from)?;
        easy.progress(true).map_err(anyhow::Error::from)?;
        easy.http_headers(build_headers(
            request,
            url,
            &remote.credential_origin,
            range_validator(remote),
        )?)
        .map_err(anyhow::Error::from)?;

        let sent = handed.elapsed();
        let response = Rc::new(RefCell::new(ResponseHeaders::default()));
        let callback_response = response.clone();
        let write_response = response.clone();
        let io_error = Rc::new(RefCell::new(None::<std::io::Error>));
        let write_error = io_error.clone();
        let mut written = 0u64;
        let mut throttle_started = Instant::now();
        let mut throttle_cost = 0u64;
        let expected = ExpectedResponse::Range {
            start: range.start,
            end: Some(range.end - 1),
            total: remote.total,
        };
        let perform_result = {
            let mut transfer = easy.transfer();
            transfer
                .header_function(move |line| {
                    parse_header_line(&mut callback_response.borrow_mut(), line);
                    true
                })
                .map_err(anyhow::Error::from)?;
            transfer
                .write_function(|data| {
                    if control.stop_requested() || abort.load(Ordering::Acquire) {
                        return Ok(0);
                    }
                    if !response_allows_remote_body(&write_response.borrow(), expected, remote)
                        || page_refused(&write_response.borrow(), remote)
                    {
                        return Ok(0);
                    }
                    let remaining = range.len().saturating_sub(written);
                    if data.len() as u64 > remaining {
                        return Ok(0);
                    }
                    if written == 0 {
                        allowance.note_request(sent);
                        allowance.note_first_byte(handed.elapsed());
                    }
                    // The bytes are claimed before they are written: a cut of the lease then
                    // leaves them to this worker, and what lies beyond the cut is not written.
                    let allowed = allowance.take(data.len() as u64) as usize;
                    if allowed > 0 {
                        if let Err(error) =
                            write_all_at(file, &data[..allowed], range.start + written)
                        {
                            *write_error.borrow_mut() = Some(error);
                            return Ok(0);
                        }
                        writes.note(range.start + written, allowed as u64);
                        if written == 0 {
                            target.note_delivery(generation);
                        }
                        written += allowed as u64;
                        counter.store(written, Ordering::Release);
                        throttle(
                            control,
                            busy.load(Ordering::Acquire) as u64,
                            &mut throttle_started,
                            &mut throttle_cost,
                            allowed as u64,
                        );
                    }
                    // Data beyond a cut stops the transfer, and that completes the lease.
                    Ok(if allowed < data.len() { 0 } else { allowed })
                })
                .map_err(anyhow::Error::from)?;
            transfer
                .progress_function(|_, _, _, _| {
                    // A lease that was cut and has everything up to the cut needs nothing more,
                    // however long the server stays silent.
                    let seen = allowance.view();
                    let spent = seen.size < range.len() && seen.to_come() == 0;
                    !control.stop_requested() && !abort.load(Ordering::Acquire) && !spent
                })
                .map_err(anyhow::Error::from)?;
            transfer.perform()
        };
        if easy.num_connects().is_ok_and(|connects| connects > 0) {
            allowance.note_connection();
        }

        let headers = response.borrow().clone();
        if control.stop_requested() || abort.load(Ordering::Acquire) {
            return Err(AttemptFailure::Interrupted);
        }
        if let Some(error) = io_error.borrow_mut().take() {
            return Err(AttemptFailure::Other(anyhow!(error).context(
                crate::i18n::ui(
                    "Dosyaya yazılamadı; disk dolu veya erişim engellendi",
                    "The file could not be written; the disk is full or access was denied",
                ),
            )));
        }
        if matches!(headers.status, 429 | 503) {
            control.mark_throttled();
            return Err(AttemptFailure::RetryableStatus {
                status: headers.status,
                retry_after: headers.retry_after,
            });
        }
        if matches!(headers.status, 401 | 403 | 404 | 408 | 410)
            || is_transient_server_error(headers.status)
        {
            return Err(AttemptFailure::Other(anyhow!(HttpStatusError {
                status: headers.status
            })));
        }
        if headers.status == 0 {
            return match perform_result {
                Err(error) => Err(AttemptFailure::RetryableTransport(
                    governed_transport_error(
                        easy,
                        error,
                        crate::i18n::ui(
                            "Ağ aktarımı tamamlanamadı",
                            "The network transfer did not complete",
                        ),
                    ),
                )),
                Ok(()) => Err(AttemptFailure::RangeRejected),
            };
        }
        if page_refused(&headers, remote) {
            return Err(AttemptFailure::Other(page_instead_of_file()));
        }
        if identity_changed(&headers, expected, remote) {
            return Err(AttemptFailure::ResourceChanged);
        }
        if !response_allows_body(&headers, expected) {
            return Err(range_refusal(&headers));
        }
        // The scheduler may have cut the lease short of the range that was requested.
        let size = allowance.view().size;
        if let Err(error) = perform_result {
            // The write callback ends the transfer when data arrives beyond a cut, and the
            // progress callback when none is left before it; either completes the lease once
            // every byte up to the cut is written.
            if size == range.len() || written != size {
                return Err(AttemptFailure::RetryableTransport(
                    governed_transport_error(
                        easy,
                        error,
                        crate::i18n::ui(
                            "Ağ aktarımı tamamlanamadı",
                            "The network transfer did not complete",
                        ),
                    ),
                ));
            }
        }
        if written != size {
            return Err(AttemptFailure::Other(anyhow!(
                "{}",
                crate::i18n::ui_format!(
                    "Byte aralığı eksik kaldı: {0}/{1}",
                    "Byte range incomplete: {0}/{1}",
                    written,
                    size
                )
            )));
        }
        Ok(())
    }
}

fn prepare_dynamic_state(
    request: &AddRequest,
    output: &Path,
    remote: &RemoteInfo,
    total: u64,
    connections: u8,
) -> std::result::Result<ResumeState, AttemptFailure> {
    let part = part_path(output);
    let state_path = state_path(output);
    let checksum = normalized_checksum(request.checksum.as_deref());
    let previous = read_state(&state_path);
    if let Some(state) = previous {
        if state.version == STATE_VERSION {
            if !state_matches(&state, remote, true, connections, checksum.as_deref()) {
                return Err(AttemptFailure::ResourceChanged);
            }
            if fs::metadata(&part).map(|value| value.len()).ok() != Some(total) {
                return Err(AttemptFailure::Other(preserved_partial(crate::i18n::ui(
                    "Aralık haritasının seyrek dosyası eksik veya kısa; sonraki deneme indirmeyi baştan başlatır",
                    "The range map's sparse file is missing or short; the next attempt restarts the download from the beginning",
                ))));
            }
            return Ok(ResumeState {
                url: source_url_digest(&remote.url),
                etag: remote.etag.clone(),
                last_modified: remote.last_modified.clone(),
                connections,
                checksum,
                ..state
            });
        }
        if state.version == 2 && legacy_state_matches(&state, remote) {
            return migrate_v2_state(output, remote, state, checksum, connections);
        }
        return Err(AttemptFailure::ResourceChanged);
    }
    if part.exists() || legacy_segments_exist(output) {
        if let Some(error) = unreadable_state(&state_path) {
            return Err(AttemptFailure::Other(error));
        }
        return Err(AttemptFailure::Other(preserved_partial(crate::i18n::ui(
            "Kaydı olmayan kısmi dosyaların kimliği kanıtlanamadı; sonraki deneme indirmeyi baştan başlatır",
            "The partial files have no state record, so their identity cannot be proven; the next attempt restarts the download from the beginning",
        ))));
    }
    crate::output::require_space(output, total).map_err(AttemptFailure::Other)?;
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&part)
        .with_context(|| {
            crate::i18n::ui_format!(
                "Geçici dosya oluşturulamadı: {0}",
                "The temporary file could not be created: {0}",
                part.display()
            )
        })
        .map_err(AttemptFailure::Other)?;
    mark_sparse(&file);
    file.set_len(total).map_err(|error| {
        AttemptFailure::Other(anyhow!(error).context(crate::i18n::ui(
            "Seyrek geçici dosya ayrılamadı",
            "The sparse temporary file could not be allocated",
        )))
    })?;
    file.sync_all()
        .map_err(|error| AttemptFailure::Other(error.into()))?;
    Ok(ResumeState {
        version: STATE_VERSION,
        url: source_url_digest(&remote.url),
        etag: remote.etag.clone(),
        last_modified: remote.last_modified.clone(),
        total: Some(total),
        segmented: true,
        connections,
        checksum,
        completed: Vec::new(),
        synced: None,
    })
}

/// Marks a range file sparse before it is extended, so unwritten ranges occupy no disk
/// space and a write far into the file does not make NTFS zero-fill everything before it.
/// Volumes without sparse support (FAT32, exFAT) keep the plain allocation.
fn mark_sparse(file: &File) {
    use std::os::windows::io::AsRawHandle;
    const FSCTL_SET_SPARSE: u32 = 0x0009_00C4;
    let mut returned = 0u32;
    unsafe {
        windows_sys::Win32::System::IO::DeviceIoControl(
            file.as_raw_handle(),
            FSCTL_SET_SPARSE,
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        );
    }
}

fn migrate_v2_state(
    output: &Path,
    remote: &RemoteInfo,
    old: ResumeState,
    checksum: Option<String>,
    connections: u8,
) -> std::result::Result<ResumeState, AttemptFailure> {
    let total = remote.total.expect("segmented transfer has total");
    let part = part_path(output);
    let mut completed = Vec::new();
    if old.segmented {
        if fs::metadata(&part).map(|value| value.len()).ok() == Some(total) {
            completed.push(ByteRange {
                start: 0,
                end: total,
            });
        } else {
            let mut target = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .read(true)
                .open(&part)
                .map_err(|error| AttemptFailure::Other(error.into()))?;
            mark_sparse(&target);
            target
                .set_len(total)
                .map_err(|error| AttemptFailure::Other(error.into()))?;
            let mut buffer = vec![0u8; IO_BUFFER];
            for (index, (start, end)) in
                split_ranges(total, old.connections).into_iter().enumerate()
            {
                let source_path = segment_path(output, index);
                let length = fs::metadata(&source_path)
                    .map(|value| value.len())
                    .unwrap_or(0);
                let maximum = end - start + 1;
                if length > maximum {
                    return Err(AttemptFailure::Other(anyhow!(
                        "{}",
                        crate::i18n::ui(
                            "Eski parça sınırını aşıyor; eserler korundu",
                            "The legacy part exceeds its range; the files were kept",
                        )
                    )));
                }
                if length == 0 {
                    continue;
                }
                let mut source = File::open(&source_path)
                    .map_err(|error| AttemptFailure::Other(error.into()))?;
                target
                    .seek(SeekFrom::Start(start))
                    .map_err(|error| AttemptFailure::Other(error.into()))?;
                let mut remaining = length;
                while remaining > 0 {
                    let amount = source
                        .read(&mut buffer[..remaining.min(IO_BUFFER as u64) as usize])
                        .map_err(|error| AttemptFailure::Other(error.into()))?;
                    if amount == 0 {
                        return Err(AttemptFailure::Other(anyhow!(
                            "{}",
                            crate::i18n::ui(
                                "Eski parça beklenmedik biçimde kısa",
                                "The legacy part is unexpectedly short",
                            )
                        )));
                    }
                    target
                        .write_all(&buffer[..amount])
                        .map_err(|error| AttemptFailure::Other(error.into()))?;
                    remaining -= amount as u64;
                }
                completed.push(ByteRange {
                    start,
                    end: start + length,
                });
            }
            target
                .sync_all()
                .map_err(|error| AttemptFailure::Other(error.into()))?;
        }
    } else {
        let length = fs::metadata(&part).map(|value| value.len()).unwrap_or(0);
        if length > total {
            return Err(AttemptFailure::Other(anyhow!(
                "{}",
                crate::i18n::ui(
                    "Eski kısmi dosya beklenen boyutu aşıyor; eser korundu",
                    "The legacy partial file exceeds the expected size; the file was kept",
                )
            )));
        }
        if length > 0 {
            OpenOptions::new()
                .write(true)
                .open(&part)
                .and_then(|file| file.sync_all())
                .map_err(|error| AttemptFailure::Other(error.into()))?;
            completed.push(ByteRange {
                start: 0,
                end: length,
            });
        }
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&part)
            .and_then(|file| {
                mark_sparse(&file);
                file.set_len(total)?;
                file.sync_all()
            })
            .map_err(|error| AttemptFailure::Other(error.into()))?;
    }
    let state = ResumeState {
        version: STATE_VERSION,
        url: source_url_digest(&remote.url),
        etag: remote.etag.clone(),
        last_modified: remote.last_modified.clone(),
        total: Some(total),
        segmented: true,
        connections,
        checksum,
        completed: normalize_completed(&completed, total)?,
        synced: None,
    };
    write_state(&state_path(output), &state).map_err(AttemptFailure::Other)?;
    cleanup_legacy_segments(output);
    Ok(state)
}

fn normalize_completed(
    ranges: &[ByteRange],
    total: u64,
) -> std::result::Result<Vec<ByteRange>, AttemptFailure> {
    let mut ranges = ranges.to_vec();
    ranges.sort_by_key(|range| (range.start, range.end));
    let mut normalized: Vec<ByteRange> = Vec::with_capacity(ranges.len());
    for range in ranges {
        if range.start >= range.end || range.end > total {
            return Err(AttemptFailure::Other(anyhow!(
                "{}",
                crate::i18n::ui(
                    "Kalıcı aralık haritası geçersiz",
                    "The permanent range map is invalid",
                )
            )));
        }
        if let Some(previous) = normalized.last_mut() {
            if range.start < previous.end {
                return Err(AttemptFailure::Other(anyhow!(
                    "{}",
                    crate::i18n::ui(
                        "Kalıcı aralık haritasında çakışma var",
                        "The permanent range map has overlapping ranges",
                    )
                )));
            }
            if range.start == previous.end {
                previous.end = range.end;
                continue;
            }
        }
        normalized.push(range);
    }
    Ok(normalized)
}

fn missing_ranges(total: u64, completed: &[ByteRange]) -> Vec<RangeWork> {
    let mut missing = Vec::with_capacity(completed.len() + 1);
    let mut offset = 0;
    for range in completed {
        if offset < range.start {
            missing.push(RangeWork {
                range: ByteRange {
                    start: offset,
                    end: range.start,
                },
                attempts: 0,
            });
        }
        offset = range.end;
    }
    if offset < total {
        missing.push(RangeWork {
            range: ByteRange {
                start: offset,
                end: total,
            },
            attempts: 0,
        });
    }
    missing
}

fn range_can_split(length: u64, maximum: u64) -> bool {
    length > maximum && length - maximum >= MIN_SEGMENT_SIZE
}

fn take_largest_range(pending: &mut Vec<RangeWork>, maximum: u64) -> RangeWork {
    let index = pending
        .iter()
        .enumerate()
        .max_by_key(|(_, work)| work.range.len())
        .map(|(index, _)| index)
        .expect("pending range missing");
    if !range_can_split(pending[index].range.len(), maximum) {
        return pending.swap_remove(index);
    }
    let start = pending[index].range.start;
    pending[index].range.start += maximum;
    RangeWork {
        range: ByteRange {
            start,
            end: start + maximum,
        },
        attempts: pending[index].attempts,
    }
}

fn legacy_segments_exist(output: &Path) -> bool {
    (0..16).any(|index| segment_path(output, index).exists())
}

fn cleanup_legacy_segments(output: &Path) {
    for index in 0..16 {
        remove_if_exists(&segment_path(output, index));
    }
}

fn download_ftp(
    request: &AddRequest,
    output: &Path,
    url: Url,
    control: &TransferControl,
    network: &NetworkGovernor,
    progress: &mut dyn FnMut(TransferProgress),
) -> Result<TransferResult> {
    let remote = probe_ftp(request, url, control, network)?;
    match download_ftp_resuming(request, output, &remote, control, network, progress) {
        Err(error) if preserves_partial(&error) && !control.stop_requested() => {
            restart_from_zero(output, remote.total, control, progress);
            download_ftp_resuming(request, output, &remote, control, network, progress)
        }
        other => other,
    }
}

/// Resumes a retained part only when the server's size and modification time match the
/// recorded state. Anything else (no `MDTM` or `SIZE`, a changed file, a server that refuses
/// `REST`) is an unproven partial that `download_ftp` replaces with a fresh download.
fn download_ftp_resuming(
    request: &AddRequest,
    output: &Path,
    remote: &RemoteInfo,
    control: &TransferControl,
    network: &NetworkGovernor,
    progress: &mut dyn FnMut(TransferProgress),
) -> Result<TransferResult> {
    let part = part_path(output);
    let state_path = state_path(output);
    let previous = read_state(&state_path);
    let part_bytes = fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    let offset = previous
        .as_ref()
        .map_or(part_bytes, |state| resume_point(state, part_bytes));
    let may_resume = offset > 0
        && previous
            .as_ref()
            .map(|state| ftp_state_matches(state, remote))
            .unwrap_or(false)
        && remote.last_modified.is_some()
        && remote.total.is_some_and(|total| offset <= total);
    if !may_resume && offset > 0 {
        if let Some(error) = previous
            .is_none()
            .then(|| unreadable_state(&state_path))
            .flatten()
        {
            return Err(error);
        }
        return Err(preserved_partial(crate::i18n::ui(
            "FTP kısmi dosyasının kimliği kanıtlanamadı; dosya kullanılamaz, sonraki deneme indirmeyi baştan başlatır",
            "The FTP partial file's identity could not be proven; it cannot be reused and the next attempt restarts the download from the beginning",
        )));
    }
    if may_resume {
        settle_part(&part, offset)?;
    }
    if may_resume && remote.total == Some(offset) {
        return finalize_part(
            output,
            &part,
            &state_path,
            offset,
            control,
            request.checksum.as_deref(),
        );
    }
    let record = ResumeState {
        version: STATE_VERSION,
        url: hex::encode(Sha256::digest(remote.url.as_str().as_bytes())),
        etag: None,
        last_modified: remote.last_modified.clone(),
        total: remote.total,
        segmented: false,
        connections: 1,
        checksum: normalized_checksum(request.checksum.as_deref()),
        completed: Vec::new(),
        synced: Some(offset),
    };
    write_state(&state_path, &record)?;

    let mut easy = network.ftp_easy(&remote.url, control)?;
    configure_easy(&mut easy, &remote.url)?;
    easy.http_headers(build_headers(
        request,
        &remote.url,
        &origin(&remote.url),
        None,
    )?)?;
    if offset > 0 {
        easy.resume_from(offset)?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&part)?;
    if offset == 0 {
        file.set_len(0)?;
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut sink = SequentialSink::new(file, &part, &state_path, record, offset)?;
    let mut last = Instant::now();
    let mut last_bytes = offset;
    let mut throttle_started = Instant::now();
    let mut throttle_cost = 0u64;
    let io_error = Rc::new(RefCell::new(None::<std::io::Error>));
    let write_error = io_error.clone();
    easy.progress(true)?;
    let result = {
        let mut transfer = easy.transfer();
        transfer.write_function(|data| {
            if control.stop_requested() {
                return Ok(0);
            }
            if let Err(error) = sink.write(data) {
                *write_error.borrow_mut() = Some(error);
                return Ok(0);
            }
            throttle(
                control,
                1,
                &mut throttle_started,
                &mut throttle_cost,
                data.len() as u64,
            );
            let now = Instant::now();
            if now.duration_since(last) >= Duration::from_millis(200) {
                let current = offset + sink.written;
                let elapsed = now.duration_since(last).as_secs_f64().max(0.001);
                let speed = ((current - last_bytes) as f64 / elapsed) as u64;
                progress(TransferProgress {
                    processing: false,
                    downloaded: current,
                    total: remote.total,
                    speed,
                    eta: eta(remote.total, current, speed),
                    phase: crate::i18n::ui("FTP indiriliyor", "Downloading over FTP").into(),
                });
                last = now;
                last_bytes = current;
            }
            Ok(data.len())
        })?;
        transfer.progress_function(|_, _, _, _| !control.stop_requested())?;
        transfer.perform()
    };
    if control.stop_requested() {
        sink.checkpoint(Record::Required).context(crate::i18n::ui(
            "Duraklatılan FTP dosyası diske yazılamadı",
            "The paused FTP file could not be written to disk",
        ))?;
        bail!(
            "{}",
            crate::i18n::ui("İndirme durduruldu", "Download stopped",)
        );
    }
    if let Some(error) = io_error.borrow_mut().take() {
        return Err(anyhow!(error).context(crate::i18n::ui(
            "Dosyaya yazılamadı; disk dolu veya erişim engellendi",
            "The file could not be written; the disk is full or access was denied",
        )));
    }
    if let Err(error) = result {
        if offset > 0 && error.code() == curl_sys::CURLE_FTP_COULDNT_USE_REST {
            return Err(preserved_partial(crate::i18n::ui(
                "FTP sunucusu kaldığı yerden sürdürmeyi kabul etmedi; sonraki deneme indirmeyi baştan başlatır",
                "The FTP server refused to resume; the next attempt restarts the download from the beginning",
            )));
        }
        sink.checkpoint(Record::Attempted).context(crate::i18n::ui(
            "Kısmi dosya diske yazılamadı",
            "The partial file could not be written to disk",
        ))?;
        return Err(governed_transport_error(
            &easy,
            error,
            crate::i18n::ui(
                "FTP aktarımı tamamlanamadı",
                "The FTP transfer did not complete",
            ),
        ));
    }
    sink.checkpoint(Record::Skipped).context(crate::i18n::ui(
        "Kısmi dosya diske yazılamadı",
        "The partial file could not be written to disk",
    ))?;
    let bytes = offset + sink.written;
    if let Some(total) = remote.total {
        if bytes != total {
            bail!(
                "{}",
                crate::i18n::ui_format!(
                    "FTP aktarımı eksik kaldı: {bytes}/{total} bayt",
                    "FTP transfer incomplete: {bytes}/{total} bytes",
                    bytes = bytes,
                    total = total
                )
            );
        }
    }
    drop(sink);
    finalize_part(
        output,
        &part,
        &state_path,
        bytes,
        control,
        request.checksum.as_deref(),
    )
}

fn probe_http(
    request: &AddRequest,
    mut url: Url,
    control: &TransferControl,
    network: &NetworkGovernor,
) -> Result<RemoteInfo> {
    let original_origin = origin(&url);
    let mut use_get = false;
    let mut redirects = 0;
    loop {
        if control.stop_requested() {
            bail!(
                "{}",
                crate::i18n::ui("İndirme durduruldu", "Download stopped",)
            );
        }
        let mut easy = network.easy_for_url(&url, control)?;
        configure_easy(&mut easy, &url)?;
        easy.nobody(!use_get)?;
        if use_get {
            easy.range("0-0")?;
        }
        easy.progress(true)?;
        easy.timeout(Duration::from_secs(45))?;
        easy.follow_location(false)?;
        easy.http_headers(build_headers(request, &url, &original_origin, None)?)?;
        let response = Rc::new(RefCell::new(ResponseHeaders::default()));
        let callback = response.clone();
        let result = {
            let mut transfer = easy.transfer();
            transfer.header_function(move |line| {
                parse_header_line(&mut callback.borrow_mut(), line);
                true
            })?;
            // Metadata only: intentionally abort at the first body chunk even if Range is ignored.
            transfer.write_function(|_| Ok(0))?;
            transfer.progress_function(|_, _, _, _| !control.stop_requested())?;
            transfer.perform()
        };
        if control.stop_requested() {
            bail!(
                "{}",
                crate::i18n::ui("İndirme durduruldu", "Download stopped",)
            );
        }
        let headers = response.borrow().clone();
        if is_redirect(headers.status) {
            if redirects == MAX_REDIRECTS {
                bail!(
                    "{}",
                    crate::i18n::ui("Çok fazla HTTP yönlendirmesi", "Too many HTTP redirects",)
                );
            }
            url = redirected_url(&url, headers.location.as_deref())?;
            redirects += 1;
            continue;
        }
        if !use_get && matches!(headers.status, 403 | 405 | 501) {
            use_get = true;
            continue;
        }
        if let Err(error) = result {
            if !(use_get && error.is_write_error() && headers.headers_complete) {
                return Err(governed_transport_error(
                    &easy,
                    error,
                    crate::i18n::ui(
                        "Sunucu bilgileri alınamadı",
                        "The server information could not be retrieved",
                    ),
                ));
            }
        }
        if matches!(headers.status, 429 | 503) {
            control.mark_throttled();
            return Err(retryable_status_error(headers.status, headers.retry_after));
        }
        if !(200..300).contains(&headers.status) {
            return Err(anyhow!(HttpStatusError {
                status: headers.status
            }));
        }
        let total = if headers.status == 206 {
            headers.content_range.and_then(|r| r.total)
        } else {
            headers.content_length
        };
        return Ok(RemoteInfo {
            url,
            credential_origin: original_origin,
            total,
            etag: headers.etag,
            last_modified: headers.last_modified,
            retained: Retained::None,
            content_type: headers.content_type,
            attachment: headers.attachment,
            filename: headers.filename,
            refuse_page: redirects > 0,
            repr_digest: headers.repr_digest.as_deref().and_then(digest::repr_sha256),
        });
    }
}

fn probe_ftp(
    request: &AddRequest,
    url: Url,
    control: &TransferControl,
    network: &NetworkGovernor,
) -> Result<RemoteInfo> {
    let mut easy = network.ftp_easy(&url, control)?;
    configure_easy(&mut easy, &url)?;
    easy.nobody(true)?;
    easy.progress(true)?;
    easy.timeout(Duration::from_secs(45))?;
    let signal = control.clone();
    easy.progress_function(move |_, _, _, _| !signal.stop_requested())?;
    easy.fetch_filetime(true)?;
    easy.http_headers(build_headers(request, &url, &origin(&url), None)?)?;
    if let Err(error) = easy.perform() {
        return Err(governed_transport_error(
            &easy,
            error,
            crate::i18n::ui(
                "FTP sunucu bilgileri alınamadı",
                "The FTP server information could not be retrieved",
            ),
        ));
    }
    let length = easy
        .content_length_download()
        .ok()
        .filter(|v| *v >= 0.0)
        .map(|v| v as u64);
    let modified = easy.filetime().ok().flatten().map(|v| v.to_string());
    let credential_origin = origin(&url);
    Ok(RemoteInfo {
        url,
        credential_origin,
        total: length,
        etag: None,
        last_modified: modified,
        retained: Retained::None,
        content_type: None,
        attachment: false,
        filename: None,
        refuse_page: false,
        repr_digest: None,
    })
}

/// Asks for the first byte. Range support is proven only by a 206 whose `Content-Range` names
/// that byte and a known total; `Accept-Ranges` and the answer to HEAD prove nothing. Returns
/// the response headers of such an answer. A 200 is not read: the body is abandoned at its
/// first chunk, so the download that follows is the only transfer of the full body.
///
/// With `fresh` nothing is on disk, so the request carries no `If-Range` and the answer's total
/// and validators are the caller's to adopt. Otherwise the answer has to match `remote`.
fn verify_range_support(
    request: &AddRequest,
    remote: &RemoteInfo,
    fresh: bool,
    control: &TransferControl,
    network: &NetworkGovernor,
) -> std::result::Result<Option<ResponseHeaders>, AttemptFailure> {
    let mut easy = network
        .easy_for_url(&remote.url, control)
        .map_err(AttemptFailure::Other)?;
    configure_easy(&mut easy, &remote.url)?;
    easy.range("0-0").map_err(anyhow::Error::from)?;
    easy.progress(true).map_err(anyhow::Error::from)?;
    easy.timeout(Duration::from_secs(30))
        .map_err(anyhow::Error::from)?;
    easy.follow_location(false).map_err(anyhow::Error::from)?;
    easy.http_headers(build_headers(
        request,
        &remote.url,
        &remote.credential_origin,
        range_validator(remote).filter(|_| !fresh),
    )?)
    .map_err(anyhow::Error::from)?;
    let response = Rc::new(RefCell::new(ResponseHeaders::default()));
    let header_callback = response.clone();
    let body_response = response.clone();
    let expected = ExpectedResponse::Range {
        start: 0,
        end: Some(0),
        total: remote.total.filter(|_| !fresh),
    };
    let result = {
        let mut transfer = easy.transfer();
        transfer
            .progress_function(|_, _, _, _| !control.stop_requested())
            .map_err(anyhow::Error::from)?;
        transfer
            .header_function(move |line| {
                parse_header_line(&mut header_callback.borrow_mut(), line);
                true
            })
            .map_err(anyhow::Error::from)?;
        transfer
            .write_function(move |data| {
                let headers = body_response.borrow();
                let allowed = if fresh {
                    response_allows_body(&headers, expected)
                } else {
                    response_allows_remote_body(&headers, expected, remote)
                };
                Ok(if allowed { data.len() } else { 0 })
            })
            .map_err(anyhow::Error::from)?;
        transfer.perform()
    };
    let headers = response.borrow().clone();
    if matches!(headers.status, 429 | 503) {
        control.mark_throttled();
    }
    if page_refused(&headers, remote) {
        return Err(AttemptFailure::Other(page_instead_of_file()));
    }
    if !fresh && validators_changed(&headers, remote) {
        return Err(AttemptFailure::ResourceChanged);
    }
    let known_total = headers
        .content_range
        .is_some_and(|range| range.total.is_some());
    Ok(
        (result.is_ok() && known_total && response_allows_body(&headers, expected))
            .then_some(headers),
    )
}

/// Whether the request failed because the address it went to is no longer valid.
fn lost_to_expiry(error: &anyhow::Error) -> bool {
    http_status(error).is_some_and(|status| matches!(status, 401 | 403 | 410))
}

/// Requests `original` once more after `failed`, an address that its redirects led to, answered
/// 401, 403 or 410, and returns the fresh address its redirects lead to now. The fresh address is
/// used only if it is not less secure than `failed` and a ranged request with `If-Range` shows it
/// to be the file that `record` describes: an equal strong ETag, or an equal `Last-Modified` with
/// the recorded size. Any failure, a page, or another identity gives `None`.
fn refollow(
    request: &AddRequest,
    original: &Url,
    failed: &Url,
    record: &RemoteInfo,
    control: &TransferControl,
    network: &NetworkGovernor,
) -> Option<Url> {
    let fresh = probe_http(request, original.clone(), control, network).ok()?;
    if failed.scheme() == "https" && fresh.url.scheme() != "https" {
        return None;
    }
    // Bytes are already on disk, so the response itself has to vouch for them, whatever the
    // record was vouched by when the transfer began.
    let candidate = RemoteInfo {
        url: fresh.url.clone(),
        retained: if record.retained == Retained::Disputed {
            Retained::Disputed
        } else {
            Retained::Unvouched
        },
        refuse_page: record.refuse_page && fresh.refuse_page,
        ..record.clone()
    };
    let Ok(Some(headers)) = verify_range_support(request, &candidate, false, control, network)
    else {
        return None;
    };
    // The check enforces the identity as the body arrives, and a one-byte answer may have none.
    let expected = ExpectedResponse::Range {
        start: 0,
        end: Some(0),
        total: candidate.total,
    };
    (!identity_changed(&headers, expected, &candidate)).then_some(fresh.url)
}

pub(crate) fn configure_easy(easy: &mut GovernedEasy, url: &Url) -> Result<()> {
    easy.url(url.as_str())?;
    easy.useragent(concat!("SSDownload/", env!("CARGO_PKG_VERSION")))?;
    easy.connect_timeout(Duration::from_secs(20))?;
    easy.low_speed_limit(1)?;
    easy.low_speed_time(Duration::from_secs(45))?;
    easy.ssl_verify_peer(true)?;
    easy.ssl_verify_host(true)?;
    easy.fail_on_error(false)?;
    easy.follow_location(false)?;
    // Through an HTTP proxy the CONNECT reply would otherwise reach the header callback and
    // read as the response itself (`status 200`), hiding a failed TLS handshake behind it.
    // CURLOPT_SUPPRESS_CONNECT_HEADERS is not wrapped by the curl crate.
    const CURLOPT_SUPPRESS_CONNECT_HEADERS: curl_sys::CURLoption = curl_sys::CURLOPTTYPE_LONG + 265;
    let code = unsafe {
        curl_sys::curl_easy_setopt(
            easy.raw(),
            CURLOPT_SUPPRESS_CONNECT_HEADERS,
            1 as std::os::raw::c_long,
        )
    };
    if code != curl_sys::CURLE_OK {
        bail!(
            "{}",
            crate::i18n::ui(
                "libcurl CONNECT başlıklarını gizleyemedi",
                "libcurl could not suppress the CONNECT headers",
            )
        );
    }
    // Site logins answer only for their own host (see `proxy::site_login`); every
    // redirect hop configures a new handle for its own URL, so credentials never follow.
    if let Some((username, password)) = crate::proxy::site_login(url) {
        let mut auth = curl::easy::Auth::new();
        auth.basic(true);
        easy.http_auth(&auth)?;
        easy.username(&username)?;
        easy.password(&password)?;
    }
    Ok(())
}

fn governed_transport_error(
    easy: &GovernedEasy,
    error: curl::Error,
    context: &'static str,
) -> anyhow::Error {
    let error = anyhow!(error).context(context);
    match easy.network_error() {
        Some(failure) => error.context(failure),
        None => error,
    }
}

fn build_headers(
    request: &AddRequest,
    current: &Url,
    original_origin: &(String, String, u16),
    if_range: Option<&str>,
) -> Result<List> {
    let same_origin = &origin(current) == original_origin;
    let mut list = List::new();
    for (name, value) in &request.headers {
        let lower = name.trim().to_ascii_lowercase();
        if name.contains(['\r', '\n', ':']) || value.contains(['\r', '\n']) {
            bail!(
                "{}",
                crate::i18n::ui("Geçersiz HTTP başlığı", "Invalid HTTP header",)
            );
        }
        if matches!(
            lower.as_str(),
            "host" | "content-length" | "range" | "if-range"
        ) {
            continue;
        }
        if !header_is_safe_across_origin(&lower, same_origin) {
            continue;
        }
        list.append(&format!("{}: {}", name.trim(), value))?;
    }
    if same_origin {
        if let Some(referer) = request.referer.as_deref() {
            if !referer.contains(['\r', '\n']) {
                list.append(&format!("Referer: {referer}"))?;
            }
        }
    }
    // Cookies the user consented to in the browser travel like the other session
    // headers: same origin only, and only the ones that apply to this exact URL.
    if same_origin
        && !request
            .headers
            .keys()
            .any(|name| name.trim().eq_ignore_ascii_case("cookie"))
    {
        let mut applicable = request
            .session_cookies
            .iter()
            .filter(|cookie| {
                !cookie.name.is_empty()
                    && !cookie.name.contains(['=', ';', '\r', '\n'])
                    && !cookie.value.contains([';', '\r', '\n'])
                    && crate::media::scoped_cookie_applies(cookie, current)
            })
            .collect::<Vec<_>>();
        applicable.sort_by_key(|cookie| std::cmp::Reverse(cookie.path.len()));
        if !applicable.is_empty() {
            let cookies = applicable
                .iter()
                .map(|cookie| format!("{}={}", cookie.name, cookie.value))
                .collect::<Vec<_>>()
                .join("; ");
            list.append(&format!("Cookie: {cookies}"))?;
        }
    }
    if let Some(validator) = if_range {
        list.append(&format!("If-Range: {validator}"))?;
    }
    Ok(list)
}

/// Credentials and site-specific request metadata must never follow a redirect
/// to another origin.  The short allow-list contains only browser-neutral
/// negotiation headers.
pub(crate) fn header_is_safe_across_origin(name: &str, same_origin: bool) -> bool {
    same_origin || matches!(name, "accept" | "accept-language" | "user-agent")
}

fn parse_header_line(headers: &mut ResponseHeaders, bytes: &[u8]) {
    let Ok(line) = std::str::from_utf8(bytes) else {
        return;
    };
    let trimmed = line.trim_end_matches(['\r', '\n']);
    if trimmed.starts_with("HTTP/") {
        *headers = ResponseHeaders::default();
        headers.http1 = trimmed.starts_with("HTTP/1.");
        headers.status = trimmed
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        return;
    }
    if trimmed.is_empty() {
        headers.headers_complete = true;
        return;
    }
    let Some((name, value)) = trimmed.split_once(':') else {
        return;
    };
    let value = value.trim();
    match name.trim().to_ascii_lowercase().as_str() {
        "location" => headers.location = Some(value.into()),
        "etag" => headers.etag = Some(value.into()),
        "last-modified" => headers.last_modified = Some(value.into()),
        "content-disposition" => {
            headers.filename = disposition_name(value);
            headers.attachment = value
                .split(';')
                .next()
                .is_some_and(|kind| kind.trim().eq_ignore_ascii_case("attachment"));
        }
        "content-type" => {
            headers.content_type = Some(
                value
                    .split(';')
                    .next()
                    .unwrap_or(value)
                    .trim()
                    .to_ascii_lowercase(),
            )
        }
        "content-length" => headers.content_length = value.parse().ok(),
        "transfer-encoding" => {
            headers.chunked = value.to_ascii_lowercase().contains("chunked");
        }
        "content-range" => headers.content_range = parse_content_range(value),
        "repr-digest" => {
            headers.repr_digest = Some(match headers.repr_digest.take() {
                Some(earlier) => format!("{earlier}, {value}"),
                None => value.into(),
            })
        }
        "retry-after" => headers.retry_after = parse_retry_after(value),
        _ => {}
    }
}

/// Longest server-requested pause honoured before a retry; a larger `Retry-After` is
/// clamped so a server cannot park a job indefinitely.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(3600);

pub(crate) fn parse_retry_after(value: &str) -> Option<Duration> {
    let delay = if let Ok(seconds) = value.trim().parse::<u64>() {
        Duration::from_secs(seconds)
    } else {
        let deadline = chrono::DateTime::parse_from_rfc2822(value.trim()).ok()?;
        let seconds = deadline
            .timestamp()
            .saturating_sub(chrono::Utc::now().timestamp());
        Duration::from_secs(seconds.max(0) as u64)
    };
    Some(delay.min(MAX_RETRY_AFTER))
}

fn range_retry_delay(attempt: u8) -> Duration {
    Duration::from_secs(1u64 << attempt.min(6))
}

fn wait_for_retry(control: &TransferControl, delay: Duration) -> Result<()> {
    let deadline = Instant::now().checked_add(delay).context(crate::i18n::ui(
        "Retry-After süresi desteklenen zaman aralığını aşıyor",
        "The Retry-After delay exceeds the supported time range",
    ))?;
    loop {
        if control.stop_requested() {
            bail!(
                "{}",
                crate::i18n::ui("İndirme durduruldu", "Download stopped",)
            );
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(());
        }
        thread::sleep(remaining.min(Duration::from_millis(200)));
    }
}

fn parse_content_range(value: &str) -> Option<ContentRange> {
    let value = value.trim();
    let rest = value.strip_prefix("bytes ")?;
    let (range, total) = rest.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let start = start.parse().ok()?;
    let end = end.parse().ok()?;
    if end < start {
        return None;
    }
    let total = if total == "*" {
        None
    } else {
        total.parse().ok()
    };
    if total.map(|length| end >= length).unwrap_or(false) {
        return None;
    }
    Some(ContentRange { start, end, total })
}

fn response_allows_body(headers: &ResponseHeaders, expected: ExpectedResponse) -> bool {
    if !headers.headers_complete {
        return false;
    }
    match expected {
        ExpectedResponse::Full => headers.status == 200,
        ExpectedResponse::Range { start, end, total } => {
            if headers.status != 206 {
                return false;
            }
            let Some(range) = headers.content_range else {
                return false;
            };
            range.start == start
                && end
                    .map(|expected_end| range.end == expected_end)
                    .unwrap_or(range.end >= start)
                && total
                    .map(|expected_total| range.total == Some(expected_total))
                    .unwrap_or(true)
        }
    }
}

fn response_allows_remote_body(
    headers: &ResponseHeaders,
    expected: ExpectedResponse,
    remote: &RemoteInfo,
) -> bool {
    response_allows_body(headers, expected) && !identity_changed(headers, expected, remote)
}

/// A full response starts the file from nothing, so nothing on disk depends on the probe's
/// validators and the response's own are adopted (`adopt_full_response`). Every ranged
/// response continues bytes that are already on disk and has to match them.
fn identity_changed(
    headers: &ResponseHeaders,
    expected: ExpectedResponse,
    remote: &RemoteInfo,
) -> bool {
    let (etag, last_modified) = (headers.etag.as_deref(), headers.last_modified.as_deref());
    let total = headers.content_range.and_then(|range| range.total);
    matches!(expected, ExpectedResponse::Range { .. })
        && (validators_changed(headers, remote)
            || match remote.retained {
                Retained::None | Retained::Vouched => false,
                Retained::Unvouched => !vouches_for_record(remote, etag, last_modified, total),
                Retained::Disputed => !saved_etag_matches(remote, etag),
            })
}

/// Whether the recorded ETag is strong and `etag` equals it.
fn saved_etag_matches(remote: &RemoteInfo, etag: Option<&str>) -> bool {
    remote.etag.as_deref().is_some_and(is_strong_etag) && etag == remote.etag.as_deref()
}

/// Whether the given validators and total positively identify the file that `remote` records:
/// an equal strong ETag, or an equal Last-Modified with the exact recorded total and no
/// ETag that disagrees with the recorded one (as in `modification_identity`). A missing
/// validator proves nothing.
fn vouches_for_record(
    remote: &RemoteInfo,
    etag: Option<&str>,
    last_modified: Option<&str>,
    total: Option<u64>,
) -> bool {
    let etags_disagree = remote
        .etag
        .as_deref()
        .zip(etag)
        .is_some_and(|(recorded, given)| recorded != given);
    let dated = remote
        .last_modified
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty())
        && last_modified == remote.last_modified.as_deref()
        && remote.total.is_some()
        && total == remote.total
        && !etags_disagree;
    saved_etag_matches(remote, etag) || dated
}

/// How the requests of an attempt that continues the bytes recorded in `state` are bound to
/// them. `record` is the remote filled from the state, `probe` is this attempt's HEAD.
///
/// A probe that reports another ETag disputes the record, and then neither a date, a
/// validator-less answer nor the probe itself proves anything. A supplied SHA-256 guards the
/// result, except against a strong ETag that conflicts with the recorded strong one.
fn retention(
    state: &ResumeState,
    record: &RemoteInfo,
    probe: &RemoteInfo,
    checksum: Option<&str>,
) -> Retained {
    let disputed = record
        .etag
        .as_deref()
        .zip(probe.etag.as_deref())
        .is_some_and(|(recorded, reported)| recorded != reported);
    let strong_conflict = disputed
        && record.etag.as_deref().is_some_and(is_strong_etag)
        && probe.etag.as_deref().is_some_and(is_strong_etag);
    let digest_guards = !strong_conflict
        && normalized_checksum(checksum)
            .is_some_and(|digest| state.checksum.as_deref() == Some(digest.as_str()));
    if digest_guards {
        Retained::Vouched
    } else if disputed {
        Retained::Disputed
    } else if vouches_for_record(
        record,
        probe.etag.as_deref(),
        probe.last_modified.as_deref(),
        probe.total,
    ) {
        Retained::Vouched
    } else {
        Retained::Unvouched
    }
}

/// A 200 whose end is marked only by the closing connection: nothing in the response tells a
/// complete body from a truncated one.
fn close_delimited(headers: &ResponseHeaders) -> bool {
    headers.http1 && headers.status == 200 && headers.content_length.is_none() && !headers.chunked
}

/// The file as a full response describes it. A length the response leaves out stays unknown;
/// the probe's, which may come from a HEAD, is not a substitute.
fn adopt_full_response(remote: &RemoteInfo, headers: &ResponseHeaders) -> RemoteInfo {
    RemoteInfo {
        total: headers.content_length,
        etag: headers.etag.clone(),
        last_modified: headers.last_modified.clone(),
        ..remote.clone()
    }
}

/// The size of the file as far as the answer to the request in flight knows it: a full
/// response states its own; a ranged one continues the file the record describes.
fn expected_total(
    expected: ExpectedResponse,
    headers: &ResponseHeaders,
    remote: &RemoteInfo,
) -> Option<u64> {
    match expected {
        ExpectedResponse::Full => headers.content_length,
        ExpectedResponse::Range { .. } => remote.total,
    }
}

/// Why a ranged request that did not conflict with the stored validators got no usable
/// partial answer: a 200 means the server does not honour ranges.
fn range_refusal(headers: &ResponseHeaders) -> AttemptFailure {
    if headers.status == 200 {
        AttemptFailure::RangeIgnored
    } else {
        AttemptFailure::RangeRejected
    }
}

fn single_state(request: &AddRequest, remote: &RemoteInfo, synced: u64) -> ResumeState {
    ResumeState {
        version: STATE_VERSION,
        url: source_url_digest(&remote.url),
        etag: remote.etag.clone(),
        last_modified: remote.last_modified.clone(),
        total: remote.total,
        segmented: false,
        connections: 1,
        checksum: normalized_checksum(request.checksum.as_deref()),
        completed: Vec::new(),
        synced: Some(synced),
    }
}

/// A missing validator neither proves nor disproves remote identity. Only two
/// present validators that disagree are a resource change.
fn validators_changed(headers: &ResponseHeaders, remote: &RemoteInfo) -> bool {
    remote
        .etag
        .as_ref()
        .zip(headers.etag.as_ref())
        .map(|(a, b)| a != b)
        .unwrap_or(false)
        || remote
            .last_modified
            .as_ref()
            .zip(headers.last_modified.as_ref())
            .map(|(a, b)| a != b)
            .unwrap_or(false)
}

fn state_matches(
    state: &ResumeState,
    remote: &RemoteInfo,
    segmented: bool,
    connections: u8,
    checksum: Option<&str>,
) -> bool {
    if state.segmented != segmented || state.total != remote.total {
        return false;
    }
    if state.version == 2 {
        return state.connections == connections && legacy_state_matches(state, remote);
    }
    if state.version != STATE_VERSION {
        return false;
    }
    let strong_etag = state
        .etag
        .as_deref()
        .zip(remote.etag.as_deref())
        .and_then(|(old, new)| {
            if is_strong_etag(old) && is_strong_etag(new) {
                Some(old == new)
            } else {
                None
            }
        });
    if strong_etag == Some(false) {
        return false;
    }
    let checksum = checksum
        .zip(state.checksum.as_deref())
        .map(|(old, new)| old.eq_ignore_ascii_case(new))
        .unwrap_or(false);
    strong_etag == Some(true) || checksum || modification_identity(state, remote)
}

/// The weaker identity many servers offer: the same `Last-Modified` and the same length,
/// with no disagreeing ETag of any kind. Every range request that relies on it carries
/// `If-Range`, so a changed resource answers with a full body the range check rejects.
fn modification_identity(state: &ResumeState, remote: &RemoteInfo) -> bool {
    let etags_disagree = state
        .etag
        .as_deref()
        .zip(remote.etag.as_deref())
        .is_some_and(|(old, new)| old != new);
    !etags_disagree
        && state.total.is_some()
        && state.total == remote.total
        && state
            .last_modified
            .as_deref()
            .zip(remote.last_modified.as_deref())
            .is_some_and(|(old, new)| !old.trim().is_empty() && old == new)
}

/// Whether the remote offers any identity a resumed or segmented transfer can be bound
/// to: a strong ETag, a caller checksum, or `Last-Modified` with a known length.
fn has_resume_identity(remote: &RemoteInfo, checksum: Option<&str>) -> bool {
    remote.etag.as_deref().is_some_and(is_strong_etag)
        || normalized_checksum(checksum).is_some()
        || (remote.total.is_some()
            && remote
                .last_modified
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty()))
}

fn legacy_state_matches(state: &ResumeState, remote: &RemoteInfo) -> bool {
    state.version == 2
        && state.url == source_url_digest(&remote.url)
        && state.total == remote.total
        && state
            .etag
            .as_deref()
            .zip(remote.etag.as_deref())
            .map(|(old, new)| is_strong_etag(old) && is_strong_etag(new) && old == new)
            .unwrap_or(false)
}

fn ftp_state_matches(state: &ResumeState, remote: &RemoteInfo) -> bool {
    (state.version == 2 || state.version == STATE_VERSION)
        && state.url == source_url_digest(&remote.url)
        && state.total == remote.total
        && state.last_modified == remote.last_modified
        && !state.segmented
}

fn source_url_digest(url: &Url) -> String {
    hex::encode(Sha256::digest(url.as_str().as_bytes()))
}

fn normalized_checksum(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    let value = value.strip_prefix("sha256:").unwrap_or(value).trim();
    (value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| value.to_ascii_lowercase())
}

fn split_ranges(total: u64, count: u8) -> Vec<(u64, u64)> {
    let count = count.max(1) as u64;
    let base = total / count;
    let remainder = total % count;
    let mut ranges = Vec::with_capacity(count as usize);
    let mut start = 0u64;
    for index in 0..count {
        let length = base + u64::from(index < remainder);
        if length == 0 {
            continue;
        }
        ranges.push((start, start + length - 1));
        start += length;
    }
    ranges
}

fn read_state(path: &Path) -> Option<ResumeState> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The error for a state file that exists but cannot be read (a virus scanner holds it for a
/// moment): it says nothing about the partial files beside it, so they are kept, and the I/O
/// error stays in the chain for the retry rules. A missing state file, or one that reads but does
/// not parse, cannot prove the partial files, so the caller restarts them.
fn unreadable_state(path: &Path) -> Option<anyhow::Error> {
    let error = fs::read(path)
        .err()
        .filter(|error| error.kind() != std::io::ErrorKind::NotFound)?;
    Some(anyhow::Error::new(error).context(crate::i18n::ui(
        "Devam kaydı okunamadı; kısmi dosyalar olduğu gibi korundu",
        "The resume record could not be read; the partial files were kept as they are",
    )))
}

fn write_state(path: &Path, state: &ResumeState) -> Result<()> {
    crate::recovery::atomic_write(path, &serde_json::to_vec(state)?)
}

fn finalize_part(
    output: &Path,
    part: &Path,
    state: &Path,
    bytes: u64,
    control: &TransferControl,
    checksum: Option<&str>,
) -> Result<TransferResult> {
    verify_checksum(part, checksum, control).inspect_err(|error| {
        // The part holds exactly the bytes that failed; keeping it would make every retry
        // verify the same bytes again instead of downloading them anew.
        if crate::error_codes::code_of(error) == Some(crate::error_codes::TRF_003) {
            remove_if_exists(part);
            remove_if_exists(state);
        }
    })?;
    publish_part(output, part, state, bytes, control)
}

/// Finishes an HTTP download. A SHA-256 the user gave decides alone and nothing else is
/// requested. Without one, the digest that the source publishes does: the `Repr-Digest` of the
/// response that delivered the bytes or, failing that, a file next to the address the download
/// was given. With no digest the file is published as it is, without reading it again. A file
/// that does not match the digest is downloaded once more; if that gives the same bytes the file
/// is published with a warning, and if it gives other bytes the job fails.
#[allow(clippy::too_many_arguments)]
fn finalize_http(
    request: &AddRequest,
    remote: &RemoteInfo,
    response: Option<&ResponseHeaders>,
    network: &NetworkGovernor,
    output: &Path,
    part: &Path,
    state: &Path,
    bytes: u64,
    control: &TransferControl,
) -> Result<TransferResult> {
    if request
        .checksum
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty())
    {
        return finalize_part(
            output,
            part,
            state,
            bytes,
            control,
            request.checksum.as_deref(),
        );
    }
    let header = response
        .and_then(|headers| headers.repr_digest.as_deref())
        .and_then(digest::repr_sha256)
        .or_else(|| remote.repr_digest.clone());
    let Some(published) = digest::published(request, header, bytes, control, network) else {
        return finalize_part(output, part, state, bytes, control, None);
    };
    let actual = sha256_of(part, control)?;
    if actual.eq_ignore_ascii_case(&published.sha256) {
        note_verified(published.source);
        crate::logging::record(
            crate::logging::Event::info("transfer.digest_verified")
                .outcome(crate::logging::Outcome::Ok)
                .detail(format!("source={}", published.source.label())),
        );
        return publish_part(output, part, state, bytes, control);
    }
    // A second download that gives the very bytes of the first rules out damage in transit, a
    // truncated body and a fault in assembling the segments: what is left is a digest that is
    // stale or describes another file, or a copy damaged on the server itself, which the
    // client cannot tell apart. The file is kept, as it would be with no digest at all.
    let record = digest::retry_record(state);
    if digest::first_download(&record).as_deref() == Some(actual.as_str()) {
        crate::logging::record(
            crate::logging::Event::warn("transfer.digest_mismatch_consistent")
                .outcome(crate::logging::Outcome::Partial)
                .detail(format!("source={}", published.source.label())),
        );
        let mut result = publish_part(output, part, state, bytes, control)?;
        result.warning = Some(crate::i18n::ui_format!(
            "Dosya kaydedildi, ancak kaynağın yayımladığı SHA-256 özetiyle ({0}) eşleşmiyor; iki indirme aynı baytları verdi",
            "The file was saved, but it does not match the SHA-256 digest its source publishes ({0}); two downloads gave the same bytes",
            published.source.label()
        ));
        return Ok(result);
    }
    Err(digest_mismatch(
        published.source,
        part,
        state,
        bytes,
        &actual,
    ))
}

/// Discards bytes with the SHA-256 `actual` that do not match the digest their source publishes.
/// The first mismatch of a job grants one fetch from the beginning; the grant, with the SHA-256
/// of these bytes, is recorded durably before they are discarded, so no restart can grant
/// another. A mismatch with the grant used up, whose bytes differ from the first download's,
/// ends the job with `SSD-TRF-003` and clears the record, so that retrying the failed job starts
/// anew.
fn digest_mismatch(
    source: digest::Source,
    part: &Path,
    state: &Path,
    bytes: u64,
    actual: &str,
) -> anyhow::Error {
    let record = digest::retry_record(state);
    let used = record.exists();
    let granted = digest::grant_retry(&record, actual);
    remove_if_exists(part);
    remove_if_exists(state);
    crate::logging::record(
        crate::logging::Event::warn("transfer.digest_mismatch")
            .outcome(if granted {
                crate::logging::Outcome::Retry
            } else {
                crate::logging::Outcome::Failed
            })
            .detail(format!("source={}", source.label())),
    );
    if granted {
        return anyhow!(DigestRetry(bytes));
    }
    remove_if_exists(&record);
    let message = if used {
        crate::i18n::ui_format!(
            "Dosya, kaynağın yayımladığı SHA-256 özetiyle ({0}) eşleşmedi ve iki indirme farklı bayt verdi; kısmi dosya silindi, yeniden denemede dosya baştan indirilir",
            "The file does not match the SHA-256 digest its source publishes ({0}), and two downloads gave different bytes; the partial file was deleted and a retry downloads it again",
            source.label()
        )
    } else {
        crate::i18n::ui(
            "SHA-256 doğrulaması başarısız oldu; kısmi dosya silindi, yeniden denemede dosya baştan indirilir",
            "SHA-256 verification failed; the partial file was deleted and a retry downloads it again",
        )
        .to_owned()
    };
    crate::error_codes::coded(crate::error_codes::TRF_003, message)
}

fn publish_part(
    output: &Path,
    part: &Path,
    state: &Path,
    bytes: u64,
    control: &TransferControl,
) -> Result<TransferResult> {
    if control.stop_requested() {
        bail!(
            "{}",
            crate::i18n::ui("İndirme durduruldu", "Download stopped",)
        )
    }
    OpenOptions::new().write(true).open(part)?.sync_all()?;
    let actual = crate::output::publish(part, output)?;
    remove_if_exists(state);
    remove_if_exists(&digest::retry_record(state));
    Ok(TransferResult {
        path: actual,
        bytes,
        warning: None,
    })
}

#[cfg(test)]
fn note_verified(source: digest::Source) {
    TEST_VERIFIED.with(|verified| verified.borrow_mut().push(source));
}

#[cfg(not(test))]
fn note_verified(_source: digest::Source) {}

fn verify_checksum(path: &Path, checksum: Option<&str>, control: &TransferControl) -> Result<()> {
    let Some(value) = checksum.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(());
    };
    let expected = value.strip_prefix("sha256:").unwrap_or(value).trim();
    if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!(
            "{}",
            crate::i18n::ui(
                "SHA-256 sağlama toplamı 64 onaltılık karakter olmalıdır",
                "The SHA-256 checksum must be 64 hexadecimal characters",
            )
        );
    }
    let actual = sha256_of(path, control)?;
    if !actual.eq_ignore_ascii_case(expected) {
        crate::bail_code!(
            crate::error_codes::TRF_003,
            "{}",
            crate::i18n::ui(
                "SHA-256 doğrulaması başarısız oldu; kısmi dosya silindi, yeniden denemede dosya baştan indirilir",
                "SHA-256 verification failed; the partial file was deleted and a retry downloads it again",
            )
        );
    }
    Ok(())
}

/// The SHA-256 of the file at `path` as lower-case hex; a stop request ends the reading.
fn sha256_of(path: &Path, control: &TransferControl) -> Result<String> {
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0u8; IO_BUFFER];
    loop {
        if control.stop_requested() {
            bail!(
                "{}",
                crate::i18n::ui("Doğrulama durduruldu", "Verification stopped",)
            )
        }
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(hex::encode(digest.finalize()))
}

fn part_path(output: &Path) -> PathBuf {
    append_suffix(output, ".ssdownload.part")
}
fn state_path(output: &Path) -> PathBuf {
    append_suffix(output, ".ssdownload.state")
}
fn segment_path(output: &Path, index: usize) -> PathBuf {
    append_suffix(output, &format!(".ssdownload.part.{index:03}"))
}
fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}
fn remove_if_exists(path: &Path) {
    let _ = fs::remove_file(path);
}

pub(crate) fn redirected_url(current: &Url, location: Option<&str>) -> Result<Url> {
    let location = location.ok_or_else(|| {
        anyhow!(
            "{}",
            crate::i18n::ui("Yönlendirme hedefi eksik", "The redirect target is missing")
        )
    })?;
    let next = current.join(location).context(crate::i18n::ui(
        "Yönlendirme hedefi geçersiz",
        "The redirect target is invalid",
    ))?;
    if current.scheme() == "https" && next.scheme() != "https" {
        bail!(
            "{}",
            crate::i18n::ui(
                "HTTPS bağlantısının şifresiz bir adrese yönlendirilmesi reddedildi.",
                "Redirecting an HTTPS connection to an insecure address was rejected.",
            )
        );
    }
    match next.scheme() {
        "http" | "https" => {}
        _ => bail!(
            "{}",
            crate::i18n::ui(
                "Güvenli olmayan yönlendirme reddedildi",
                "Unsafe redirect rejected",
            )
        ),
    }
    if next.host_str().is_none() {
        bail!(
            "{}",
            crate::i18n::ui(
                "Yönlendirme sunucusu eksik",
                "The redirect address has no host name",
            )
        );
    }
    Ok(next)
}

fn origin(url: &Url) -> (String, String, u16) {
    (
        url.scheme().to_ascii_lowercase(),
        url.host_str().unwrap_or_default().to_ascii_lowercase(),
        url.port_or_known_default().unwrap_or(0),
    )
}
fn is_redirect(status: u32) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}
/// A server failure that a later attempt can get past. 501 is left out: a server that does
/// not implement ranges may answer a Range request with it, which is a range rejection.
pub(crate) fn is_transient_server_error(status: u32) -> bool {
    (500..=599).contains(&status) && status != 501
}
fn is_strong_etag(value: &str) -> bool {
    !value.trim_start().starts_with("W/")
}
fn range_validator(remote: &RemoteInfo) -> Option<&str> {
    remote
        .etag
        .as_deref()
        .filter(|etag| is_strong_etag(etag))
        .or(remote.last_modified.as_deref())
}
pub(crate) fn request_requires_media_pipeline(request: &AddRequest) -> bool {
    request.format_id.is_some()
        || request.video_format_id.is_some()
        || request.container.is_some()
        || request.max_height.is_some()
        || request.exact_height.is_some()
        || request.audio_format_id.is_some()
        || request.audio_language.is_some()
        || !request.audio_tracks.is_empty()
        || request.subtitle_mode.is_some()
        || !request.external_subtitles.is_empty()
        || !request.subtitle_languages.is_empty()
        || !request.subtitle_tracks.is_empty()
        || request.audio_format.is_some()
        || request.expected_audio.is_some()
        || request.playlist
}

fn media_extension(url: &Url) -> bool {
    let path = url.path().to_ascii_lowercase();
    path.ends_with(".m3u8") || path.ends_with(".mpd")
}
fn is_media_content_type(value: &str) -> bool {
    matches!(
        value,
        "text/html"
            | "application/xhtml+xml"
            | "application/vnd.apple.mpegurl"
            | "application/x-mpegurl"
            | "application/dash+xml"
    )
}

/// An answer that is a web page and not a file: HTML that the server did not send as an
/// attachment.
fn is_web_page(content_type: Option<&str>, attachment: bool) -> bool {
    !attachment && matches!(content_type, Some("text/html" | "application/xhtml+xml"))
}

fn is_html_name(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            ["htm", "html", "xhtml"]
                .iter()
                .any(|html| extension.eq_ignore_ascii_case(html))
        })
}

/// Whether `headers` answer a file download with the web page that a redirect led it to. Such
/// a page is where a site sends a request that lacks the session, so it is never saved as the
/// file.
fn page_refused(headers: &ResponseHeaders, remote: &RemoteInfo) -> bool {
    remote.refuse_page
        && (200..300).contains(&headers.status)
        && is_web_page(headers.content_type.as_deref(), headers.attachment)
}

fn page_instead_of_file() -> anyhow::Error {
    crate::error_codes::coded(
        crate::error_codes::NET_004,
        crate::i18n::ui(
            "Kaynak dosya yerine bir oturum açma veya web sayfasıyla yanıt verdi. Dosyayı tarayıcıda indirin ya da eklentinin sağ tık menüsünden Bu site için oturum çerezlerine izin ver seçeneğini kullanıp bağlantıyı tarayıcıdan yeniden ekleyin (Bağlantıyı SSDownload ile indir).",
            "The source answered with a sign-in or web page instead of the file. Download it in the browser, or choose Allow session cookies for this site in the extension's context menu and add the link again from the browser (Download link with SSDownload).",
        ),
    )
}
fn eta(total: Option<u64>, downloaded: u64, speed: u64) -> Option<u64> {
    total
        .filter(|total| *total >= downloaded)
        .and_then(|total| (total - downloaded).checked_div(speed))
}
/// Paces a write of `written` bytes to a `workers`-th share of the speed limit. `cost` adds
/// each chunk's length times the worker count in force when it was written, so a change in
/// the number of busy workers never turns bytes counted under the old share into credit
/// that would let the aggregate rate exceed the limit.
pub(crate) fn throttle(
    control: &TransferControl,
    workers: u64,
    started: &mut Instant,
    cost: &mut u64,
    written: u64,
) {
    *cost = cost.saturating_add(written.saturating_mul(workers.max(1)));
    loop {
        let limit = control.speed_limit();
        if limit == 0 || control.stop_requested() {
            *started = Instant::now();
            *cost = 0;
            return;
        }
        let expected = Duration::from_secs_f64(*cost as f64 / limit as f64);
        let elapsed = started.elapsed();
        if expected <= elapsed {
            break;
        }
        thread::sleep((expected - elapsed).min(Duration::from_millis(50)));
    }
    if started.elapsed() >= Duration::from_secs(1) {
        *started = Instant::now();
        *cost = 0;
    }
}

fn final_name(
    output: &Path,
    request: &AddRequest,
    remote: &RemoteInfo,
    response: Option<&ResponseHeaders>,
) -> PathBuf {
    if request.filename.is_none() {
        if let Some(name) = response
            .and_then(|headers| headers.filename.as_ref())
            .or(remote.filename.as_ref())
        {
            return output.with_file_name(crate::engine::safe_filename(name, "download"));
        }
    }
    output.to_owned()
}
/// Decodes `%XX` escapes; `None` when an escape is malformed or the bytes are not UTF-8.
fn percent_decode_utf8(value: &str) -> Option<String> {
    let input = value.as_bytes();
    let mut bytes = Vec::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        if input[index] == b'%' {
            let high = (*input.get(index + 1)? as char).to_digit(16)?;
            let low = (*input.get(index + 2)? as char).to_digit(16)?;
            bytes.push((high * 16 + low) as u8);
            index += 3;
        } else {
            bytes.push(input[index]);
            index += 1;
        }
    }
    String::from_utf8(bytes).ok()
}

/// The last path segment of a URL as a file name. A parsed `Url` keeps its segments
/// percent-encoded, so `My%20File.pdf` must be decoded or the escapes would end up in
/// the name on disk.
pub(crate) fn url_file_name(url: &Url) -> Option<String> {
    let segment = url
        .path_segments()?
        .next_back()
        .filter(|segment| !segment.is_empty())?;
    Some(percent_decode_utf8(segment).unwrap_or_else(|| segment.to_owned()))
}

fn disposition_name(value: &str) -> Option<String> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    let mut escaped = false;
    for (index, ch) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if quoted && ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == '"' {
            quoted = !quoted;
        }
        if ch == ';' && !quoted {
            parts.push(&value[start..index]);
            start = index + 1;
        }
    }
    parts.push(&value[start..]);
    let mut plain = None;
    for part in parts.into_iter().skip(1) {
        let Some((key, value)) = part.trim().split_once('=') else {
            continue;
        };
        let value = value.trim();
        if key.eq_ignore_ascii_case("filename*") {
            let Some((charset, tail)) = value.split_once('\'') else {
                continue;
            };
            let Some((_, encoded)) = tail.split_once('\'') else {
                continue;
            };
            if !charset.eq_ignore_ascii_case("utf-8") {
                continue;
            }
            if let Some(name) = percent_decode_utf8(encoded).filter(|name| !name.is_empty()) {
                return Some(name);
            }
        } else if key.eq_ignore_ascii_case("filename") {
            let value = value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .unwrap_or(value);
            let mut name = String::new();
            let mut escape = false;
            for ch in value.chars() {
                if !escape && ch == '\\' {
                    escape = true;
                    continue;
                }
                name.push(ch);
                escape = false;
            }
            if !name.is_empty() {
                plain = Some(name);
            }
        }
    }
    plain
}

/// Moves only the named legacy partial set under the job's owned scratch directory.
/// An interrupted migration is repeatable; never replace an existing scratch file.
pub(crate) fn migrate_legacy_parts(original: &Path, scratch: &Path) -> Result<()> {
    use std::os::windows::fs::MetadataExt;
    let mut candidates = vec![part_path(original)];
    candidates.extend((0..16).map(|index| segment_path(original, index)));
    candidates.push(state_path(original));
    for source in candidates {
        if !source.exists() {
            continue;
        }
        let metadata = fs::symlink_metadata(&source)?;
        if !metadata.is_file() || metadata.file_attributes() & 0x400 != 0 {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Eski parça dosyası güvenle taşınamıyor",
                    "The legacy partial file cannot be moved safely",
                )
            );
        }
        let target = scratch
            .parent()
            .context(crate::i18n::ui(
                "Çalışma klasörü eksik",
                "The working folder is missing",
            ))?
            .join(source.file_name().context(crate::i18n::ui(
                "Parça adı eksik",
                "The part name is missing",
            ))?);
        if target.exists() {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Eski ve yeni çalışma dosyaları çakışıyor; dosyalar korundu",
                    "The legacy and new working files collide; the files were kept",
                )
            );
        }
        fs::rename(&source, &target).context(crate::i18n::ui(
            "Eski parça çalışma klasörüne taşınamadı",
            "The old part could not be moved to the working folder",
        ))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_http::Sequence;
    use std::{
        io::{BufRead, BufReader, Read, Write},
        net::{SocketAddr, TcpListener, TcpStream},
        sync::{mpsc, Mutex},
        time::SystemTime,
    };

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(label: &str) -> Self {
            let stamp = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "ssdownload-transfer-{label}-{}-{stamp}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn read_request(stream: &mut TcpStream) -> String {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0u8; 1024];
        while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = stream.read(&mut buffer).unwrap();
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..read]);
        }
        String::from_utf8(bytes).unwrap()
    }

    fn response(stream: &mut TcpStream, status: &str, headers: &[&str], body: &[u8]) {
        let mut head = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\n", body.len());
        for header in headers {
            head.push_str(header);
            head.push_str("\r\n");
        }
        head.push_str("Connection: close\r\n\r\n");
        stream.write_all(head.as_bytes()).unwrap();
        stream.write_all(body).unwrap();
        stream.flush().unwrap();
    }

    fn local_request(url: String) -> AddRequest {
        AddRequest {
            url,
            ..Default::default()
        }
    }

    fn remote_with_etag(etag: Option<&str>) -> RemoteInfo {
        let url = validate_source_url("https://example.test/payload.bin").unwrap();
        RemoteInfo {
            credential_origin: origin(&url),
            url,
            total: Some(100),
            etag: etag.map(str::to_owned),
            last_modified: None,
            retained: Retained::None,
            content_type: None,
            attachment: false,
            filename: None,
            refuse_page: false,
            repr_digest: None,
        }
    }

    #[test]
    fn rejects_malformed_and_out_of_bounds_content_ranges() {
        assert!(parse_content_range("bytes 10-19/20").is_some());
        assert!(parse_content_range("bytes 20-10/30").is_none());
        assert!(parse_content_range("bytes 0-20/20").is_none());
        assert!(parse_content_range("items 0-1/2").is_none());
    }

    #[test]
    fn range_response_must_match_the_exact_requested_segment() {
        let mut headers = ResponseHeaders {
            status: 206,
            headers_complete: true,
            ..Default::default()
        };
        headers.content_range = parse_content_range("bytes 0-9/100");
        assert!(response_allows_body(
            &headers,
            ExpectedResponse::Range {
                start: 0,
                end: Some(9),
                total: Some(100)
            }
        ));
        assert!(!response_allows_body(
            &headers,
            ExpectedResponse::Range {
                start: 10,
                end: Some(19),
                total: Some(100)
            }
        ));
        headers.status = 200;
        assert!(!response_allows_body(
            &headers,
            ExpectedResponse::Range {
                start: 0,
                end: Some(9),
                total: Some(100)
            }
        ));
    }

    #[test]
    fn valid_range_rejects_a_conflicting_remote_validator() {
        let remote = remote_with_etag(Some("\"fixture-v1\""));
        let mut headers = ResponseHeaders {
            status: 206,
            etag: Some("\"fixture-v2\"".into()),
            headers_complete: true,
            ..Default::default()
        };
        headers.content_range = parse_content_range("bytes 0-0/100");
        let expected = ExpectedResponse::Range {
            start: 0,
            end: Some(0),
            total: Some(100),
        };
        assert!(response_allows_body(&headers, expected));
        assert!(!response_allows_remote_body(&headers, expected, &remote));

        headers.etag = Some("\"fixture-v1\"".into());
        assert!(response_allows_remote_body(&headers, expected, &remote));
        headers.etag = None;
        assert!(response_allows_remote_body(&headers, expected, &remote));
    }

    #[test]
    fn bytes_left_by_an_earlier_attempt_need_a_validator_equal_to_the_record() {
        let expected = ExpectedResponse::Range {
            start: 10,
            end: None,
            total: Some(100),
        };
        let partial = |etag: Option<&str>, last_modified: Option<&str>| {
            let mut headers = ResponseHeaders {
                status: 206,
                etag: etag.map(str::to_owned),
                last_modified: last_modified.map(str::to_owned),
                headers_complete: true,
                ..Default::default()
            };
            headers.content_range = parse_content_range("bytes 10-99/100");
            headers
        };
        let date = "Wed, 01 Jan 2026 00:00:00 GMT";
        let with = |retained, etag: Option<&str>, last_modified: Option<&str>| RemoteInfo {
            retained,
            last_modified: last_modified.map(str::to_owned),
            ..remote_with_etag(etag)
        };

        // Without earlier bytes a missing validator disproves nothing.
        let fresh = with(Retained::None, Some("\"v1\""), None);
        assert!(!identity_changed(&partial(None, None), expected, &fresh));

        let unvouched = with(Retained::Unvouched, Some("\"v1\""), None);
        assert!(identity_changed(&partial(None, None), expected, &unvouched));
        assert!(identity_changed(
            &partial(Some("W/\"v1\""), None),
            expected,
            &unvouched
        ));
        assert!(identity_changed(
            &partial(Some("\"v2\""), None),
            expected,
            &unvouched
        ));
        assert!(!identity_changed(
            &partial(Some("\"v1\""), None),
            expected,
            &unvouched
        ));

        // A date vouches only for the recorded length, and a weak ETag is no identity.
        let dated = with(Retained::Unvouched, Some("W/\"a\""), Some(date));
        assert!(!identity_changed(
            &partial(None, Some(date)),
            expected,
            &dated
        ));
        assert!(identity_changed(
            &partial(None, Some("Thu, 02 Jan 2026 00:00:00 GMT")),
            expected,
            &dated
        ));
        let mut shorter = partial(None, Some(date));
        shorter.content_range = parse_content_range("bytes 10-99/101");
        assert!(identity_changed(&shorter, expected, &dated));

        // The probe of this attempt may vouch instead.
        let vouched = with(Retained::Vouched, Some("\"v1\""), None);
        assert!(!identity_changed(&partial(None, None), expected, &vouched));
        assert!(identity_changed(
            &partial(Some("\"v2\""), None),
            expected,
            &vouched
        ));
    }

    #[test]
    fn a_date_never_proves_a_file_whose_etag_disagrees() {
        let date = "Wed, 01 Jan 2026 00:00:00 GMT";
        let record = |etag: &str| RemoteInfo {
            last_modified: Some(date.into()),
            ..remote_with_etag(Some(etag))
        };
        let proves = |recorded: &str, etag: Option<&str>| {
            vouches_for_record(&record(recorded), etag, Some(date), Some(100))
        };
        assert!(proves("\"v1\"", None));
        assert!(proves("\"v1\"", Some("\"v1\"")));
        assert!(!proves("\"v1\"", Some("\"v2\"")));
        assert!(!proves("W/\"v1\"", Some("W/\"v2\"")));
        assert!(proves("W/\"v1\"", None));

        // Once the probe reports another ETag, only the recorded strong ETag on the ranged
        // answer proves the file.
        let mut disputed = record("\"v1\"");
        disputed.retained = Retained::Disputed;
        let partial = |etag: Option<&str>| {
            let mut headers = ResponseHeaders {
                status: 206,
                etag: etag.map(str::to_owned),
                last_modified: Some(date.into()),
                headers_complete: true,
                ..Default::default()
            };
            headers.content_range = parse_content_range("bytes 10-99/100");
            headers
        };
        let expected = ExpectedResponse::Range {
            start: 10,
            end: None,
            total: Some(100),
        };
        assert!(!identity_changed(
            &partial(Some("\"v1\"")),
            expected,
            &disputed
        ));
        assert!(identity_changed(&partial(None), expected, &disputed));
        disputed.etag = Some("W/\"v1\"".into());
        assert!(identity_changed(
            &partial(Some("W/\"v1\"")),
            expected,
            &disputed
        ));
    }

    #[test]
    fn a_probe_that_reports_another_etag_disputes_the_record() {
        let date = "Wed, 01 Jan 2026 00:00:00 GMT";
        let digest = "ab".repeat(32);
        let state = |etag: Option<&str>, checksum: Option<&str>| ResumeState {
            version: STATE_VERSION,
            etag: etag.map(str::to_owned),
            last_modified: Some(date.into()),
            total: Some(100),
            checksum: checksum.map(str::to_owned),
            ..Default::default()
        };
        let retention_of =
            |state: &ResumeState, probe_etag: Option<&str>, supplied: Option<&str>| {
                let record = RemoteInfo {
                    last_modified: state.last_modified.clone(),
                    ..remote_with_etag(state.etag.as_deref())
                };
                let probe = RemoteInfo {
                    last_modified: Some(date.into()),
                    ..remote_with_etag(probe_etag)
                };
                retention(state, &record, &probe, supplied)
            };
        let strong = state(Some("\"v1\""), None);
        assert_eq!(
            retention_of(&strong, Some("\"v1\""), None),
            Retained::Vouched
        );
        assert_eq!(retention_of(&strong, None, None), Retained::Vouched);
        assert_eq!(
            retention_of(&strong, Some("\"v2\""), None),
            Retained::Disputed
        );
        let weak = state(Some("W/\"v1\""), None);
        assert_eq!(
            retention_of(&weak, Some("W/\"v2\""), None),
            Retained::Disputed
        );

        // A digest guards the result, but not against a conflicting strong ETag.
        let guarded = state(Some("\"v1\""), Some(&digest));
        assert_eq!(
            retention_of(&guarded, Some("\"v2\""), Some(&digest)),
            Retained::Disputed
        );
        assert_eq!(
            retention_of(&guarded, Some("W/\"v2\""), Some(&digest)),
            Retained::Vouched
        );
        assert_eq!(
            retention_of(&guarded, Some("\"v2\""), Some(&"cd".repeat(32))),
            Retained::Disputed
        );
        let undated = ResumeState {
            etag: None,
            last_modified: None,
            ..state(None, Some(&digest))
        };
        assert_eq!(
            retention_of(&undated, Some("\"v2\""), Some(&digest)),
            Retained::Vouched
        );
    }

    #[test]
    fn only_a_response_with_http1_framing_that_names_no_end_is_close_delimited() {
        let parse = |lines: &[&str]| {
            let mut headers = ResponseHeaders::default();
            for line in lines {
                parse_header_line(&mut headers, format!("{line}\r\n").as_bytes());
            }
            headers
        };
        assert!(close_delimited(&parse(&["HTTP/1.1 200 OK"])));
        assert!(!close_delimited(&parse(&[
            "HTTP/1.1 200 OK",
            "Content-Length: 5"
        ])));
        assert!(!close_delimited(&parse(&[
            "HTTP/1.1 200 OK",
            "Transfer-Encoding: chunked"
        ])));
        assert!(!close_delimited(&parse(&["HTTP/2 200"])));
        assert!(!close_delimited(&parse(&["HTTP/1.1 206 Partial Content"])));
    }

    #[test]
    fn strong_etag_conflict_cannot_be_overridden_by_checksum() {
        let checksum = "ab".repeat(32);
        let state = ResumeState {
            version: STATE_VERSION,
            url: String::new(),
            etag: Some("\"fixture-v1\"".into()),
            last_modified: None,
            total: Some(100),
            segmented: false,
            connections: 1,
            checksum: Some(checksum.clone()),
            completed: Vec::new(),
            synced: None,
        };
        assert!(!state_matches(
            &state,
            &remote_with_etag(Some("\"fixture-v2\"")),
            false,
            1,
            Some(&checksum),
        ));
        assert!(state_matches(
            &state,
            &remote_with_etag(Some("\"fixture-v1\"")),
            false,
            1,
            Some(&checksum),
        ));
        assert!(state_matches(
            &state,
            &remote_with_etag(None),
            false,
            1,
            Some(&checksum),
        ));
        // Last-Modified plus length is an identity; a changed date or a disagreeing weak
        // ETag is not.
        let dated = ResumeState {
            etag: None,
            last_modified: Some("Wed, 01 Jan 2026 00:00:00 GMT".into()),
            checksum: None,
            ..state
        };
        let mut remote = remote_with_etag(None);
        remote.last_modified = dated.last_modified.clone();
        assert!(state_matches(&dated, &remote, false, 1, None));
        remote.last_modified = Some("Thu, 02 Jan 2026 00:00:00 GMT".into());
        assert!(!state_matches(&dated, &remote, false, 1, None));
        remote.last_modified = dated.last_modified.clone();
        let weak = ResumeState {
            etag: Some("W/\"a\"".into()),
            ..dated
        };
        remote.etag = Some("W/\"b\"".into());
        assert!(!state_matches(&weak, &remote, false, 1, None));
    }

    #[test]
    fn selected_video_and_subtitle_tracks_require_media_pipeline() {
        let mut request = AddRequest {
            video_format_id: Some("video-1".into()),
            ..Default::default()
        };
        assert!(request_requires_media_pipeline(&request));
        request.video_format_id = None;
        request.subtitle_tracks.push(Default::default());
        assert!(request_requires_media_pipeline(&request));
    }

    #[test]
    fn split_ranges_cover_each_byte_once() {
        assert_eq!(split_ranges(10, 3), vec![(0, 3), (4, 6), (7, 9)]);
        assert_eq!(split_ranges(2, 4), vec![(0, 0), (1, 1)]);
    }

    #[test]
    fn only_supported_network_schemes_are_accepted() {
        assert!(validate_source_url("https://example.test/file").is_ok());
        assert!(validate_source_url("ftp://example.test/file").is_ok());
        assert!(validate_source_url("file:///C:/secret.txt").is_err());
        assert!(validate_source_url("data:text/plain,secret").is_err());
    }

    #[test]
    fn content_disposition_prefers_utf8_and_handles_quoted_semicolons() {
        assert_eq!(
            disposition_name(
                "attachment; filename=plain.txt; filename*=UTF-8''rapor%20%C3%B6zel.txt"
            ),
            Some("rapor özel.txt".into())
        );
        assert_eq!(
            disposition_name("attachment; filename=\"one; two.txt\""),
            Some("one; two.txt".into())
        );
        assert_eq!(
            disposition_name("attachment; filename*=ISO-8859-1''bad"),
            None
        );
    }

    fn headers_of(lines: &[&str]) -> ResponseHeaders {
        let mut headers = ResponseHeaders::default();
        for line in lines {
            parse_header_line(&mut headers, format!("{line}\r\n").as_bytes());
        }
        headers
    }

    #[test]
    fn only_a_disposition_of_type_attachment_marks_an_attachment() {
        let attachment = |value: &str| {
            let line = format!("Content-Disposition: {value}");
            headers_of(&[line.as_str()]).attachment
        };
        assert!(attachment("attachment"));
        assert!(attachment("Attachment; filename=\"a.bin\""));
        assert!(attachment("  ATTACHMENT ;filename=a.bin"));
        assert!(!attachment("inline; filename=\"attachment\""));
        assert!(!attachment("inline"));
        assert!(!attachment("form-data; name=\"attachment\""));
        assert!(!headers_of(&["Content-Type: text/html"]).attachment);
        // A new status line starts a new answer: a redirect's disposition is not the page's.
        let mut headers = headers_of(&["HTTP/1.1 302 Found", "Content-Disposition: attachment"]);
        parse_header_line(&mut headers, b"HTTP/1.1 200 OK\r\n");
        assert!(!headers.attachment);
    }

    #[test]
    fn a_web_page_is_html_that_is_not_an_attachment() {
        assert!(is_web_page(Some("text/html"), false));
        assert!(is_web_page(Some("application/xhtml+xml"), false));
        assert!(!is_web_page(Some("text/html"), true));
        assert!(!is_web_page(Some("text/plain"), false));
        assert!(!is_web_page(Some("application/octet-stream"), false));
        assert!(!is_web_page(Some("application/vnd.apple.mpegurl"), false));
        assert!(!is_web_page(None, false));
        // The type is lowercased and cut at its parameters when the header is read.
        let page = headers_of(&["Content-Type: Text/HTML; charset=UTF-8"]);
        assert!(is_web_page(page.content_type.as_deref(), page.attachment));
    }

    #[test]
    fn a_requested_name_is_a_page_name_by_its_extension_only() {
        for name in [
            "a.htm",
            "a.html",
            "a.xhtml",
            "A.HTML",
            "page (1).Htm",
            "x.y.html",
        ] {
            assert!(is_html_name(Path::new(name)), "{name}");
        }
        for name in [
            "html",
            "a.html.bin",
            "a.htmlx",
            "a.php",
            "a.pdf",
            "uc",
            "a.html ",
        ] {
            assert!(!is_html_name(Path::new(name)), "{name}");
        }
    }

    #[test]
    fn a_page_is_refused_only_for_a_redirect_and_only_when_the_server_sends_it_as_a_page() {
        let answer = |status: u32, content_type: Option<&str>, attachment: bool| ResponseHeaders {
            status,
            content_type: content_type.map(str::to_owned),
            attachment,
            ..Default::default()
        };
        let redirected = RemoteInfo {
            refuse_page: true,
            ..remote_with_etag(None)
        };
        let direct = remote_with_etag(None);
        for status in [200, 206] {
            assert!(page_refused(
                &answer(status, Some("text/html"), false),
                &redirected
            ));
            assert!(!page_refused(
                &answer(status, Some("text/html"), false),
                &direct
            ));
            assert!(!page_refused(
                &answer(status, Some("text/html"), true),
                &redirected
            ));
            assert!(!page_refused(
                &answer(status, Some("application/pdf"), false),
                &redirected
            ));
        }
        // An error status or a redirect has its own handling; a page there is not the file.
        for status in [0, 302, 403, 404, 503] {
            assert!(!page_refused(
                &answer(status, Some("text/html"), false),
                &redirected
            ));
        }
    }

    #[test]
    fn the_refusal_of_a_page_is_the_authorization_code_and_no_transient_failure() {
        let error = page_instead_of_file();
        assert_eq!(
            crate::error_codes::code_of_text(&error),
            Some(crate::error_codes::NET_004)
        );
        assert!(format!("{error:#}").starts_with("SSD-NET-004 "));
        assert!(
            format!("{error:#}").contains("bağlantıyı tarayıcıdan yeniden ekleyin"),
            "{error:#}"
        );
        let english = crate::i18n::with_language("en", || format!("{:#}", page_instead_of_file()));
        assert!(
            english
                .ends_with("add the link again from the browser (Download link with SSDownload)."),
            "{english}"
        );
        assert!(retry_after(&error).is_none() && http_status(&error).is_none());
        assert!(!preserves_partial(&error));
    }

    #[test]
    fn consented_session_cookies_reach_only_the_matching_same_origin_request() {
        let cookie =
            |name: &str, domain: &str, path: &str, secure: bool| crate::model::ScopedCookie {
                name: name.into(),
                value: format!("{name}-value"),
                domain: domain.into(),
                path: path.into(),
                secure,
                host_only: true,
                ..Default::default()
            };
        let request = AddRequest {
            session_cookies: vec![
                cookie("site", "example.test", "/", false),
                cookie("deep", "example.test", "/files", false),
                cookie("elsewhere", "other.test", "/", false),
                cookie("tls-only", "example.test", "/", true),
            ],
            ..Default::default()
        };
        let headers = |url: &str, origin_url: &str| {
            let current = Url::parse(url).unwrap();
            build_headers(
                &request,
                &current,
                &origin(&Url::parse(origin_url).unwrap()),
                None,
            )
            .unwrap()
            .iter()
            .map(|line| String::from_utf8_lossy(line).into_owned())
            .collect::<Vec<_>>()
        };
        assert_eq!(
            headers(
                "http://example.test/files/a.bin",
                "http://example.test/files/a.bin"
            ),
            vec!["Cookie: deep=deep-value; site=site-value".to_owned()]
        );
        assert_eq!(
            headers("https://example.test/a.bin", "https://example.test/a.bin"),
            vec!["Cookie: site=site-value; tls-only=tls-only-value".to_owned()]
        );
        // A redirect to another origin never carries the session.
        assert!(headers("http://cdn.example.test/a.bin", "http://example.test/a.bin").is_empty());
    }

    #[test]
    fn url_file_names_are_percent_decoded() {
        let name = |value: &str| url_file_name(&Url::parse(value).unwrap());
        assert_eq!(
            name("https://example.test/a/My%20Report%20%281%29.pdf?token=a%20b"),
            Some("My Report (1).pdf".into())
        );
        assert_eq!(
            name("https://example.test/%C3%BCr%C3%BCn.zip"),
            Some("ürün.zip".into())
        );
        // Parsing already percent-encodes characters typed straight into the address.
        assert_eq!(
            name("https://example.test/ürün listesi.zip"),
            Some("ürün listesi.zip".into())
        );
        // A malformed or non-UTF-8 escape keeps the segment as received.
        assert_eq!(
            name("https://example.test/100%.txt"),
            Some("100%.txt".into())
        );
        assert_eq!(
            name("https://example.test/a%FF.bin"),
            Some("a%FF.bin".into())
        );
        assert_eq!(name("https://example.test/folder/"), None);
    }

    #[test]
    fn a_redirect_never_leads_from_https_to_plain_http() {
        let secure = Url::parse("https://cdn.example/a/0.ts").unwrap();
        let plain = Url::parse("http://cdn.example/a/0.ts").unwrap();
        let followed = |current: &Url, location: &str| {
            redirected_url(current, Some(location)).map(|next| next.to_string())
        };
        assert!(followed(&secure, "http://mirror.example/0.ts").is_err());
        assert!(followed(&secure, "ftp://mirror.example/0.ts").is_err());
        assert!(redirected_url(&secure, None).is_err());
        assert_eq!(
            followed(&secure, "//mirror.example/0.ts").unwrap(),
            "https://mirror.example/0.ts"
        );
        assert_eq!(
            followed(&secure, "/b/0.ts").unwrap(),
            "https://cdn.example/b/0.ts"
        );
        assert_eq!(
            followed(&plain, "https://mirror.example/0.ts").unwrap(),
            "https://mirror.example/0.ts"
        );
        assert_eq!(
            followed(&plain, "http://mirror.example/0.ts").unwrap(),
            "http://mirror.example/0.ts"
        );
    }

    #[test]
    fn redirect_never_allows_site_credentials_to_cross_origins() {
        assert!(header_is_safe_across_origin("authorization", true));
        assert!(!header_is_safe_across_origin("authorization", false));
        assert!(!header_is_safe_across_origin("cookie", false));
        assert!(!header_is_safe_across_origin("x-site-token", false));
        assert!(header_is_safe_across_origin("accept", false));
        assert!(header_is_safe_across_origin("accept-language", false));
    }

    #[test]
    fn a_checksum_mismatch_discards_the_part_and_state_but_a_malformed_digest_keeps_them() {
        let directory = TestDirectory::new("checksum");
        let output = directory.0.join("payload.bin");
        let (part, state) = (part_path(&output), state_path(&output));
        let control = TransferControl::default();
        let code = |error: &anyhow::Error| crate::error_codes::code_of(error);

        fs::write(&part, b"payload").unwrap();
        fs::write(&state, b"{}").unwrap();
        let error =
            finalize_part(&output, &part, &state, 7, &control, Some(&"0".repeat(64))).unwrap_err();
        assert_eq!(code(&error), Some(crate::error_codes::TRF_003), "{error:#}");
        assert!(!part.exists() && !state.exists());
        assert!(!output.exists());
        assert_eq!(retained_bytes(&output), Some(0));

        // A checksum that is not a digest says nothing about the downloaded bytes.
        fs::write(&part, b"payload").unwrap();
        fs::write(&state, b"{}").unwrap();
        let error =
            finalize_part(&output, &part, &state, 7, &control, Some("not-a-digest")).unwrap_err();
        assert_ne!(code(&error), Some(crate::error_codes::TRF_003), "{error:#}");
        assert!(part.exists() && state.exists());
        assert!(!output.exists());
    }

    #[test]
    fn a_changed_worker_count_does_not_turn_earlier_bytes_into_credit() {
        let control = TransferControl::default();
        control.set_speed_limit(100_000);
        // 45 000 bytes written while two workers shared the limit use up 0.9 s of a
        // 50 000 B/s share.
        let mut started = Instant::now() - Duration::from_millis(900);
        let mut cost = 0u64;
        throttle(&control, 2, &mut started, &mut cost, 45_000);

        // Alone, the same worker may write at the whole limit from now on, but the bytes
        // written earlier stay charged at the old share: another 45 000 bytes still wait
        // about 0.45 s instead of arriving at once.
        let before = Instant::now();
        throttle(&control, 1, &mut started, &mut cost, 45_000);
        assert!(
            before.elapsed() >= Duration::from_millis(400),
            "{:?}",
            before.elapsed()
        );
    }

    #[test]
    fn source_refresh_preparation_is_cancellable_and_does_not_stage_early() {
        let directory = TestDirectory::new("refresh-stage");
        let output = directory.0.join("payload.bin");
        fs::write(part_path(&output), b"x").unwrap();
        let previous = AddRequest {
            url: "https://old.example.test/payload.bin".into(),
            ..Default::default()
        };
        let old_state = ResumeState {
            version: STATE_VERSION,
            url: source_url_digest(&validate_source_url(&previous.url).unwrap()),
            etag: Some("\"stable-v1\"".into()),
            total: Some(4),
            segmented: false,
            connections: 1,
            checksum: None,
            completed: vec![ByteRange { start: 0, end: 1 }],
            last_modified: None,
            synced: None,
        };
        write_state(&state_path(&output), &old_state).unwrap();
        let before = fs::read(state_path(&output)).unwrap();

        let network = NetworkGovernor::new(16);
        let cancelled = TransferControl::default();
        cancelled.cancel();
        assert!(
            prepare_source_refresh(&previous, &previous, &output, false, &cancelled, &network)
                .is_err()
        );
        assert_eq!(fs::read(state_path(&output)).unwrap(), before);

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            assert!(request.starts_with("HEAD "));
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nETag: \"stable-v1\"\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
            stream.flush().unwrap();
        });
        let next = AddRequest {
            url: format!("http://{address}/payload.bin"),
            ..Default::default()
        };
        let prepared = prepare_source_refresh(
            &previous,
            &next,
            &output,
            false,
            &TransferControl::default(),
            &network,
        )
        .unwrap();
        server.join().unwrap();
        assert_eq!(fs::read(state_path(&output)).unwrap(), before);
        assert!(stage_source_refresh(&prepared).unwrap());
        let staged = read_state(&state_path(&output)).unwrap();
        assert_eq!(
            staged.url,
            source_url_digest(&validate_source_url(&next.url).unwrap())
        );
    }

    #[test]
    fn head_rejection_falls_back_to_ranged_get_and_uses_server_filename() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let payload = b"fallback-data".to_vec();
        let served = payload.clone();
        let (server_done, server_result) = mpsc::channel();
        thread::spawn(move || {
            let mut seen = Vec::new();
            for _ in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_request(&mut stream);
                seen.push(request.clone());
                if request.starts_with("HEAD ") {
                    response(&mut stream, "405 Method Not Allowed", &[], b"");
                } else if request.contains("Range: bytes=0-0") {
                    response(
                        &mut stream,
                        "206 Partial Content",
                        &[
                            "Content-Range: bytes 0-0/13",
                            "Accept-Ranges: bytes",
                            "ETag: \"fixture-v1\"",
                        ],
                        &served[..1],
                    );
                } else {
                    response(
                        &mut stream,
                        "200 OK",
                        &[
                            "Accept-Ranges: bytes",
                            "ETag: \"fixture-v1\"",
                            "Content-Disposition: attachment; filename*=UTF-8''rapor%20%C3%B6zel.txt",
                        ],
                        &served,
                    );
                }
            }
            server_done.send(seen).unwrap();
        });

        let url = format!("http://{address}/file");
        let request = local_request(url);
        let control = TransferControl::default();
        let network = NetworkGovernor::new(16);
        let (media, prepared) = prepare_auto(&request, &control, &network).unwrap();
        assert!(!media);
        let directory = TestDirectory::new("head-fallback");
        let output = directory.0.join("chosen-name.bin");
        let result = download(
            &request,
            &output,
            &control,
            &network,
            1,
            prepared,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(fs::read(&result.path).unwrap(), payload);
        assert_eq!(
            result.path.file_name().unwrap().to_string_lossy(),
            "rapor özel.txt"
        );
        let seen = server_result.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(seen[0].starts_with("HEAD "));
        assert!(seen[1].starts_with("GET "));
        assert!(seen[1].contains("Range: bytes=0-0"));
    }

    #[test]
    fn interrupted_http_transfer_resumes_with_an_exact_range() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let payload: Vec<u8> = (0..128).collect();
        let served = payload.clone();
        let (server_done, server_result) = mpsc::channel();
        thread::spawn(move || {
            let mut requests = Vec::new();
            for index in 0..4 {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_request(&mut stream);
                requests.push(request.clone());
                match index {
                    0 | 2 => {
                        stream
                            .write_all(
                                b"HTTP/1.1 200 OK\r\nContent-Length: 128\r\nAccept-Ranges: bytes\r\nETag: \"resume-v1\"\r\nConnection: close\r\n\r\n",
                            )
                            .unwrap();
                        stream.flush().unwrap();
                    }
                    1 => {
                        stream
                            .write_all(
                                b"HTTP/1.1 200 OK\r\nContent-Length: 128\r\nAccept-Ranges: bytes\r\nETag: \"resume-v1\"\r\nConnection: close\r\n\r\n"
                            )
                            .unwrap();
                        stream.write_all(&served[..31]).unwrap();
                        stream.flush().unwrap();
                    }
                    _ => {
                        response(
                            &mut stream,
                            "206 Partial Content",
                            &["Content-Range: bytes 31-127/128", "ETag: \"resume-v1\""],
                            &served[31..],
                        );
                    }
                }
            }
            server_done.send(requests).unwrap();
        });

        let request = local_request(format!("http://{address}/resume"));
        let control = TransferControl::default();
        let network = NetworkGovernor::new(16);
        let directory = TestDirectory::new("resume");
        let output = directory.0.join("resume.bin");
        assert!(download(&request, &output, &control, &network, 1, None, &mut |_| {}).is_err());
        assert_eq!(fs::metadata(part_path(&output)).unwrap().len(), 31);
        let result = download(&request, &output, &control, &network, 1, None, &mut |_| {});
        let requests = server_result.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(result.is_ok(), "resume request sequence was {requests:#?}");
        let result = result.unwrap();
        assert_eq!(fs::read(result.path).unwrap(), payload);
        assert!(requests[3].contains("Range: bytes=31-"));
    }

    #[test]
    fn a_partial_without_any_validator_restarts_from_zero_instead_of_failing() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let payload: Vec<u8> = (0..128).collect();
        let served = payload.clone();
        let (server_done, server_result) = mpsc::channel();
        thread::spawn(move || {
            let mut requests = Vec::new();
            for index in 0..4 {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_request(&mut stream);
                requests.push(request);
                let head =
                    b"HTTP/1.1 200 OK\r\nContent-Length: 128\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n";
                stream.write_all(head).unwrap();
                if index == 1 {
                    stream.write_all(&served[..31]).unwrap();
                } else if index == 3 {
                    stream.write_all(&served).unwrap();
                }
                stream.flush().unwrap();
            }
            server_done.send(requests).unwrap();
        });
        let request = local_request(format!("http://{address}/no-validators"));
        let control = TransferControl::default();
        let network = NetworkGovernor::new(16);
        let directory = TestDirectory::new("restart-unproven");
        let output = directory.0.join("payload.bin");
        assert!(download(&request, &output, &control, &network, 1, None, &mut |_| {}).is_err());
        assert_eq!(fs::metadata(part_path(&output)).unwrap().len(), 31);
        let result = download(&request, &output, &control, &network, 1, None, &mut |_| {}).unwrap();
        assert_eq!(fs::read(result.path).unwrap(), payload);
        let requests = server_result.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(!requests[3].to_ascii_lowercase().contains("range:"));
        assert!(!part_path(&output).exists());
    }

    #[test]
    fn cross_origin_redirect_strips_credentials_during_probe_and_download() {
        let target_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let target_address = target_listener.local_addr().unwrap();
        let (target_done, target_requests) = mpsc::channel();
        thread::spawn(move || {
            let mut requests = Vec::new();
            for _ in 0..2 {
                let (mut stream, _) = target_listener.accept().unwrap();
                let request = read_request(&mut stream);
                requests.push(request);
                response(
                    &mut stream,
                    "200 OK",
                    &["Content-Type: application/octet-stream"],
                    b"safe",
                );
            }
            target_done.send(requests).unwrap();
        });

        let source_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let source_address = source_listener.local_addr().unwrap();
        let (source_done, source_request) = mpsc::channel();
        thread::spawn(move || {
            let (mut stream, _) = source_listener.accept().unwrap();
            let request = read_request(&mut stream);
            response(
                &mut stream,
                "302 Found",
                &[&format!("Location: http://{target_address}/target")],
                b"",
            );
            source_done.send(request).unwrap();
        });

        let mut request = local_request(format!("http://{source_address}/source"));
        request
            .headers
            .insert("Authorization".into(), "Bearer secret".into());
        request
            .headers
            .insert("Cookie".into(), "sid=private".into());
        request
            .headers
            .insert("X-Site-Token".into(), "do-not-forward".into());
        request.headers.insert("Accept".into(), "*/*".into());
        let control = TransferControl::default();
        let network = NetworkGovernor::new(16);
        let (_media, prepared) = prepare_auto(&request, &control, &network).unwrap();
        let directory = TestDirectory::new("redirect-headers");
        let result = download(
            &request,
            &directory.0.join("redirect.bin"),
            &control,
            &network,
            1,
            prepared,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(fs::read(result.path).unwrap(), b"safe");

        let source = source_request.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(source.contains("Authorization: Bearer secret"));
        let target = target_requests
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        for request in target {
            let lower = request.to_ascii_lowercase();
            assert!(!lower.contains("authorization:"));
            assert!(!lower.contains("cookie:"));
            assert!(!lower.contains("x-site-token:"));
            assert!(lower.contains("accept: */*"));
        }
    }

    #[test]
    fn stable_large_resource_downloads_exactly_once_per_range_then_merges() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let payload: Vec<u8> = (0..(MIN_SEGMENT_SIZE * 2 + 257))
            .map(|index| (index % 251) as u8)
            .collect();
        let total = payload.len();
        let served = std::sync::Arc::new(payload.clone());
        let (server_done, server_requests) = mpsc::channel();
        thread::spawn(move || {
            let mut requests = Vec::new();
            for _ in 0..4 {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_request(&mut stream);
                if request.starts_with("HEAD ") {
                    stream
                        .write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nAccept-Ranges: bytes\r\nETag: \"segmented-v1\"\r\nConnection: close\r\n\r\n"
                            )
                            .as_bytes(),
                        )
                        .unwrap();
                    stream.flush().unwrap();
                } else {
                    let value = request
                        .lines()
                        .find(|line| line.to_ascii_lowercase().starts_with("range:"))
                        .expect("ranged request")
                        .split_once(':')
                        .unwrap()
                        .1
                        .trim()
                        .strip_prefix("bytes=")
                        .unwrap();
                    let (start, end) = value.split_once('-').unwrap();
                    let start: usize = start.parse().unwrap();
                    let end: usize = if end.is_empty() {
                        total - 1
                    } else {
                        end.parse().unwrap()
                    };
                    assert!(start <= end && end < total);
                    response(
                        &mut stream,
                        "206 Partial Content",
                        &[
                            &format!("Content-Range: bytes {start}-{end}/{total}"),
                            "Accept-Ranges: bytes",
                            "ETag: \"segmented-v1\"",
                        ],
                        &served[start..=end],
                    );
                }
                requests.push(request);
            }
            server_done.send(requests).unwrap();
        });

        let request = local_request(format!("http://{address}/large"));
        let control = TransferControl::default();
        let network = NetworkGovernor::new(16);
        let (_media, prepared) = prepare_auto(&request, &control, &network).unwrap();
        let directory = TestDirectory::new("segmented");
        let output = directory.0.join("large.bin");
        let result = download(
            &request,
            &output,
            &control,
            &network,
            2,
            prepared,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(fs::read(result.path).unwrap(), payload);
        assert!(!part_path(&output).exists());
        assert!(!segment_path(&output, 0).exists());
        assert!(!segment_path(&output, 1).exists());
        let requests = server_requests
            .recv_timeout(Duration::from_secs(10))
            .unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.starts_with("HEAD "))
                .count(),
            1
        );
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.contains("Range:"))
                .count(),
            3
        );
    }

    #[test]
    fn a_failed_range_does_not_hang_the_scheduler_while_another_worker_keeps_reporting() {
        fn serve(stream: &mut TcpStream, payload: &[u8]) {
            let request = read_request(stream);
            let total = payload.len();
            if request.starts_with("HEAD ") {
                stream
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nAccept-Ranges: bytes\r\nETag: \"range-failure-v1\"\r\nConnection: close\r\n\r\n"
                        )
                        .as_bytes(),
                    )
                    .unwrap();
                return;
            }
            let Some(range) = request
                .lines()
                .find(|line| line.to_ascii_lowercase().starts_with("range:"))
                .map(|line| line.split_once(':').unwrap().1.trim().to_owned())
            else {
                response(
                    stream,
                    "200 OK",
                    &["Accept-Ranges: bytes", "ETag: \"range-failure-v1\""],
                    payload,
                );
                return;
            };
            let (start, end) = range
                .strip_prefix("bytes=")
                .and_then(|value| value.split_once('-'))
                .unwrap();
            let start: usize = start.parse().unwrap();
            let end: usize = end.parse().unwrap();
            if start == 0 && end > 0 {
                response(stream, "416 Range Not Satisfiable", &[], b"");
                return;
            }
            let body = &payload[start..=end];
            let head = format!(
                "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {start}-{end}/{total}\r\nAccept-Ranges: bytes\r\nETag: \"range-failure-v1\"\r\nConnection: close\r\n\r\n",
                body.len()
            );
            if stream.write_all(head.as_bytes()).is_err() {
                return;
            }
            // Every range after the first is served slowly, so its worker keeps reporting
            // progress long after the first range has already failed.
            for chunk in body.chunks(32 * 1024) {
                if stream.write_all(chunk).is_err() {
                    return;
                }
                if start > 0 {
                    thread::sleep(Duration::from_millis(25));
                }
            }
        }

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let payload: Arc<Vec<u8>> = Arc::new(
            (0..MIN_SEGMENT_SIZE * 3)
                .map(|index| (index % 251) as u8)
                .collect(),
        );
        let stop = Arc::new(AtomicBool::new(false));
        let server = {
            let payload = payload.clone();
            let stop = stop.clone();
            thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            stream.set_nonblocking(false).unwrap();
                            let payload = payload.clone();
                            thread::spawn(move || serve(&mut stream, &payload));
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            })
        };

        let directory = TestDirectory::new("range-failure");
        let output = directory.0.join("large.bin");
        let (finished, outcome) = mpsc::channel();
        {
            let output = output.clone();
            thread::spawn(move || {
                let result = download(
                    &local_request(format!("http://{address}/large")),
                    &output,
                    &TransferControl::default(),
                    &NetworkGovernor::new(16),
                    2,
                    None,
                    &mut |_| {},
                )
                .map(|result| fs::read(result.path).unwrap())
                .map_err(|error| format!("{error:#}"));
                let _ = finished.send(result);
            });
        }
        let outcome = outcome
            .recv_timeout(Duration::from_secs(30))
            .expect("the segmented scheduler hung after one range failed");
        stop.store(true, Ordering::Release);
        server.join().unwrap();
        assert_eq!(outcome.unwrap(), *payload);
    }

    /// How the server answers the lease that waits for the other lease to write.
    #[derive(Clone, Copy, Debug)]
    enum LateAnswer {
        Rejected,
        Ignored,
        Changed,
    }

    /// One of two leases streams; the other is answered only after the first has written some
    /// bytes, and then discards the ranges. Nothing records those bytes, because no cycle can
    /// start, so the download has to fall back to one connection and finish in one attempt.
    fn a_discarded_range_leaves_the_bytes_of_the_other_lease_unrecorded(late: LateAnswer) {
        fn serve(stream: &mut TcpStream, payload: &[u8], streamed: &AtomicBool, late: LateAnswer) {
            let request = read_request(stream);
            let total = payload.len();
            if request.starts_with("HEAD ") {
                let _ = stream.write_all(
                    format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nAccept-Ranges: bytes\r\nETag: \"discard-v1\"\r\nConnection: close\r\n\r\n").as_bytes(),
                );
                return;
            }
            let Some(range) = request
                .lines()
                .find(|line| line.to_ascii_lowercase().starts_with("range:"))
                .map(|line| line.split_once(':').unwrap().1.trim().to_owned())
            else {
                let _ = stream.write_all(
                    format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nAccept-Ranges: bytes\r\nETag: \"discard-v1\"\r\nConnection: close\r\n\r\n").as_bytes(),
                );
                let _ = stream.write_all(payload);
                return;
            };
            let (start, end) = range
                .strip_prefix("bytes=")
                .and_then(|value| value.split_once('-'))
                .unwrap();
            let start: usize = start.parse().unwrap();
            let end: usize = end.parse().unwrap();
            let (status, etag, body) = if start > 0 {
                let deadline = Instant::now() + Duration::from_secs(20);
                while !streamed.load(Ordering::Acquire) && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(5));
                }
                match late {
                    LateAnswer::Rejected => {
                        let _ = stream.write_all(
                            b"HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        );
                        return;
                    }
                    LateAnswer::Ignored => {
                        let _ = stream.write_all(
                            format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nAccept-Ranges: bytes\r\nETag: \"discard-v1\"\r\nConnection: close\r\n\r\n").as_bytes(),
                        );
                        let _ = stream.write_all(payload);
                        return;
                    }
                    LateAnswer::Changed => {
                        ("206 Partial Content", "discard-v2", &payload[start..=end])
                    }
                }
            } else {
                ("206 Partial Content", "discard-v1", &payload[start..=end])
            };
            let head = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Range: bytes {start}-{end}/{total}\r\nAccept-Ranges: bytes\r\nETag: \"{etag}\"\r\nConnection: close\r\n\r\n",
                body.len()
            );
            if stream.write_all(head.as_bytes()).is_err() {
                return;
            }
            for chunk in body.chunks(32 * 1024) {
                if stream.write_all(chunk).is_err() {
                    return;
                }
                // The first lease streams slowly; the probe is one byte.
                if end > 0 {
                    thread::sleep(Duration::from_millis(25));
                }
            }
        }

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let payload: Arc<Vec<u8>> = Arc::new(
            (0..MIN_SEGMENT_SIZE * 3)
                .map(|index| (index % 251) as u8)
                .collect(),
        );
        let streamed = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let server = {
            let (payload, streamed, stop) = (payload.clone(), streamed.clone(), stop.clone());
            thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            stream.set_nonblocking(false).unwrap();
                            let (payload, streamed) = (payload.clone(), streamed.clone());
                            thread::spawn(move || serve(&mut stream, &payload, &streamed, late));
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            })
        };

        let directory = TestDirectory::new("discarded-range");
        let output = directory.0.join("large.bin");
        let (finished, outcome) = mpsc::channel();
        {
            let (output, streamed) = (output.clone(), streamed.clone());
            thread::spawn(move || {
                // No cycle can start, so only the end of the attempt could record the bytes.
                TEST_CADENCE.with(|cadence| {
                    cadence.set(Some(Cadence {
                        interval: Duration::from_secs(3600),
                        bytes: u64::MAX,
                    }))
                });
                let control = TransferControl::default();
                let result = download(
                    &local_request(format!("http://{address}/large")),
                    &output,
                    &control,
                    &NetworkGovernor::new(16),
                    2,
                    None,
                    &mut |progress| {
                        if progress.downloaded >= 256 * 1024 {
                            streamed.store(true, Ordering::Release);
                        }
                    },
                )
                .map(|result| fs::read(result.path).unwrap())
                .map_err(|error| format!("{error:#}"));
                let _ = finished.send((result, control.restarted()));
            });
        }
        let (outcome, restarted) = outcome
            .recv_timeout(Duration::from_secs(60))
            .expect("the segmented scheduler hung after a range was discarded");
        stop.store(true, Ordering::Release);
        server.join().unwrap();
        assert_eq!(
            outcome.unwrap_or_else(|error| panic!("{late:?}: {error}")),
            *payload,
            "{late:?}"
        );
        // A recorded range would have kept the one-connection fallback from running and made the
        // download start over from byte zero instead.
        assert!(!restarted, "{late:?}: the download started over");
    }

    #[test]
    fn a_rejected_range_does_not_record_what_the_other_lease_wrote() {
        a_discarded_range_leaves_the_bytes_of_the_other_lease_unrecorded(LateAnswer::Rejected);
    }

    #[test]
    fn an_ignored_range_does_not_record_what_the_other_lease_wrote() {
        a_discarded_range_leaves_the_bytes_of_the_other_lease_unrecorded(LateAnswer::Ignored);
    }

    #[test]
    fn a_changed_source_does_not_record_what_the_other_lease_wrote() {
        a_discarded_range_leaves_the_bytes_of_the_other_lease_unrecorded(LateAnswer::Changed);
    }

    #[test]
    fn a_gateway_error_on_a_later_range_is_a_retryable_http_status_not_a_range_rejection() {
        // 522 stands for the proxy and CDN statuses (520-524) that no fixed list names.
        for (status, line) in [(502, "502 Bad Gateway"), (522, "522 Connection Timed Out")] {
            later_range_server_error_keeps_the_recorded_range(status, line);
        }
    }

    fn later_range_server_error_keeps_the_recorded_range(status: u32, line: &'static str) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let payload: Arc<Vec<u8>> = Arc::new(
            (0..MIN_SEGMENT_SIZE * 3)
                .map(|index| (index % 251) as u8)
                .collect(),
        );
        let stop = Arc::new(AtomicBool::new(false));
        let server = {
            let payload = payload.clone();
            let stop = stop.clone();
            thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            stream.set_nonblocking(false).unwrap();
                            let payload = payload.clone();
                            thread::spawn(move || {
                                let request = read_request(&mut stream);
                                let total = payload.len();
                                if request.starts_with("HEAD ") {
                                    let _ = stream.write_all(
                                        format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nAccept-Ranges: bytes\r\nETag: \"gateway-v1\"\r\nConnection: close\r\n\r\n").as_bytes(),
                                    );
                                    return;
                                }
                                let range = request
                                    .lines()
                                    .find(|line| line.to_ascii_lowercase().starts_with("range:"))
                                    .and_then(|line| {
                                        line.split_once("bytes=").map(|v| v.1.trim().to_owned())
                                    })
                                    .expect("ranged request");
                                let (start, end) = range.split_once('-').unwrap();
                                let (start, end): (usize, usize) =
                                    (start.parse().unwrap(), end.parse().unwrap());
                                if start > 0 {
                                    // The first range has finished and been recorded by now.
                                    thread::sleep(Duration::from_millis(400));
                                    let _ = stream.write_all(
                                        format!("HTTP/1.1 {line}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes(),
                                    );
                                    return;
                                }
                                let body = &payload[start..=end];
                                let _ = stream.write_all(
                                    format!("HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {start}-{end}/{total}\r\nETag: \"gateway-v1\"\r\nConnection: close\r\n\r\n", body.len()).as_bytes(),
                                );
                                let _ = stream.write_all(body);
                            });
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            })
        };

        let directory = TestDirectory::new(&format!("range-gateway-{status}"));
        let output = directory.0.join("large.bin");
        let error = download(
            &local_request(format!("http://{address}/large")),
            &output,
            &TransferControl::default(),
            &NetworkGovernor::new(16),
            2,
            None,
            &mut |_| {},
        )
        .unwrap_err();
        stop.store(true, Ordering::Release);
        server.join().unwrap();
        assert_eq!(http_status(&error), Some(status), "{error:#}");
        assert!(!preserves_partial(&error), "{error:#}");
        // The first range was durably recorded, so the retry resumes instead of restarting.
        let state = read_state(&state_path(&output)).unwrap();
        assert_eq!(
            state.completed,
            vec![ByteRange {
                start: 0,
                end: MIN_SEGMENT_SIZE * 3 / 2
            }]
        );
    }

    #[test]
    fn cancellation_stops_an_in_progress_http_download_and_preserves_the_part() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let payload = vec![0x5a; 512 * 1024];
        thread::spawn(move || {
            for index in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let _request = read_request(&mut stream);
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 524288\r\nAccept-Ranges: bytes\r\nETag: \"cancel-v1\"\r\nConnection: close\r\n\r\n",
                    )
                    .unwrap();
                stream.flush().unwrap();
                if index == 0 {
                    continue;
                }
                for chunk in payload.chunks(16 * 1024) {
                    if stream.write_all(chunk).is_err() {
                        break;
                    }
                    let _ = stream.flush();
                    thread::sleep(Duration::from_millis(20));
                }
            }
        });

        let request = local_request(format!("http://{address}/cancel"));
        let control = TransferControl::default();
        let network = NetworkGovernor::new(16);
        let cancel = control.clone();
        let directory = TestDirectory::new("cancel");
        let output = directory.0.join("cancel.bin");
        let error = download(
            &request,
            &output,
            &control,
            &network,
            1,
            None,
            &mut |progress| {
                if progress.downloaded > 0 {
                    cancel.cancel();
                }
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("durduruldu"));
        assert!(fs::metadata(part_path(&output)).unwrap().len() > 0);
        assert!(!output.exists());
    }

    fn patterned(length: u64) -> Vec<u8> {
        (0..length).map(|index| (index % 251) as u8).collect()
    }

    /// Serves one payload over HTTP with `HEAD`, ranged and plain `GET`, on a fresh
    /// connection per request, until it is dropped.
    struct PayloadServer {
        address: SocketAddr,
        requests: Arc<Mutex<Vec<String>>>,
        payload: Arc<Mutex<Arc<Vec<u8>>>>,
        stop: Arc<AtomicBool>,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl PayloadServer {
        fn start(payload: Vec<u8>, etag: &'static str) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let address = listener.local_addr().unwrap();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let payload = Arc::new(Mutex::new(Arc::new(payload)));
            let stop = Arc::new(AtomicBool::new(false));
            let thread = {
                let (requests, payload, stop) = (requests.clone(), payload.clone(), stop.clone());
                thread::spawn(move || {
                    while !stop.load(Ordering::Acquire) {
                        match listener.accept() {
                            Ok((mut stream, _)) => {
                                stream.set_nonblocking(false).unwrap();
                                let body = payload.lock().unwrap().clone();
                                let requests = requests.clone();
                                thread::spawn(move || {
                                    serve_payload(&mut stream, &body, etag, &requests)
                                });
                            }
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                thread::sleep(Duration::from_millis(5));
                            }
                            Err(_) => break,
                        }
                    }
                })
            };
            Self {
                address,
                requests,
                payload,
                stop,
                thread: Some(thread),
            }
        }

        fn url(&self) -> String {
            format!("http://{}/payload.bin", self.address)
        }

        fn serve(&self, payload: Vec<u8>) {
            *self.payload.lock().unwrap() = Arc::new(payload);
        }

        fn requests(&self) -> Vec<String> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl Drop for PayloadServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    fn serve_payload(
        stream: &mut TcpStream,
        payload: &[u8],
        etag: &str,
        requests: &Mutex<Vec<String>>,
    ) {
        let request = read_request(stream);
        requests.lock().unwrap().push(request.clone());
        let total = payload.len();
        let identity = format!("Accept-Ranges: bytes\r\nETag: {etag}\r\nConnection: close\r\n");
        if request.starts_with("HEAD ") {
            let _ = stream.write_all(
                format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\n{identity}\r\n").as_bytes(),
            );
            return;
        }
        let range = request
            .lines()
            .find(|line| line.to_ascii_lowercase().starts_with("range:"))
            .and_then(|line| line.split_once("bytes="))
            .and_then(|(_, value)| value.trim().split_once('-'));
        let (head, body) = match range {
            Some((start, end)) => {
                let start: usize = start.parse().unwrap();
                let end: usize = end.parse().unwrap_or(total - 1);
                (
                    format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {start}-{end}/{total}\r\n{identity}\r\n",
                        end - start + 1
                    ),
                    &payload[start..=end],
                )
            }
            None => (
                format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\n{identity}\r\n"),
                payload,
            ),
        };
        if stream.write_all(head.as_bytes()).is_ok() {
            let _ = stream.write_all(body);
        }
    }

    /// A segmented-download state as a previous run would have left it.
    fn segmented_state(
        url: &str,
        etag: &str,
        total: u64,
        completed: Vec<ByteRange>,
    ) -> ResumeState {
        ResumeState {
            version: STATE_VERSION,
            url: source_url_digest(&Url::parse(url).unwrap()),
            etag: Some(etag.into()),
            last_modified: None,
            total: Some(total),
            segmented: true,
            connections: 2,
            checksum: None,
            completed,
            synced: None,
        }
    }

    #[test]
    fn a_segmented_state_without_its_full_sparse_file_restarts_instead_of_failing_forever() {
        let payload = patterned(MIN_SEGMENT_SIZE * 2 + 257);
        let total = payload.len() as u64;
        let server = PayloadServer::start(payload.clone(), "\"sparse-v1\"");
        let request = local_request(server.url());
        for (label, existing_part) in [("missing", None), ("short", Some(10usize))] {
            let directory = TestDirectory::new(&format!("sparse-{label}"));
            let output = directory.0.join("large.bin");
            write_state(
                &state_path(&output),
                &segmented_state(
                    &request.url,
                    "\"sparse-v1\"",
                    total,
                    vec![ByteRange {
                        start: 0,
                        end: 1024,
                    }],
                ),
            )
            .unwrap();
            if let Some(length) = existing_part {
                fs::write(part_path(&output), vec![0u8; length]).unwrap();
            }
            let result = download(
                &request,
                &output,
                &TransferControl::default(),
                &NetworkGovernor::new(16),
                2,
                None,
                &mut |_| {},
            )
            .unwrap_or_else(|error| panic!("{label} sparse file: {error:#}"));
            assert_eq!(fs::read(result.path).unwrap(), payload, "{label}");
            assert!(!part_path(&output).exists() && !state_path(&output).exists());
        }
    }

    #[test]
    fn a_part_without_a_state_record_restarts_instead_of_failing_forever() {
        // A crash after the sparse file was created and before the first state write.
        let payload = patterned(MIN_SEGMENT_SIZE * 2 + 257);
        let server = PayloadServer::start(payload.clone(), "\"orphan-v1\"");
        let directory = TestDirectory::new("orphan-part");
        let output = directory.0.join("large.bin");
        let orphan = File::create(part_path(&output)).unwrap();
        orphan.set_len(payload.len() as u64).unwrap();
        drop(orphan);
        let result = download(
            &local_request(server.url()),
            &output,
            &TransferControl::default(),
            &NetworkGovernor::new(16),
            2,
            None,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(fs::read(result.path).unwrap(), payload);
        assert!(!part_path(&output).exists() && !state_path(&output).exists());
    }

    #[test]
    fn a_retry_after_a_checksum_mismatch_downloads_the_file_again() {
        let good: Vec<u8> = (0..128).collect();
        let mut corrupt = good.clone();
        corrupt[5] ^= 0xff;
        let server = PayloadServer::start(corrupt, "\"digest-v1\"");
        let mut request = local_request(server.url());
        request.checksum = Some(hex::encode(Sha256::digest(&good)));
        let control = TransferControl::default();
        let network = NetworkGovernor::new(16);
        let directory = TestDirectory::new("digest-retry");
        let output = directory.0.join("payload.bin");

        let error =
            download(&request, &output, &control, &network, 1, None, &mut |_| {}).unwrap_err();
        assert_eq!(
            crate::error_codes::code_of(&error),
            Some(crate::error_codes::TRF_003),
            "{error:#}"
        );
        assert!(!part_path(&output).exists() && !state_path(&output).exists());

        server.serve(good.clone());
        let result = download(&request, &output, &control, &network, 1, None, &mut |_| {}).unwrap();
        assert_eq!(fs::read(result.path).unwrap(), good);
        let body_requests = server
            .requests()
            .iter()
            .filter(|request| request.starts_with("GET "))
            .count();
        assert_eq!(body_requests, 2);
    }

    #[test]
    fn a_single_remaining_range_may_use_the_whole_speed_limit() {
        let payload = patterned(MIN_SEGMENT_SIZE * 4);
        let total = payload.len() as u64;
        let server = PayloadServer::start(payload.clone(), "\"limit-v1\"");
        let request = local_request(server.url());
        let directory = TestDirectory::new("limit-tail");
        let output = directory.0.join("large.bin");
        // Everything but the last 2 MiB is already durable, so one of four planned
        // connections downloads the tail.
        let done = total - MIN_SEGMENT_SIZE;
        let mut part = File::create(part_path(&output)).unwrap();
        part.write_all(&payload[..done as usize]).unwrap();
        part.set_len(total).unwrap();
        drop(part);
        let mut state = segmented_state(
            &request.url,
            "\"limit-v1\"",
            total,
            vec![ByteRange {
                start: 0,
                end: done,
            }],
        );
        state.connections = 4;
        write_state(&state_path(&output), &state).unwrap();

        let control = TransferControl::default();
        control.set_speed_limit(MIN_SEGMENT_SIZE);
        let started = Instant::now();
        let result = download(
            &request,
            &output,
            &control,
            &NetworkGovernor::new(16),
            4,
            None,
            &mut |_| {},
        )
        .unwrap();
        let elapsed = started.elapsed();
        assert_eq!(fs::read(result.path).unwrap(), payload);
        // 2 MiB at 2 MiB/s: never faster than the limit, and not paced at a quarter of it.
        assert!(elapsed >= Duration::from_millis(900), "{elapsed:?}");
        assert!(elapsed < Duration::from_millis(2500), "{elapsed:?}");
    }

    struct FtpBehavior {
        modification_time: bool,
        resume: bool,
        /// Ends the first transfer after this many bytes.
        cut_first_transfer: Option<usize>,
    }

    /// A scripted FTP server on loopback: anonymous login, passive transfers, and the
    /// optional `MDTM` and `REST` commands.
    struct FtpFixture {
        address: SocketAddr,
        commands: Arc<Mutex<Vec<String>>>,
        stop: Arc<AtomicBool>,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl FtpFixture {
        fn start(payload: Vec<u8>, behavior: FtpBehavior) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let address = listener.local_addr().unwrap();
            let commands = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let payload = Arc::new(payload);
            let behavior = Arc::new((
                behavior.modification_time,
                behavior.resume,
                Mutex::new(behavior.cut_first_transfer),
            ));
            let thread = {
                let (commands, stop) = (commands.clone(), stop.clone());
                thread::spawn(move || {
                    while !stop.load(Ordering::Acquire) {
                        match listener.accept() {
                            Ok((stream, _)) => {
                                stream.set_nonblocking(false).unwrap();
                                let (payload, behavior, commands) =
                                    (payload.clone(), behavior.clone(), commands.clone());
                                thread::spawn(move || {
                                    ftp_session(stream, &payload, &behavior, &commands)
                                });
                            }
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                thread::sleep(Duration::from_millis(5));
                            }
                            Err(_) => break,
                        }
                    }
                })
            };
            Self {
                address,
                commands,
                stop,
                thread: Some(thread),
            }
        }

        fn url(&self) -> String {
            format!("ftp://{}/payload.bin", self.address)
        }

        fn commands(&self) -> Vec<String> {
            self.commands.lock().unwrap().clone()
        }
    }

    impl Drop for FtpFixture {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    fn ftp_session(
        stream: TcpStream,
        payload: &[u8],
        behavior: &(bool, bool, Mutex<Option<usize>>),
        commands: &Mutex<Vec<String>>,
    ) {
        let (modification_time, resume, cut_first_transfer) = behavior;
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut writer = stream.try_clone().unwrap();
        let mut reader = BufReader::new(stream);
        let mut send = |line: &str| {
            let _ = writer.write_all(format!("{line}\r\n").as_bytes());
        };
        send("220 fixture ready");
        let mut data_listener = None;
        let mut rest = 0usize;
        let mut line = String::new();
        loop {
            line.clear();
            if !matches!(reader.read_line(&mut line), Ok(read) if read > 0) {
                return;
            }
            let command = line.trim_end().to_owned();
            commands.lock().unwrap().push(command.clone());
            let (verb, argument) = command.split_once(' ').unwrap_or((command.as_str(), ""));
            match verb.to_ascii_uppercase().as_str() {
                "USER" => send("230 logged in"),
                "PWD" => send("257 \"/\" is the current directory"),
                "CWD" => send("250 directory changed"),
                "TYPE" => send("200 type set"),
                "MDTM" if *modification_time => send("213 20260101000000"),
                "SIZE" => send(&format!("213 {}", payload.len())),
                "REST" if *resume => {
                    rest = argument.parse().unwrap();
                    send("350 restarting");
                }
                "EPSV" => {
                    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
                    let port = listener.local_addr().unwrap().port();
                    data_listener = Some(listener);
                    send(&format!("229 Entering Extended Passive Mode (|||{port}|)"));
                }
                "RETR" => {
                    let Some(listener) = data_listener.take() else {
                        send("425 no data connection");
                        continue;
                    };
                    send("150 opening data connection");
                    let (mut data, _) = listener.accept().unwrap();
                    let body = &payload[rest.min(payload.len())..];
                    match cut_first_transfer.lock().unwrap().take() {
                        Some(length) => {
                            let _ = data.write_all(&body[..length.min(body.len())]);
                            drop(data);
                            send("426 transfer aborted");
                        }
                        None => {
                            let _ = data.write_all(body);
                            drop(data);
                            send("226 transfer complete");
                        }
                    }
                }
                "QUIT" => {
                    send("221 bye");
                    return;
                }
                _ => send("502 command not implemented"),
            }
        }
    }

    fn ftp_download(server: &FtpFixture, output: &Path) -> Result<TransferResult> {
        download(
            &local_request(server.url()),
            output,
            &TransferControl::default(),
            &NetworkGovernor::new(16),
            1,
            None,
            &mut |_| {},
        )
    }

    #[test]
    fn an_ftp_partial_without_a_modification_time_restarts_from_zero() {
        let payload = patterned(128);
        let server = FtpFixture::start(
            payload.clone(),
            FtpBehavior {
                modification_time: false,
                resume: true,
                cut_first_transfer: Some(31),
            },
        );
        let directory = TestDirectory::new("ftp-no-mdtm");
        let output = directory.0.join("payload.bin");
        assert!(ftp_download(&server, &output).is_err());
        assert_eq!(fs::metadata(part_path(&output)).unwrap().len(), 31);

        let result = ftp_download(&server, &output).unwrap_or_else(|e| panic!("{e:#}"));
        assert_eq!(fs::read(result.path).unwrap(), payload);
        assert!(!part_path(&output).exists() && !state_path(&output).exists());
        // libcurl's metadata probe sends `REST 0`; a resumed transfer would send the offset.
        assert!(
            !server
                .commands()
                .iter()
                .any(|command| command.starts_with("REST ") && command != "REST 0"),
            "{:#?}",
            server.commands()
        );
    }

    #[test]
    fn an_ftp_server_that_refuses_rest_restarts_from_zero() {
        let payload = patterned(128);
        let server = FtpFixture::start(
            payload.clone(),
            FtpBehavior {
                modification_time: true,
                resume: false,
                cut_first_transfer: Some(31),
            },
        );
        let directory = TestDirectory::new("ftp-no-rest");
        let output = directory.0.join("payload.bin");
        assert!(ftp_download(&server, &output).is_err());
        assert_eq!(fs::metadata(part_path(&output)).unwrap().len(), 31);

        let result = ftp_download(&server, &output).unwrap_or_else(|e| panic!("{e:#}"));
        assert_eq!(fs::read(result.path).unwrap(), payload);
        assert!(!part_path(&output).exists() && !state_path(&output).exists());
        let commands = server.commands();
        assert!(
            commands.iter().any(|command| command == "REST 31"),
            "{commands:#?}"
        );
        // The refused `REST` ends the resumed attempt before `RETR`; only the cut first
        // transfer and the restarted one retrieve the file.
        assert_eq!(
            commands
                .iter()
                .filter(|command| command.starts_with("RETR"))
                .count(),
            2,
            "{commands:#?}"
        );
    }

    #[test]
    fn an_ftp_part_longer_than_its_recorded_length_is_cut_back_on_resume() {
        let payload = patterned(128);
        let server = FtpFixture::start(
            payload.clone(),
            FtpBehavior {
                modification_time: true,
                resume: true,
                cut_first_transfer: Some(31),
            },
        );
        let directory = TestDirectory::new("ftp-longer-part");
        let output = directory.0.join("payload.bin");
        assert!(ftp_download(&server, &output).is_err());
        assert_eq!(read_state(&state_path(&output)).unwrap().synced, Some(31));
        let mut part = OpenOptions::new()
            .append(true)
            .open(part_path(&output))
            .unwrap();
        part.write_all(&[0xAA; 200]).unwrap();
        drop(part);
        assert_eq!(retained_bytes(&output), Some(31));

        let result = ftp_download(&server, &output).unwrap_or_else(|e| panic!("{e:#}"));
        assert_eq!(fs::read(result.path).unwrap(), payload);
        assert!(
            server.commands().iter().any(|command| command == "REST 31"),
            "{:#?}",
            server.commands()
        );
    }

    #[test]
    fn an_ftp_part_that_already_holds_every_byte_is_finalized_without_a_transfer() {
        let payload = patterned(128);
        let server = FtpFixture::start(
            payload.clone(),
            FtpBehavior {
                modification_time: true,
                resume: true,
                cut_first_transfer: None,
            },
        );
        let directory = TestDirectory::new("ftp-complete");
        let output = directory.0.join("payload.bin");
        let request = local_request(server.url());
        let remote = probe_ftp(
            &request,
            Url::parse(&request.url).unwrap(),
            &TransferControl::default(),
            &NetworkGovernor::new(16),
        )
        .unwrap();
        assert!(remote.last_modified.is_some());
        fs::write(part_path(&output), &payload).unwrap();
        write_state(
            &state_path(&output),
            &ResumeState {
                version: STATE_VERSION,
                url: source_url_digest(&remote.url),
                etag: None,
                last_modified: remote.last_modified.clone(),
                total: remote.total,
                segmented: false,
                connections: 1,
                checksum: None,
                completed: Vec::new(),
                synced: None,
            },
        )
        .unwrap();

        let result = ftp_download(&server, &output).unwrap_or_else(|e| panic!("{e:#}"));
        assert_eq!(fs::read(result.path).unwrap(), payload);
        assert!(!part_path(&output).exists() && !state_path(&output).exists());
        assert!(
            !server
                .commands()
                .iter()
                .any(|command| command.starts_with("RETR")),
            "{:#?}",
            server.commands()
        );
    }

    /// Holds a file open without sharing, as a virus scanner does for a moment: it exists but
    /// cannot be read.
    fn hold_exclusively(path: &Path) -> File {
        use std::os::windows::fs::OpenOptionsExt;
        OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(path)
            .unwrap()
    }

    #[test]
    fn retained_bytes_are_unknown_while_the_resume_record_cannot_be_read() {
        let directory = TestDirectory::new("retained-unknown");
        let output = directory.0.join("payload.bin");
        assert_eq!(retained_bytes(&output), Some(0));
        fs::write(part_path(&output), b"payload").unwrap();
        write_state(
            &state_path(&output),
            &ResumeState {
                version: STATE_VERSION,
                ..ResumeState::default()
            },
        )
        .unwrap();
        assert_eq!(retained_bytes(&output), Some(7));

        let held = hold_exclusively(&state_path(&output));
        assert_eq!(retained_bytes(&output), None);
        drop(held);
        assert_eq!(retained_bytes(&output), Some(7));
    }

    #[test]
    fn a_segmented_partial_survives_a_state_record_that_cannot_be_read_for_a_moment() {
        let payload = patterned(MIN_SEGMENT_SIZE * 4);
        let total = payload.len() as u64;
        let server = PayloadServer::start(payload.clone(), "\"held-v1\"");
        let request = local_request(server.url());
        let directory = TestDirectory::new("held-state-segmented");
        let output = directory.0.join("large.bin");
        let done = total - MIN_SEGMENT_SIZE;
        let mut part = File::create(part_path(&output)).unwrap();
        part.write_all(&payload[..done as usize]).unwrap();
        part.set_len(total).unwrap();
        drop(part);
        let mut state = segmented_state(
            &request.url,
            "\"held-v1\"",
            total,
            vec![ByteRange {
                start: 0,
                end: done,
            }],
        );
        state.connections = 4;
        write_state(&state_path(&output), &state).unwrap();
        let recorded = fs::read(state_path(&output)).unwrap();
        let attempt = || {
            download(
                &request,
                &output,
                &TransferControl::default(),
                &NetworkGovernor::new(16),
                4,
                None,
                &mut |_| {},
            )
        };

        let held = hold_exclusively(&state_path(&output));
        let error = attempt().unwrap_err();
        assert!(!preserves_partial(&error), "{error:#}");
        drop(held);
        assert_eq!(fs::read(state_path(&output)).unwrap(), recorded);
        assert!(
            fs::read(part_path(&output)).unwrap()[..done as usize] == payload[..done as usize],
            "the verified bytes of the part were replaced"
        );

        let requests_before = server.requests().len();
        let result = attempt().unwrap_or_else(|error| panic!("{error:#}"));
        assert!(
            fs::read(result.path).unwrap() == payload,
            "the resumed download differs from the payload"
        );
        // The bytes before `done` were kept, so only the tail was requested again.
        let resumed_from_zero = server.requests()[requests_before..]
            .iter()
            .filter(|request| request.starts_with("GET "))
            .any(|request| request.to_ascii_lowercase().contains("range: bytes=0-"));
        assert!(!resumed_from_zero, "{:#?}", server.requests());
    }

    #[test]
    fn a_single_connection_partial_survives_a_state_record_that_cannot_be_read_for_a_moment() {
        let payload = patterned(MIN_SEGMENT_SIZE * 4);
        let total = payload.len() as u64;
        let server = PayloadServer::start(payload.clone(), "\"held-v1\"");
        let request = local_request(server.url());
        let directory = TestDirectory::new("held-state-single");
        let output = directory.0.join("large.bin");
        let done = total - MIN_SEGMENT_SIZE;
        let mut part = File::create(part_path(&output)).unwrap();
        part.write_all(&payload[..done as usize]).unwrap();
        part.set_len(total).unwrap();
        drop(part);
        let state = segmented_state(
            &request.url,
            "\"held-v1\"",
            total,
            vec![ByteRange {
                start: 0,
                end: done,
            }],
        );
        write_state(&state_path(&output), &state).unwrap();

        let held = hold_exclusively(&state_path(&output));
        // One connection: the per-file setting, or a retry the engine capped to one.
        let error = download(
            &request,
            &output,
            &TransferControl::default(),
            &NetworkGovernor::new(16),
            1,
            None,
            &mut |_| {},
        )
        .unwrap_err();
        drop(held);

        assert!(!preserves_partial(&error), "{error:#}");
        assert!(
            error.chain().any(|cause| cause
                .downcast_ref::<std::io::Error>()
                .and_then(std::io::Error::raw_os_error)
                == Some(32)),
            "the sharing violation reaches the retry rules: {error:#}"
        );
        assert!(
            fs::read(part_path(&output)).unwrap()[..done as usize] == payload[..done as usize],
            "the verified bytes of the part were replaced"
        );
    }

    #[test]
    fn an_ftp_partial_survives_a_state_record_that_cannot_be_read_for_a_moment() {
        let payload = patterned(128);
        let server = FtpFixture::start(
            payload.clone(),
            FtpBehavior {
                modification_time: true,
                resume: true,
                cut_first_transfer: Some(31),
            },
        );
        let directory = TestDirectory::new("held-state-ftp");
        let output = directory.0.join("payload.bin");
        assert!(ftp_download(&server, &output).is_err());
        assert_eq!(fs::metadata(part_path(&output)).unwrap().len(), 31);
        let recorded = fs::read(state_path(&output)).unwrap();

        let held = hold_exclusively(&state_path(&output));
        let error = ftp_download(&server, &output).unwrap_err();
        assert!(!preserves_partial(&error), "{error:#}");
        drop(held);
        assert_eq!(fs::read(state_path(&output)).unwrap(), recorded);
        assert_eq!(
            fs::read(part_path(&output)).expect("the partial file was kept"),
            payload[..31]
        );

        let result = ftp_download(&server, &output).unwrap_or_else(|e| panic!("{e:#}"));
        assert_eq!(fs::read(result.path).unwrap(), payload);
        assert!(
            server.commands().iter().any(|command| command == "REST 31"),
            "{:#?}",
            server.commands()
        );
    }

    const MIB: u64 = 1024 * 1024;

    #[test]
    fn a_lease_is_cut_only_while_its_back_half_is_worth_a_request() {
        let standing = |to_come: u64| Standing {
            claimed: 5,
            size: 5 + to_come,
        };
        assert_eq!(standing(0).cut_size(), None);
        assert_eq!(standing(2 * MIN_SEGMENT_SIZE - 1).cut_size(), None);
        // The back half is exactly the smallest piece that is cut from a range no worker has
        // taken: the worker that keeps the front keeps as much.
        assert_eq!(
            standing(2 * MIN_SEGMENT_SIZE).cut_size(),
            Some(5 + MIN_SEGMENT_SIZE)
        );
        // An odd byte stays with the worker that keeps the front.
        assert_eq!(
            standing(2 * MIN_SEGMENT_SIZE + 1).cut_size(),
            Some(5 + MIN_SEGMENT_SIZE + 1)
        );
    }

    #[test]
    fn a_worker_claims_nothing_beyond_the_size_its_lease_was_cut_to() {
        let allowance = Allowance::new();
        allowance.open(10 * MIB);
        assert_eq!(allowance.take(3 * MIB), 3 * MIB);
        let seen = allowance.view();
        assert!(allowance.cut(seen, 7 * MIB));
        assert_eq!(
            allowance.view(),
            Standing {
                claimed: 3 * MIB,
                size: 7 * MIB
            }
        );
        // A chunk that crosses the cut is granted only up to it, and nothing after that.
        assert_eq!(allowance.take(100 * MIB), 4 * MIB);
        assert_eq!(allowance.take(1), 0);
    }

    #[test]
    fn a_cut_made_on_a_view_that_is_out_of_date_is_refused() {
        let allowance = Allowance::new();
        allowance.open(10 * MIB);
        let seen = allowance.view();
        // The worker claims a chunk after the scheduler looked and before it cuts.
        assert_eq!(allowance.take(6 * MIB), 6 * MIB);
        assert!(!allowance.cut(seen, 5 * MIB));
        assert_eq!(
            allowance.view(),
            Standing {
                claimed: 6 * MIB,
                size: 10 * MIB
            }
        );
        // A view taken afterwards is cut beyond the claim.
        let seen = allowance.view();
        let size = seen.cut_size().unwrap();
        assert!(allowance.cut(seen, size));
        assert!(size >= 6 * MIB);
    }

    #[test]
    fn a_cut_lease_cannot_be_cut_twice_from_one_view() {
        let allowance = Allowance::new();
        allowance.open(12 * MIB);
        let seen = allowance.view();
        assert!(allowance.cut(seen, 9 * MIB));
        assert!(!allowance.cut(seen, 8 * MIB));
        assert_eq!(allowance.view().size, 9 * MIB);
    }

    #[test]
    fn the_claims_of_a_worker_and_the_range_cut_off_make_the_lease_exactly_once() {
        let mut random = Sequence::new(0x2545_F491_4F6C_DD1D);
        let mut cuts = 0;
        for _ in 0..300 {
            let size = 2 * MIN_SEGMENT_SIZE + random.below(6 * MIN_SEGMENT_SIZE);
            let allowance = Allowance::new();
            allowance.open(size);
            let mut claims = Vec::new();
            let mut position = 0;
            let mut claim = |allowance: &Allowance, position: &mut u64, wanted: u64| {
                let granted = allowance.take(wanted);
                claims.push((*position, granted));
                *position += granted;
                granted
            };
            // The scheduler looks after the worker has claimed an arbitrary number of chunks.
            let before_the_look = random.below(size / 2);
            while position < before_the_look {
                claim(&allowance, &mut position, 1 + random.below(65_536));
            }
            let seen = allowance.view();
            let Some(point) = seen.cut_size() else {
                continue;
            };
            cuts += 1;
            assert!(
                point >= position,
                "the cut {point} is before the claim {position}"
            );
            assert!(allowance.cut(seen, point));
            while claim(&allowance, &mut position, 1 + random.below(65_536)) > 0 {}
            // The worker's claims end exactly at the cut; the range cut off starts there.
            let mut tiles = claims
                .into_iter()
                .filter(|&(_, length)| length > 0)
                .collect::<Vec<_>>();
            tiles.push((point, size - point));
            let mut next = 0;
            for (start, length) in tiles {
                assert_eq!(start, next);
                next = start + length;
            }
            assert_eq!(next, size);
        }
        assert!(cuts > 100, "only {cuts} of 300 leases could be cut");
    }

    #[test]
    fn a_cut_that_races_with_the_claims_of_a_worker_never_gives_a_byte_to_both() {
        let mut random = Sequence::new(0x9E37_79B9_7F4A_7C15);
        let mut cuts = 0;
        for _ in 0..400 {
            let size = 8 * MIB;
            let allowance = Arc::new(Allowance::new());
            allowance.open(size);
            let chunks = Arc::new(AtomicU64::new(0));
            let worker = {
                let (allowance, chunks) = (allowance.clone(), chunks.clone());
                let mut random = Sequence::new(random.next_u64());
                thread::spawn(move || {
                    let mut claimed = 0;
                    loop {
                        let granted = allowance.take(1 + random.below(2_048));
                        if granted == 0 {
                            return claimed;
                        }
                        claimed += granted;
                        chunks.fetch_add(1, Ordering::AcqRel);
                    }
                })
            };
            // The scheduler cuts once the worker has claimed an arbitrary number of chunks,
            // while the worker goes on claiming on its own thread.
            let after = random.below(2_000);
            while chunks.load(Ordering::Acquire) < after {
                std::hint::spin_loop();
            }
            let cut = loop {
                let seen = allowance.view();
                let Some(point) = seen.cut_size() else {
                    break None;
                };
                if allowance.cut(seen, point) {
                    break Some(point);
                }
            };
            let claimed = worker.join().unwrap();
            match cut {
                Some(point) => {
                    cuts += 1;
                    assert_eq!(claimed, point, "the worker and the cut disagree");
                }
                None => assert_eq!(claimed, size),
            }
        }
        assert!(cuts > 100, "only {cuts} of 400 races ended in a cut");
    }

    /// A ledger of `workers` writers over one part file.
    fn ledger_over(directory: &TestDirectory, workers: usize, total: u64) -> Checkpoints {
        let part = directory.0.join("payload.bin.ssdownload.part");
        let open = || {
            OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&part)
                .unwrap()
        };
        Checkpoints::new(
            open(),
            (0..workers).map(|_| Arc::new(open())).collect(),
            &directory.0.join("payload.bin.ssdownload.state"),
            ResumeState::default(),
            Vec::new(),
            total,
        )
    }

    fn range(start: u64, end: u64) -> ByteRange {
        ByteRange { start, end }
    }

    fn work(start: u64, end: u64) -> RangeWork {
        RangeWork {
            range: range(start, end),
            attempts: 0,
        }
    }

    /// What `Checkpoints::split` hands out, as values that compare: the worker, the range and
    /// its attempts.
    fn stolen(
        ledger: &mut Checkpoints,
        idle: &[usize],
        now: Instant,
    ) -> Option<(usize, ByteRange, u8)> {
        ledger
            .split(&mut idle.to_vec(), now)
            .map(|(worker, work)| (worker, work.range, work.attempts))
    }

    /// What a worker looks like that received its last range at `rate` bytes per second without
    /// a wait for the first byte.
    fn without_wait(rate: u64) -> Option<Delivery> {
        Some(Delivery {
            wait: Duration::ZERO,
            rate,
        })
    }

    /// Hands `work` to `worker` at `start`; its first byte comes at once.
    fn start_lease(ledger: &mut Checkpoints, worker: usize, work: RangeWork, start: Instant) {
        ledger.assign(worker, work, start);
        ledger.allowance(worker).note_first_byte(Duration::ZERO);
    }

    /// A ledger of four workers, of which worker 0 has held `[0, 8 MiB)` since `start` and
    /// claimed 2 MiB of it: a second later it fetches 2 MiB/s, with 6 MiB to come. The others
    /// are idle.
    fn ledger_with_a_lease(directory: &TestDirectory, start: Instant) -> Checkpoints {
        let mut ledger = ledger_over(directory, 4, 8 * MIB);
        start_lease(&mut ledger, 0, work(0, 8 * MIB), start);
        assert_eq!(ledger.allowance(0).take(2 * MIB), 2 * MIB);
        ledger
    }

    #[test]
    fn the_lease_with_the_most_bytes_to_come_is_cut_and_its_back_half_is_returned() {
        let directory = TestDirectory::new("split-largest");
        let mut ledger = ledger_over(&directory, 4, 25 * MIB);
        let start = Instant::now();
        let now = start + Duration::from_secs(2);
        start_lease(&mut ledger, 0, work(0, 8 * MIB), start);
        start_lease(&mut ledger, 1, work(8 * MIB, 16 * MIB), start);
        start_lease(&mut ledger, 2, work(16 * MIB, 25 * MIB), start);
        assert_eq!(ledger.allowance(0).take(3 * MIB), 3 * MIB);
        assert_eq!(ledger.allowance(1).take(6 * MIB), 6 * MIB);
        assert_eq!(ledger.allowance(2).take(MIB), MIB);
        // Worker 3 fetched its last range at 4 MiB/s, faster than any of the leases runs.
        ledger.deliveries[3] = without_wait(4 * MIB);

        // Worker 2 has the most to come, 8 MiB: it keeps the front half.
        assert_eq!(
            stolen(&mut ledger, &[3], now),
            Some((3, range(21 * MIB, 25 * MIB), 0))
        );
        assert_eq!(
            ledger.leases[2].as_ref().unwrap().range,
            range(16 * MIB, 21 * MIB)
        );
        assert_eq!(ledger.allowance(2).view().size, 5 * MIB);
        // Now worker 0 has the most to come, 5 MiB; the odd half goes to the worker that
        // keeps the front.
        assert_eq!(
            stolen(&mut ledger, &[3], now),
            Some((3, range(5 * MIB + MIB / 2, 8 * MIB), 0))
        );
        // Worker 2 has 4 MiB to come, and its back half is 2 MiB.
        assert_eq!(
            stolen(&mut ledger, &[3], now),
            Some((3, range(19 * MIB, 21 * MIB), 0))
        );
        // No lease has 4 MiB to come any more.
        assert_eq!(stolen(&mut ledger, &[3], now), None);
    }

    #[test]
    fn a_cut_lease_is_recorded_and_asked_for_again_only_up_to_its_cut() {
        let directory = TestDirectory::new("split-record");
        let start = Instant::now();
        let now = start + Duration::from_secs(1);
        let whole = work(0, 8 * MIB);
        // With 1 MiB claimed, 7 MiB are to come and the back 3.5 MiB are cut off.
        let cut_at = 4 * MIB + MIB / 2;
        let back = range(cut_at, 8 * MIB);

        let mut ledger = ledger_over(&directory, 2, 8 * MIB);
        ledger.deliveries[1] = without_wait(MIB);
        start_lease(&mut ledger, 0, whole, start);
        assert_eq!(ledger.allowance(0).take(MIB), MIB);
        ledger.counter(0).store(MIB, Ordering::Release);
        assert_eq!(stolen(&mut ledger, &[1], now), Some((1, back, 0)));
        // The range cut off is a lease of its own with nothing recorded, and a record lists
        // each lease from its start up to what it wrote, never beyond the cut.
        ledger.assign(1, work(cut_at, 8 * MIB), now);
        ledger.counter(1).store(MIB, Ordering::Release);
        assert_eq!(ledger.allowance(1).take(MIB), MIB);
        ledger.allowance(0).take(MIB);
        ledger.counter(0).store(2 * MIB, Ordering::Release);
        let (record, _) = ledger.snapshot().unwrap().unwrap();
        assert_eq!(
            record.completed,
            [range(0, 2 * MIB), range(cut_at, cut_at + MIB)]
        );
        // The cut lease fails: only the part before the cut is asked for again.
        assert_eq!(ledger.abandon(0), Some(range(2 * MIB, cut_at)));

        // A cut lease that completes puts its front part, and no more, among the ranges that
        // wait for the next cycle.
        let mut ledger = ledger_over(&directory, 2, 8 * MIB);
        ledger.deliveries[1] = without_wait(MIB);
        start_lease(&mut ledger, 0, whole, start);
        assert_eq!(ledger.allowance(0).take(MIB), MIB);
        assert_eq!(stolen(&mut ledger, &[1], now), Some((1, back, 0)));
        assert_eq!(ledger.allowance(0).take(8 * MIB), cut_at - MIB);
        ledger.counter(0).store(cut_at, Ordering::Release);
        ledger.finish(0, now);
        assert_eq!(ledger.unrecorded, [range(0, cut_at)]);
    }

    #[test]
    fn a_chunk_in_flight_counts_as_claimed_when_a_lease_is_cut() {
        let directory = TestDirectory::new("split-in-flight");
        let start = Instant::now();
        let now = start + Duration::from_secs(1);
        let mut ledger = ledger_over(&directory, 2, 8 * MIB);
        ledger.deliveries[1] = without_wait(8 * MIB);
        start_lease(&mut ledger, 0, work(0, 8 * MIB), start);
        // A 5 MiB chunk is claimed and not yet written, so the published count is still 0; only
        // 3 MiB of the lease are left to come, too few to cut.
        assert_eq!(ledger.allowance(0).take(5 * MIB), 5 * MIB);
        assert_eq!(ledger.counter(0).load(Ordering::Acquire), 0);
        assert_eq!(stolen(&mut ledger, &[1], now), None);

        // With a 3 MiB chunk in flight 5 MiB are left, and the cut lies beyond the chunk.
        let mut ledger = ledger_over(&directory, 2, 8 * MIB);
        ledger.deliveries[1] = without_wait(8 * MIB);
        start_lease(&mut ledger, 0, work(0, 8 * MIB), start);
        assert_eq!(ledger.allowance(0).take(3 * MIB), 3 * MIB);
        assert_eq!(ledger.counter(0).load(Ordering::Acquire), 0);
        let (_, back, _) = stolen(&mut ledger, &[1], now).expect("5 MiB are left to come");
        assert!(
            back.start >= 3 * MIB,
            "{back:?} overlaps the chunk in flight"
        );
        assert_eq!(back.end, 8 * MIB);
    }

    #[test]
    fn a_lease_is_cut_only_for_a_worker_that_fetches_the_back_half_sooner_than_the_lease_ends() {
        let directory = TestDirectory::new("split-rate");
        let start = Instant::now();
        let now = start + Duration::from_secs(1);
        // The lease fetches 2 MiB/s with 6 MiB to come, so the back half is 3 MiB. A worker at
        // half that rate needs as long for the half as the lease needs for all of it: no gain.
        let cases = [
            (None, false),
            (Some(0), false),
            (Some(MIB), false),
            (Some(MIB + 1), true),
            (Some(8 * MIB), true),
        ];
        for (rate, cuts) in cases {
            let mut ledger = ledger_with_a_lease(&directory, start);
            ledger.deliveries[1] = rate.and_then(without_wait);
            assert_eq!(
                stolen(&mut ledger, &[1], now).is_some(),
                cuts,
                "an idle worker at {rate:?} bytes per second"
            );
            if !cuts {
                assert_eq!(ledger.allowance(0).view().size, 8 * MIB);
                assert_eq!(ledger.leases[0].as_ref().unwrap().range, range(0, 8 * MIB));
            }
        }
    }

    #[test]
    fn the_wait_of_the_idle_worker_for_a_first_byte_counts_against_the_cut() {
        let directory = TestDirectory::new("split-wait");
        let start = Instant::now();
        let now = start + Duration::from_secs(1);
        // The lease fetches 2 MiB/s with 6 MiB to come and would need 3 s for them. The idle
        // worker fetches 6 MiB/s, so the back half of 3 MiB takes it half a second once its first
        // byte has come: it has the half sooner than the lease ends only if it waits less than
        // 2.5 s for that byte, to the nanosecond.
        let cases = [
            (Duration::ZERO, true),
            (Duration::from_secs(2), true),
            (Duration::from_nanos(2_499_999_999), true),
            (Duration::from_millis(2_500), false),
            (Duration::from_nanos(2_500_000_001), false),
            (Duration::from_secs(60), false),
        ];
        for (wait, cuts) in cases {
            let mut ledger = ledger_with_a_lease(&directory, start);
            ledger.deliveries[1] = Some(Delivery {
                wait,
                rate: 6 * MIB,
            });
            assert_eq!(
                stolen(&mut ledger, &[1], now).is_some(),
                cuts,
                "an idle worker that waited {wait:?} for its first byte"
            );
            if !cuts {
                assert_eq!(ledger.allowance(0).view().size, 8 * MIB);
            }
        }
    }

    #[test]
    fn a_lease_runs_at_the_rate_it_has_had_since_its_first_byte() {
        let directory = TestDirectory::new("split-from-first-byte");
        let start = Instant::now();
        let now = start + Duration::from_secs(4);
        // The lease was handed out four seconds ago, waited three for its first byte and has
        // fetched 2 MiB in the second since: it runs at 2 MiB/s, not at the 0.5 MiB/s of the four
        // seconds, and an idle worker at 0.5 MiB/s is too slow to take the back half of it.
        for (rate, cuts) in [(MIB / 2, false), (MIB + MIB / 2, true)] {
            let mut ledger = ledger_over(&directory, 2, 8 * MIB);
            ledger.deliveries[1] = without_wait(rate);
            ledger.assign(0, work(0, 8 * MIB), start);
            ledger.allowance(0).note_first_byte(Duration::from_secs(3));
            assert_eq!(ledger.allowance(0).take(2 * MIB), 2 * MIB);
            assert_eq!(
                stolen(&mut ledger, &[1], now).is_some(),
                cuts,
                "an idle worker at {rate} bytes per second"
            );
        }
    }

    #[test]
    fn a_lease_whose_rate_is_not_known_is_not_cut() {
        let directory = TestDirectory::new("split-unknown-rate");
        let start = Instant::now();
        let mut ledger = ledger_over(&directory, 2, 8 * MIB);
        ledger.deliveries[1] = without_wait(8 * MIB);
        ledger.assign(0, work(0, 8 * MIB), start);
        // Nothing has arrived, however long it has been.
        let minute = start + Duration::from_secs(60);
        assert_eq!(stolen(&mut ledger, &[1], minute), None);
        // Bytes are claimed that no first byte was noted for: they show no rate either.
        assert_eq!(ledger.allowance(0).take(MIB), MIB);
        assert_eq!(stolen(&mut ledger, &[1], minute), None);
        // The first byte came after five seconds, and no time has passed since.
        ledger.allowance(0).note_first_byte(Duration::from_secs(5));
        assert_eq!(
            stolen(&mut ledger, &[1], start + Duration::from_secs(5)),
            None
        );
        assert!(stolen(&mut ledger, &[1], start + Duration::from_secs(6)).is_some());
    }

    #[test]
    fn a_finished_lease_sets_the_wait_of_its_worker_from_the_request_that_brought_the_first_byte() {
        let directory = TestDirectory::new("split-finish-request");
        let start = Instant::now();
        let mut ledger = ledger_over(&directory, 2, 16 * MIB);
        // The worker was handed the lease and sent its request three seconds later, say after
        // it renewed an expired address; the first byte came a second after that. The lease flowed
        // for the two seconds from the first byte to the end, at 4 MiB/s. A request of the worker
        // for another lease waits a second for its first byte, not four.
        ledger.assign(0, work(0, 8 * MIB), start);
        let allowance = ledger.allowance(0);
        allowance.note_connection();
        allowance.note_request(Duration::from_secs(3));
        allowance.note_first_byte(Duration::from_secs(4));
        ledger.finish(0, start + Duration::from_secs(6));
        assert_eq!(
            ledger.deliveries[0],
            Some(Delivery {
                wait: Duration::from_secs(1),
                rate: 4 * MIB
            })
        );
        assert_eq!(ledger.connection_wait, Some(Duration::from_secs(1)));

        // A lease that is opened again forgets when its last request was sent.
        let allowance = Allowance::new();
        allowance.note_request(Duration::from_secs(3));
        allowance.open(8 * MIB);
        assert_eq!(
            allowance.answer_wait(Duration::from_secs(4)),
            Duration::from_secs(4)
        );
    }

    #[test]
    fn a_lease_that_is_opened_again_has_no_first_byte_until_one_is_noted() {
        let allowance = Allowance::new();
        assert_eq!(allowance.first_byte_wait(), None);
        allowance.open(8 * MIB);
        allowance.note_first_byte(Duration::from_millis(1_500));
        assert_eq!(
            allowance.first_byte_wait(),
            Some(Duration::from_millis(1_500))
        );
        // A byte that came at once is a wait of nothing, not a missing byte.
        allowance.note_first_byte(Duration::ZERO);
        assert_eq!(allowance.first_byte_wait(), Some(Duration::ZERO));
        allowance.open(4 * MIB);
        assert_eq!(allowance.first_byte_wait(), None);
    }

    #[test]
    fn the_comparison_of_waits_and_rates_cannot_overflow_or_divide_by_zero() {
        let lease = Standing {
            claimed: 0,
            size: u64::from(u32::MAX),
        };
        let longest = Delivery {
            wait: Duration::MAX,
            rate: u64::MAX,
        };
        assert!(!longest.gets_the_back_half_sooner(lease, u64::MAX));
        // A lease at 0 bytes per second is beaten by a worker with any rate, and a worker
        // at 0 beats nothing.
        assert!(longest.gets_the_back_half_sooner(lease, 0));
        assert!(!without_wait(0).unwrap().gets_the_back_half_sooner(lease, 0));
        assert!(!without_wait(0)
            .unwrap()
            .gets_the_back_half_sooner(lease, u64::MAX));
    }

    #[test]
    fn the_idle_worker_that_fetched_its_last_range_fastest_takes_the_back_half() {
        let directory = TestDirectory::new("split-fastest");
        let start = Instant::now();
        let now = start + Duration::from_secs(1);
        // Only worker 2 is fast enough for the lease at 2 MiB/s; worker 3 has not finished a
        // range. The worker that takes the half is no longer idle, and the others still are.
        for order in [[1, 2, 3], [3, 2, 1], [2, 3, 1]] {
            let mut ledger = ledger_with_a_lease(&directory, start);
            ledger.deliveries[1] = without_wait(MIB / 2);
            ledger.deliveries[2] = without_wait(4 * MIB);
            let mut idle = order.to_vec();
            let (worker, work) = ledger
                .split(&mut idle, now)
                .expect("worker 2 is fast enough");
            assert_eq!((worker, work.range), (2, range(5 * MIB, 8 * MIB)));
            let mut others = order.to_vec();
            others.retain(|&other| other != 2);
            assert_eq!(idle, others, "idle workers {order:?}");
        }
        let mut ledger = ledger_with_a_lease(&directory, start);
        let mut idle = vec![1, 2, 3];
        assert!(ledger.split(&mut idle, now).is_none());
        assert_eq!(idle, [1, 2, 3]);
    }

    #[test]
    fn a_lease_that_the_idle_worker_would_not_finish_sooner_is_passed_over_for_one_it_would() {
        let directory = TestDirectory::new("split-passed-over");
        let start = Instant::now();
        let now = start + Duration::from_millis(250);
        let mut ledger = ledger_over(&directory, 3, 15 * MIB);
        // Worker 0 fetches 8 MiB/s with 7 MiB to come, more than worker 1 has. The idle worker
        // fetches 3 MiB/s, which is less than half of that, and more than half of the 4 MiB/s of
        // worker 1.
        start_lease(&mut ledger, 0, work(0, 9 * MIB), start);
        start_lease(&mut ledger, 1, work(9 * MIB, 15 * MIB), start);
        assert_eq!(ledger.allowance(0).take(2 * MIB), 2 * MIB);
        assert_eq!(ledger.allowance(1).take(MIB), MIB);
        ledger.deliveries[2] = without_wait(3 * MIB);
        assert_eq!(
            stolen(&mut ledger, &[2], now),
            Some((2, range(12 * MIB + MIB / 2, 15 * MIB), 0))
        );
        assert_eq!(ledger.allowance(0).view().size, 9 * MIB);
    }

    #[test]
    fn a_lease_is_cut_only_once_it_has_flowed_long_enough_for_its_rate_to_count() {
        let directory = TestDirectory::new("split-sample");
        let start = Instant::now();
        // The first byte comes after a second, and 64 KiB of the lease come in the first moments
        // after it: a rate of 0.25 MiB/s, which a worker that fetched its last range at 64 MiB/s
        // beats by far. The time that counts is the time since the first byte.
        let wait = Duration::from_secs(1);
        for (flowed, cuts) in [
            (Duration::ZERO, false),
            (MIN_RATE_SAMPLE - Duration::from_nanos(1), false),
            (MIN_RATE_SAMPLE, true),
        ] {
            let mut ledger = ledger_over(&directory, 2, 8 * MIB);
            ledger.deliveries[1] = without_wait(64 * MIB);
            ledger.assign(0, work(0, 8 * MIB), start);
            ledger.allowance(0).note_first_byte(wait);
            assert_eq!(ledger.allowance(0).take(64 * 1024), 64 * 1024);
            assert_eq!(
                stolen(&mut ledger, &[1], start + wait + flowed).is_some(),
                cuts,
                "a lease that has flowed for {flowed:?}"
            );
        }
    }

    #[test]
    fn the_range_cut_off_a_lease_keeps_the_attempts_of_the_lease() {
        let directory = TestDirectory::new("split-attempts");
        let start = Instant::now();
        let mut ledger = ledger_over(&directory, 2, 8 * MIB);
        ledger.deliveries[1] = without_wait(8 * MIB);
        let retried = RangeWork {
            range: range(0, 8 * MIB),
            attempts: 2,
        };
        start_lease(&mut ledger, 0, retried, start);
        assert_eq!(ledger.allowance(0).take(2 * MIB), 2 * MIB);
        assert_eq!(
            stolen(&mut ledger, &[1], start + Duration::from_secs(1)),
            Some((1, range(5 * MIB, 8 * MIB), 2))
        );
    }

    #[test]
    fn a_finished_lease_sets_the_wait_and_the_rate_of_its_worker_from_its_first_byte() {
        let directory = TestDirectory::new("split-finish-delivery");
        let start = Instant::now();
        let mut ledger = ledger_over(&directory, 2, 16 * MIB);
        assert_eq!(ledger.deliveries[0], None);
        // A lease that ends the moment its first byte comes shows no rate.
        ledger.assign(0, work(0, 8 * MIB), start);
        ledger.allowance(0).note_first_byte(Duration::from_secs(1));
        ledger.finish(0, start + Duration::from_secs(1));
        assert_eq!(ledger.deliveries[0], None);

        // The first byte of 8 MiB came after a second and the last after three: 4 MiB/s once the
        // bytes flowed.
        ledger.assign(0, work(0, 8 * MIB), start);
        ledger.allowance(0).note_first_byte(Duration::from_secs(1));
        ledger.finish(0, start + Duration::from_secs(3));
        assert_eq!(
            ledger.deliveries[0],
            Some(Delivery {
                wait: Duration::from_secs(1),
                rate: 4 * MIB
            })
        );

        // A lease that was cut kept 5 MiB of its 8 and fetched them in 5 s.
        ledger.deliveries[1] = without_wait(4 * MIB);
        start_lease(&mut ledger, 0, work(8 * MIB, 16 * MIB), start);
        assert_eq!(ledger.allowance(0).take(2 * MIB), 2 * MIB);
        assert_eq!(
            stolen(&mut ledger, &[1], start + Duration::from_secs(1)),
            Some((1, range(13 * MIB, 16 * MIB), 0))
        );
        assert_eq!(ledger.allowance(0).take(8 * MIB), 3 * MIB);
        ledger.finish(0, start + Duration::from_secs(5));
        assert_eq!(ledger.deliveries[0], without_wait(MIB));
    }

    #[test]
    fn a_lease_shorter_than_a_minimum_segment_leaves_the_delivery_of_its_worker_as_it_was() {
        let directory = TestDirectory::new("split-finish-short");
        let start = Instant::now();
        // The first byte comes after 100 ms and the last a millisecond later: a rate of a few
        // GiB/s, which no lasting transfer has.
        let finished = start + Duration::from_millis(101);
        for (length, sets) in [
            (MIN_SEGMENT_SIZE - 1, false),
            (MIN_SEGMENT_SIZE, true),
            (MIN_SEGMENT_SIZE + 1, true),
        ] {
            let mut ledger = ledger_over(&directory, 1, 16 * MIB);
            ledger.assign(0, work(0, length), start);
            ledger
                .allowance(0)
                .note_first_byte(Duration::from_millis(100));
            ledger.finish(0, finished);
            assert_eq!(
                ledger.deliveries[0].is_some(),
                sets,
                "a lease of {length} bytes"
            );
        }

        let mut ledger = ledger_over(&directory, 1, 16 * MIB);
        let before = Delivery {
            wait: Duration::from_secs(1),
            rate: 4 * MIB,
        };
        ledger.deliveries[0] = Some(before);
        ledger.assign(0, work(8 * MIB, 8 * MIB + 64 * 1024), start);
        ledger.allowance(0).note_first_byte(Duration::ZERO);
        ledger.finish(0, start + Duration::from_millis(1));
        assert_eq!(ledger.deliveries[0], Some(before));
    }

    #[test]
    fn a_finished_lease_notes_when_its_worker_went_idle_and_how_long_a_new_connection_waited() {
        let directory = TestDirectory::new("split-finish-connection");
        let start = Instant::now();
        let mut ledger = ledger_over(&directory, 2, 16 * MIB);
        assert_eq!((ledger.idle_since[0], ledger.connection_wait), (None, None));

        // A lease that reused a connection leaves what a new connection waited as it was, and
        // a lease with no first byte is not a wait at all, but both leave their worker idle.
        ledger.assign(0, work(0, 8 * MIB), start);
        ledger
            .allowance(0)
            .note_first_byte(Duration::from_millis(300));
        ledger.finish(0, start + Duration::from_secs(2));
        assert_eq!(ledger.idle_since[0], Some(start + Duration::from_secs(2)));
        assert_eq!(ledger.connection_wait, None);
        ledger.assign(1, work(8 * MIB, 16 * MIB), start);
        ledger.allowance(1).note_connection();
        ledger.finish(1, start + Duration::from_secs(3));
        assert_eq!(ledger.idle_since[1], Some(start + Duration::from_secs(3)));
        assert_eq!(ledger.connection_wait, None);

        // A lease that opened a connection sets it, however short, and the latest sets it again.
        ledger.assign(1, work(8 * MIB, 8 * MIB + 64 * 1024), start);
        ledger
            .allowance(1)
            .note_first_byte(Duration::from_millis(700));
        ledger.allowance(1).note_connection();
        ledger.finish(1, start + Duration::from_secs(4));
        assert_eq!(ledger.connection_wait, Some(Duration::from_millis(700)));
        ledger.assign(0, work(0, 8 * MIB), start);
        ledger
            .allowance(0)
            .note_first_byte(Duration::from_millis(500));
        ledger.allowance(0).note_connection();
        ledger.finish(0, start + Duration::from_secs(5));
        assert_eq!(ledger.connection_wait, Some(Duration::from_millis(500)));

        // Whether a lease opened a connection is noted for that lease alone.
        ledger.assign(0, work(0, 8 * MIB), start);
        ledger
            .allowance(0)
            .note_first_byte(Duration::from_millis(900));
        ledger.finish(0, start + Duration::from_secs(6));
        assert_eq!(ledger.connection_wait, Some(Duration::from_millis(500)));
    }

    #[test]
    fn an_idle_worker_that_has_closed_its_socket_waits_as_long_as_a_new_connection_did() {
        let directory = TestDirectory::new("split-closed-socket");
        let start = Instant::now();
        let now = start + Duration::from_secs(1);
        // As in the test of the wait for a first byte: the lease needs 3 s for the 6 MiB to
        // come, and the idle worker at 6 MiB/s has the back half sooner only if it waits less
        // than 2.5 s for its first byte, to the nanosecond.
        let setup = Some(Duration::from_millis(2_500));
        let cases = [
            (Duration::ZERO, true),
            (RANGE_WORKER_IDLE_TIMEOUT - Duration::from_nanos(1), true),
            (RANGE_WORKER_IDLE_TIMEOUT, false),
            (RANGE_WORKER_IDLE_TIMEOUT + Duration::from_nanos(1), false),
            (Duration::from_secs(1), false),
        ];
        for (idle, cuts) in cases {
            let mut ledger = ledger_with_a_lease(&directory, start);
            ledger.deliveries[1] = without_wait(6 * MIB);
            ledger.idle_since[1] = Some(now - idle);
            ledger.connection_wait = setup;
            assert_eq!(
                stolen(&mut ledger, &[1], now).is_some(),
                cuts,
                "an idle worker that finished its last range {idle:?} ago"
            );
        }

        // Its wait is the longer of the two, not the sum, and the setup to the nanosecond.
        let cases = [
            (None, Duration::from_millis(2_400), true),
            (None, Duration::from_millis(2_500), false),
            (
                Some(Duration::from_secs(1)),
                Duration::from_millis(2_400),
                true,
            ),
            (
                Some(Duration::from_secs(1)),
                Duration::from_millis(2_500),
                false,
            ),
            (Some(Duration::from_millis(2_400)), Duration::ZERO, true),
            (
                Some(Duration::from_nanos(2_499_999_999)),
                Duration::ZERO,
                true,
            ),
            (Some(Duration::from_millis(2_500)), Duration::ZERO, false),
            (Some(Duration::MAX), Duration::ZERO, false),
        ];
        for (setup, wait, cuts) in cases {
            let mut ledger = ledger_with_a_lease(&directory, start);
            ledger.deliveries[1] = Some(Delivery {
                wait,
                rate: 6 * MIB,
            });
            ledger.idle_since[1] = Some(start);
            ledger.connection_wait = setup;
            assert_eq!(
                stolen(&mut ledger, &[1], now).is_some(),
                cuts,
                "a closed socket after a wait of {wait:?} and a setup of {setup:?}"
            );
        }

        // A worker that finished after `now`, or never, has not closed its socket.
        for since in [Some(now + Duration::from_secs(1)), None] {
            let mut ledger = ledger_with_a_lease(&directory, start);
            ledger.deliveries[1] = without_wait(6 * MIB);
            ledger.idle_since[1] = since;
            ledger.connection_wait = setup;
            assert!(stolen(&mut ledger, &[1], now).is_some(), "{since:?}");
        }
    }

    fn address(name: &str) -> Url {
        Url::parse(&format!("http://files.example/{name}")).unwrap()
    }

    #[test]
    fn a_target_is_renewed_once_per_generation_and_a_later_one_only_after_a_byte_came_from_it() {
        let asked = AtomicUsize::new(0);
        let follow = |from: &'static str, to: &'static str| {
            let asked = &asked;
            move |original: &Url, failed: &Url| {
                assert_eq!(original, &address("file"));
                assert_eq!(failed, &address(from));
                asked.fetch_add(1, Ordering::AcqRel);
                Some(address(to))
            }
        };
        let target = Target::new(Some(address("file")), address("a"));

        // The first address is renewed on its first refusal, without a byte having come.
        assert!(target.renew(0, || false, follow("a", "b")));
        assert_eq!(target.current(), (address("b"), 1));
        // A worker whose request to the first address was refused later uses the new one
        // without another request for the source.
        assert!(target.renew(0, || false, |_, _| panic!("asked again")));
        assert_eq!(asked.load(Ordering::Acquire), 1);
        // The new address has delivered nothing: it is not renewed, and stays refused.
        assert!(!target.renew(1, || false, |_, _| panic!("renewed without progress")));
        assert!(!target.renew(1, || false, |_, _| panic!("renewed without progress")));
        assert_eq!(target.current(), (address("b"), 1));

        // A byte from an older generation does not count for the newer one.
        let target = Target::new(Some(address("file")), address("a"));
        target.note_delivery(0);
        assert!(target.renew(0, || false, follow("a", "b")));
        assert!(!target.renew(1, || false, |_, _| panic!("renewed on an older delivery")));

        // A byte from the new address allows the next renewal.
        let target = Target::new(Some(address("file")), address("a"));
        assert!(target.renew(0, || false, follow("a", "b")));
        target.note_delivery(1);
        assert!(target.renew(1, || false, follow("b", "c")));
        assert_eq!(target.current(), (address("c"), 2));
        assert!(!target.renew(2, || false, |_, _| panic!("renewed without progress")));
    }

    #[test]
    fn a_renewal_that_fails_is_final_and_a_direct_address_is_never_renewed() {
        let target = Target::new(Some(address("file")), address("a"));
        assert!(!target.renew(0, || false, |_, _| None));
        assert!(!target.renew(0, || false, |_, _| panic!("asked again after a failure")));
        assert_eq!(target.current(), (address("a"), 0));

        let direct = Target::new(None, address("file"));
        assert!(!direct.renew(
            0,
            || false,
            |_, _| panic!("a direct address has no source to ask")
        ));
    }

    #[test]
    fn workers_that_are_refused_together_renew_the_target_once() {
        let target = Arc::new(Target::new(Some(address("file")), address("a")));
        let asked = Arc::new(AtomicUsize::new(0));
        let workers = (0..8)
            .map(|_| {
                let (target, asked) = (target.clone(), asked.clone());
                thread::spawn(move || {
                    target.renew(
                        0,
                        || false,
                        |_, _| {
                            asked.fetch_add(1, Ordering::AcqRel);
                            thread::sleep(Duration::from_millis(20));
                            Some(address("b"))
                        },
                    )
                })
            })
            .collect::<Vec<_>>();
        for worker in workers {
            assert!(worker.join().unwrap());
        }
        assert_eq!(asked.load(Ordering::Acquire), 1);
        assert_eq!(target.current(), (address("b"), 1));
    }

    const ENOUGH: Duration = Duration::from_secs(5);

    /// A renewal that is under way: it has begun and ends when `release` is sent.
    struct Renewal {
        release: mpsc::Sender<()>,
        worker: thread::JoinHandle<bool>,
    }

    impl Renewal {
        fn begin(target: &Arc<Target>, panics: bool) -> Self {
            let (started_tx, started_rx) = mpsc::channel();
            let (release, release_rx) = mpsc::channel::<()>();
            let target = target.clone();
            let worker = thread::spawn(move || {
                target.renew(
                    0,
                    || false,
                    |_, _| {
                        started_tx.send(()).unwrap();
                        release_rx.recv().unwrap();
                        assert!(!panics, "the renewal panics");
                        Some(address("b"))
                    },
                )
            });
            started_rx.recv_timeout(ENOUGH).unwrap();
            Self { release, worker }
        }

        fn end(self) -> thread::Result<bool> {
            self.release.send(()).unwrap();
            self.worker.join()
        }
    }

    /// Runs `work` on another thread, so that a call that waits fails the test by `ENOUGH`
    /// instead of hanging it.
    fn on_a_thread<T: Send + 'static>(
        work: impl FnOnce() -> T + Send + 'static,
    ) -> mpsc::Receiver<T> {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(work());
        });
        rx
    }

    #[test]
    fn a_renewal_in_progress_holds_no_lock_and_makes_no_worker_wait_for_the_address() {
        let target = Arc::new(Target::new(Some(address("file")), address("a")));
        let renewal = Renewal::begin(&target, false);
        let asked = on_a_thread({
            let target = target.clone();
            move || target.current()
        });
        let got = asked.recv_timeout(ENOUGH);
        // Released before the check, so that a failure does not leave the renewal waiting.
        assert!(renewal.end().unwrap());
        assert_eq!(got, Ok((address("a"), 0)));
        assert_eq!(target.current(), (address("b"), 1));
    }

    #[test]
    fn a_worker_refused_during_a_renewal_waits_for_its_result() {
        let target = Arc::new(Target::new(Some(address("file")), address("a")));
        let renewal = Renewal::begin(&target, false);
        let waiter = on_a_thread({
            let target = target.clone();
            move || target.renew(0, || false, |_, _| panic!("a second renewal began"))
        });
        // It has no result to use yet, however long it waits.
        assert!(waiter.recv_timeout(Duration::from_millis(500)).is_err());
        assert!(renewal.end().unwrap());
        assert_eq!(waiter.recv_timeout(ENOUGH), Ok(true));
    }

    #[test]
    fn a_worker_that_waits_for_a_renewal_gives_up_when_the_transfer_is_stopped() {
        let target = Arc::new(Target::new(Some(address("file")), address("a")));
        let renewal = Renewal::begin(&target, false);
        let stopped = Arc::new(AtomicBool::new(false));
        let waiter = on_a_thread({
            let (target, stopped) = (target.clone(), stopped.clone());
            move || {
                target.renew(
                    0,
                    || stopped.load(Ordering::Acquire),
                    |_, _| panic!("a second renewal began"),
                )
            }
        });
        assert!(waiter.recv_timeout(Duration::from_millis(300)).is_err());
        stopped.store(true, Ordering::Release);
        let got = waiter.recv_timeout(ENOUGH);
        assert!(renewal.end().unwrap());
        assert_eq!(got, Ok(false));
    }

    #[test]
    fn a_renewal_that_panics_wakes_its_waiters_and_counts_as_failed() {
        let target = Arc::new(Target::new(Some(address("file")), address("a")));
        let renewal = Renewal::begin(&target, true);
        let waiter = on_a_thread({
            let target = target.clone();
            move || target.renew(0, || false, |_, _| panic!("a second renewal began"))
        });
        assert!(waiter.recv_timeout(Duration::from_millis(300)).is_err());
        assert!(renewal.end().is_err());
        assert_eq!(waiter.recv_timeout(ENOUGH), Ok(false));
        assert!(!target.renew(0, || false, |_, _| panic!("asked again after a panic")));
    }
}

mod digest;
#[cfg(test)]
mod loopback;
