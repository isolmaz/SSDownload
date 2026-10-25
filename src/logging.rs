//! Session event log for the desktop application, the CLI and the browser extension.
//!
//! One event stream feeds two files: `logs/events-YYYYMMDD.jsonl` for queries and
//! `logs/ssdownload-YYYYMMDD.log` for humans. Every launch writes a session banner, so any
//! report can be traced back to the run that produced it. A failing write never affects a
//! download: the writer runs on its own thread, the queue is bounded, and an overflow is
//! counted instead of blocking.
//!
//! Secrets stay out of the log. URLs lose user information, query and fragment; credential-like
//! tokens are dropped; every detail line is collapsed to one line and length-bounded.

use crate::error_codes::{self, Code, Severity};
use crate::paths::AppPaths;
use anyhow::Result;
use serde::Serialize;
use std::{
    borrow::Cow,
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering},
        mpsc::{sync_channel, Receiver, SyncSender},
        OnceLock,
    },
    thread,
    time::Instant,
};

const QUEUE_CAPACITY: usize = 4096;
const FILE_LIMIT: u64 = 5 * 1024 * 1024;
const FILE_FAMILIES: usize = 5;
const DETAIL_LIMIT: usize = 4096;
const JOB_FILE_KEEP: usize = 200;
/// Job timelines keep one archived part. A single family cannot rotate: the counter would reset
/// while the file keeps growing, so this value is enforced below and used by the caller.
const JOB_FAMILIES: usize = 2;
const _: () = assert!(JOB_FAMILIES >= 2, "job timelines must be able to rotate");
const RECENT_KEEP: usize = 200;
/// Day files older than this many days are deleted when a new day's files are opened.
const RETENTION_DAYS: i64 = 30;
/// Pause between attempts to open a day file that could not be opened.
const OPEN_RETRY: std::time::Duration = std::time::Duration::from_secs(30);

/// How much detail reaches the log. Chosen in Settings; read once per session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Off,
    Error,
    Normal,
    Detailed,
}

impl Level {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" | "kapali" | "kapalı" => Some(Self::Off),
            "error" | "hata" => Some(Self::Error),
            "normal" => Some(Self::Normal),
            "detailed" | "ayrintili" | "ayrıntılı" => Some(Self::Detailed),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Error => "error",
            Self::Normal => "normal",
            Self::Detailed => "detailed",
        }
    }

    const fn allows(self, level: EventLevel) -> bool {
        match self {
            Self::Off => false,
            Self::Error => matches!(level, EventLevel::Error),
            Self::Normal => matches!(
                level,
                EventLevel::Error | EventLevel::Warn | EventLevel::Info
            ),
            Self::Detailed => true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventLevel {
    /// Measurements and observation detail; only in the detailed level.
    Debug,
    Info,
    Warn,
    Error,
}

impl EventLevel {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
        }
    }
}

/// Event names are `area.action` in lower case; the extension supplies its own names, so the
/// bridge validates them against this rule before anything is recorded.
pub fn valid_event_name(name: &str) -> bool {
    let mut characters = name.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    name.len() <= 64
        && first.is_ascii_lowercase()
        && characters.all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || "._-".contains(character)
        })
}

/// Parses an inbound level name; anything unknown becomes `info`.
pub fn parse_level(value: &str) -> EventLevel {
    match value.trim().to_ascii_lowercase().as_str() {
        "debug" => EventLevel::Debug,
        "warn" | "warning" => EventLevel::Warn,
        "error" => EventLevel::Error,
        _ => EventLevel::Info,
    }
}

/// Parses an inbound outcome name.
pub fn parse_outcome(value: &str) -> Option<Outcome> {
    match value.trim().to_ascii_lowercase().as_str() {
        "ok" => Some(Outcome::Ok),
        "retry" => Some(Outcome::Retry),
        "partial" => Some(Outcome::Partial),
        "failed" => Some(Outcome::Failed),
        "skipped" => Some(Outcome::Skipped),
        _ => None,
    }
}

/// How the recorded operation ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Ok,
    Retry,
    Partial,
    Failed,
    Skipped,
}

impl Outcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Retry => "retry",
            Self::Partial => "partial",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }
}

/// One recorded event. Unset fields are omitted from the JSON line.
#[derive(Debug, Clone, Serialize)]
pub struct Event {
    /// Event name; static for internal callers, owned for events relayed from the extension.
    pub event: Cow<'static, str>,
    pub level: EventLevel,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<&'static str>,
    /// Who can resolve this: `app`, `site`, `drm` or `observation`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub area: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<Outcome>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl Event {
    pub fn new(event: impl Into<Cow<'static, str>>, level: EventLevel) -> Self {
        Self {
            event: event.into(),
            level,
            code: None,
            owner: None,
            area: None,
            outcome: None,
            host: None,
            job: None,
            ms: None,
            detail: None,
        }
    }

    /// An event relayed from the browser extension; the name was validated by the bridge.
    pub fn external(event: String, level: EventLevel) -> Self {
        Self::new(event, level)
    }

    pub fn debug(event: &'static str) -> Self {
        Self::new(event, EventLevel::Debug)
    }

    pub fn info(event: &'static str) -> Self {
        Self::new(event, EventLevel::Info)
    }

    pub fn warn(event: &'static str) -> Self {
        Self::new(event, EventLevel::Warn)
    }

    /// A failure event. The level follows the registry severity; the owner states whether the
    /// application, the site, rights management or the observation boundary is at fault.
    pub fn failure(event: &'static str, code: Code, detail: impl Into<String>) -> Self {
        let level = match error_codes::definition(code).map(|entry| entry.severity) {
            Some(Severity::Info) => EventLevel::Info,
            Some(Severity::Warn) => EventLevel::Warn,
            _ => EventLevel::Error,
        };
        Self::new(event, level)
            .code(code)
            .outcome(Outcome::Failed)
            .detail(detail)
    }

    pub fn code(mut self, code: Code) -> Self {
        self.code = Some(code.0);
        self.owner = Some(error_codes::owner(code).as_str());
        self.area = error_codes::definition(code).map(|entry| entry.area);
        self
    }

    pub fn outcome(mut self, outcome: Outcome) -> Self {
        self.outcome = Some(outcome);
        self
    }

    pub fn host(mut self, host: impl Into<String>) -> Self {
        let host = host.into();
        if !host.is_empty() {
            self.host = Some(host);
        }
        self
    }

    pub fn job(mut self, job: impl Into<String>) -> Self {
        let job = job.into();
        if !job.is_empty() {
            self.job = Some(job);
        }
        self
    }

    pub fn ms(mut self, ms: u64) -> Self {
        self.ms = Some(ms);
        self
    }

    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        let detail = sanitize(&detail.into());
        if !detail.is_empty() {
            self.detail = Some(detail);
        }
        self
    }
}

/// Session banner values; every session writes exactly one `app.session` line.
#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    pub channel: &'static str,
    pub version: &'static str,
}

#[derive(Debug, Clone, Copy)]
pub struct Config {
    pub level: Level,
    /// Records site host names. Switched off by the privacy setting.
    pub hosts: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            level: Level::Detailed,
            hosts: true,
        }
    }
}

struct Writer {
    /// Live configuration: the Settings dialog changes take effect without a restart.
    level: AtomicU8,
    hosts: AtomicBool,
    session: Session,
    lines: SyncSender<Line>,
    drops: AtomicU64,
    /// Where this session writes, so a reader can find a job's own timeline.
    logs: PathBuf,
}

enum Line {
    Event(Box<Event>),
    Job(String, Box<Event>),
}

static WRITER: OnceLock<Writer> = OnceLock::new();
/// Serializes `start`, so a second initialization cannot spawn a writer whose channel has no
/// reader (the CLI and the desktop can open the same data directory at the same time).
static START: std::sync::Mutex<()> = std::sync::Mutex::new(());
/// Newest rendered lines, for the desktop event window. Kept short and never used as the
/// durable record: the files under `logs/` are.
static RECENT: std::sync::LazyLock<std::sync::Mutex<std::collections::VecDeque<String>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::VecDeque::new()));

/// Starts the writer for this session. The first call wins; a later call from the CLI after the
/// desktop already opened the same data directory keeps the original session.
pub fn start(paths: &AppPaths, session: Session, config: Config) -> Result<()> {
    let _guard = START.lock().unwrap_or_else(|error| error.into_inner());
    if WRITER.get().is_some() {
        return Ok(());
    }
    let logs = paths.base_dir.join("logs");
    fs::create_dir_all(logs.join("jobs"))?;
    let (lines, receiver) = sync_channel(QUEUE_CAPACITY);
    let banner = Event::info("app.session").detail(format!(
        "channel={} version={} level={}",
        session.channel,
        session.version,
        config.level.as_str()
    ));
    let session_for_thread = session.clone();
    let writer_logs = logs.clone();
    thread::Builder::new()
        .name("event-log".into())
        .spawn(move || run(logs, session_for_thread, receiver))?;
    let _ = WRITER.set(Writer {
        level: AtomicU8::new(level_code(config.level)),
        hosts: AtomicBool::new(config.hosts),
        session,
        lines,
        drops: AtomicU64::new(0),
        logs: writer_logs,
    });
    record(banner);
    Ok(())
}

/// Records one event. Safe from any thread; never blocks and never fails.
pub fn record(event: Event) {
    let Some(writer) = WRITER.get() else {
        return;
    };
    let event = writer.prepare(event);
    if event.is_none() {
        return;
    }
    if let Some(event) = event {
        if writer.lines.try_send(Line::Event(Box::new(event))).is_err() {
            writer.drops.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Records one event on the session stream and on the job's own timeline.
pub fn record_job(job_id: &str, event: Event) {
    let Some(writer) = WRITER.get() else {
        return;
    };
    if let Some(session_event) = writer.prepare(event.clone()) {
        let mut tagged = session_event;
        tagged.job = Some(job_id.to_owned());
        if writer
            .lines
            .try_send(Line::Event(Box::new(tagged)))
            .is_err()
        {
            writer.drops.fetch_add(1, Ordering::Relaxed);
        }
    }
    if let Some(job_event) = writer.prepare(event) {
        if writer
            .lines
            .try_send(Line::Job(job_id.to_owned(), Box::new(job_event)))
            .is_err()
        {
            writer.drops.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// The log session of the browser's native host. That process starts once per browser message and
/// ends right after, so it writes from the calling thread (nothing waits in a queue when the
/// process exits) and opens its session with the first event it records: a host that only relays
/// traffic leaves no session banner. It appends to the day files of the desktop session, which
/// open them shared; the `native-host` channel tells the two apart.
pub struct HostLog {
    database: PathBuf,
    sink: Sink,
    session: Session,
    /// Read from the profile settings when the first event arrives.
    config: Option<Config>,
}

impl HostLog {
    pub fn new(paths: &AppPaths) -> Self {
        Self {
            database: paths.database.clone(),
            sink: Sink::new(paths.base_dir.join("logs")),
            session: Session {
                id: uuid::Uuid::new_v4().to_string()[..8].to_owned(),
                channel: "native-host",
                version: env!("CARGO_PKG_VERSION"),
            },
            config: None,
        }
    }

    pub fn record(&mut self, event: Event) {
        let config = match self.config {
            Some(config) => config,
            None => {
                let config = crate::store::Store::stored_settings(&self.database)
                    .map(|settings| Config {
                        level: Level::parse(&settings.logging_level).unwrap_or(Level::Detailed),
                        hosts: settings.logging_hosts,
                    })
                    .unwrap_or_default();
                self.config = Some(config);
                let banner = Event::info("app.session").detail(format!(
                    "channel={} version={} level={}",
                    self.session.channel,
                    self.session.version,
                    config.level.as_str()
                ));
                self.write(config, banner);
                config
            }
        };
        self.write(config, event);
    }

    fn write(&mut self, config: Config, event: Event) {
        if let Some(event) = filtered(config.level, config.hosts, event) {
            self.sink.event_line(&self.session, &event);
        }
    }
}

impl Writer {
    fn level(&self) -> Level {
        level_from_code(self.level.load(Ordering::Relaxed))
    }

    fn hosts(&self) -> bool {
        self.hosts.load(Ordering::Relaxed)
    }

    /// Applies the level filter and the host privacy switch as configured right now.
    fn prepare(&self, event: Event) -> Option<Event> {
        filtered(self.level(), self.hosts(), event)
    }
}

fn filtered(level: Level, hosts: bool, event: Event) -> Option<Event> {
    if !level.allows(event.level) {
        return None;
    }
    if hosts {
        return Some(event);
    }
    let mut event = event;
    let own_host = event.host.take();
    event.detail = event
        .detail
        .map(|detail| hide_hosts(&detail, own_host.as_deref()));
    Some(event)
}

const HIDDEN_HOST: &str = "host-hidden.invalid";

/// Replaces the host of every address in an already sanitized detail line, and the host names
/// that stand alone, so the privacy switch that stops host recording does not leave the site
/// name behind inside a URL or an error message.
fn hide_hosts(detail: &str, own_host: Option<&str>) -> String {
    let addresses = detail
        .split(' ')
        .map(hide_word_host)
        .collect::<Vec<_>>()
        .join(" ");
    hide_bare_hosts(&addresses, own_host)
}

/// Hides the names that appear outside an address: the event's own host, which curl and the
/// media tools repeat in their messages (also as a subdomain or before a port), and a dotted
/// name after `host:`, the way curl reports `Could not resolve host: <name>`.
fn hide_bare_hosts(detail: &str, own_host: Option<&str>) -> String {
    let own_host = own_host
        .filter(|host| !host.is_empty())
        .map(str::to_ascii_lowercase);
    let mut hidden = String::with_capacity(detail.len());
    let mut copied = 0;
    let mut name_start = None;
    // The sentinel ends a name that runs to the end of the line.
    for (index, character) in detail.char_indices().chain([(detail.len(), ' ')]) {
        let in_name = character.is_ascii_alphanumeric() || character == '-' || character == '.';
        match (in_name, name_start) {
            (true, None) => name_start = Some(index),
            (false, Some(start)) => {
                name_start = None;
                let name = detail[start..index].trim_end_matches('.');
                if is_host_name(name, own_host.as_deref(), &detail[..start]) {
                    hidden.push_str(&detail[copied..start]);
                    hidden.push_str(HIDDEN_HOST);
                    copied = start + name.len();
                }
            }
            _ => {}
        }
    }
    hidden.push_str(&detail[copied..]);
    hidden
}

fn is_host_name(name: &str, own_host: Option<&str>, before: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    own_host.is_some_and(|host| {
        lower == host
            || lower
                .strip_suffix(host)
                .is_some_and(|rest| rest.ends_with('.'))
    }) || (name.contains('.') && follows_host_label(before))
}

/// Whether `before` ends with the word `host:`.
fn follows_host_label(before: &str) -> bool {
    let before = before.trim_end();
    let Some(label_start) = before.len().checked_sub("host:".len()) else {
        return false;
    };
    before
        .get(label_start..)
        .is_some_and(|label| label.eq_ignore_ascii_case("host:"))
        && before[..label_start]
            .chars()
            .next_back()
            .is_none_or(|character| !character.is_alphanumeric())
}

fn hide_word_host(word: &str) -> String {
    for index in std::iter::once(0).chain(embedded_url_start(word)) {
        let (head, tail) = word.split_at(index);
        let Ok(mut url) = url::Url::parse(tail.trim_matches(WORD_TRIM)) else {
            continue;
        };
        if matches!(url.scheme(), "http" | "https" | "ftp" | "ftps")
            && url.host().is_some()
            && url.set_host(Some(HIDDEN_HOST)).is_ok()
        {
            return format!("{head}{url}");
        }
    }
    word.to_owned()
}

const fn level_code(level: Level) -> u8 {
    match level {
        Level::Off => 0,
        Level::Error => 1,
        Level::Normal => 2,
        Level::Detailed => 3,
    }
}

const fn level_from_code(code: u8) -> Level {
    match code {
        1 => Level::Error,
        2 => Level::Normal,
        3 => Level::Detailed,
        _ => Level::Off,
    }
}

/// Newest rendered lines (oldest first) for the desktop event window.
pub fn recent(limit: usize) -> Vec<String> {
    let ring = RECENT.lock().unwrap_or_else(|error| error.into_inner());
    let skip = ring.len().saturating_sub(limit);
    ring.iter().skip(skip).cloned().collect()
}

/// One job's recorded timeline, oldest first: the newest `limit` lines of its own
/// `logs/jobs/job-<id>.jsonl`. The desktop shows exactly this for a single download.
pub fn job_timeline(job_id: &str, limit: usize) -> Vec<String> {
    let Some(writer) = WRITER.get() else {
        return Vec::new();
    };
    read_job_timeline(&writer.logs.join("jobs"), job_id, limit)
}

/// Reads one job timeline from `dir`. Rotated content is read after the live file, so the newest
/// lines win when `limit` truncates the history.
pub(crate) fn read_job_timeline(dir: &Path, job_id: &str, limit: usize) -> Vec<String> {
    if limit == 0 {
        return Vec::new();
    }
    let name = safe_name(job_id, 64);
    let mut chosen: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    for file in [
        dir.join(format!("job-{name}.jsonl")),
        dir.join(format!("job-{name}.1.jsonl")),
    ] {
        if chosen.len() >= limit {
            break;
        }
        let Ok(text) = fs::read_to_string(&file) else {
            continue;
        };
        for line in text.lines().rev() {
            if chosen.len() == limit {
                break;
            }
            if let Some(rendered) = stored_line(line) {
                chosen.push_front(rendered);
            }
        }
    }
    chosen.into_iter().collect()
}

/// Renders a stored JSON line the way the live writer renders the human log. Mirrors
/// `human_detail`: a stored line is JSON, so the same fields are read here.
fn stored_line(json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let stamp = value.get("ts")?.as_str()?;
    let stamp = chrono::DateTime::parse_from_rfc3339(stamp)
        .ok()?
        .with_timezone(&chrono::Local);
    let level = value
        .get("level")
        .and_then(|value| value.as_str())
        .unwrap_or("info")
        .to_uppercase();
    let event = value.get("event").and_then(|value| value.as_str())?;
    let mut parts: Vec<String> = Vec::new();
    for (key, prefix) in [
        ("code", ""),
        ("owner", "owner="),
        ("outcome", ""),
        ("host", "site="),
        ("job", "job="),
    ] {
        if let Some(part) = value.get(key).and_then(|value| value.as_str()) {
            parts.push(format!("{prefix}{part}"));
        }
    }
    if let Some(ms) = value.get("ms").and_then(|value| value.as_u64()) {
        parts.push(format!("{ms}ms"));
    }
    if let Some(detail) = value.get("detail").and_then(|value| value.as_str()) {
        parts.push(detail.to_owned());
    }
    Some(format!(
        "{} {:<5} {} | {} | {}",
        stamp.format("%Y-%m-%d %H:%M:%S"),
        level,
        value
            .get("channel")
            .and_then(|value| value.as_str())
            .unwrap_or("job"),
        event,
        parts.join(" | ")
    ))
}

/// The active session id, when logging started.
pub fn session_id() -> Option<&'static str> {
    WRITER.get().map(|writer| writer.session.id.as_str())
}

/// The configured level right now, when logging started.
pub fn level() -> Option<Level> {
    WRITER.get().map(|writer| writer.level())
}

/// Applies a Settings change to the running session log without a restart. The level filter and
/// the host switch take effect for the next recorded event.
pub fn reconfigure(level: Level, hosts: bool) {
    let Some(writer) = WRITER.get() else {
        return;
    };
    let previous = writer.level();
    let previous_hosts = writer.hosts();
    writer.level.store(level_code(level), Ordering::Relaxed);
    writer.hosts.store(hosts, Ordering::Relaxed);
    if previous != level || previous_hosts != hosts {
        record(
            Event::info("log.reconfigured")
                .detail(format!("level={} hosts={hosts}", level.as_str())),
        );
    }
}

/// Events dropped because the writer queue was full.
pub fn dropped_events() -> u64 {
    WRITER
        .get()
        .map(|writer| writer.drops.load(Ordering::Relaxed))
        .unwrap_or(0)
}

/// Lines that could not be written to their log file. Counted directly here: the failure
/// path must not re-enter the logger it is reporting on.
static WRITE_FAILURES: AtomicU64 = AtomicU64::new(0);

fn write_failures() -> &'static AtomicU64 {
    &WRITE_FAILURES
}

pub fn write_failure_count() -> u64 {
    WRITE_FAILURES.load(Ordering::Relaxed)
}

/// Redacts text the way the desktop message list does: credential-free, query-free URLs, one
/// line, length-bounded.
pub fn sanitize(text: &str) -> String {
    let collapsed: String = text
        .chars()
        .map(|character| {
            if character == '\n' || character == '\r' {
                ' '
            } else {
                character
            }
        })
        .collect();
    collapsed
        .split_whitespace()
        .map(redact_word)
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(DETAIL_LIMIT)
        .collect()
}

const WORD_TRIM: [char; 6] = ['\'', '"', '(', ')', ',', ';'];

/// The address a word names when it is a whole http(s)/ftp(s) URL: user information and
/// fragment removed, query replaced.
fn redact_url(text: &str) -> Option<String> {
    let mut url = url::Url::parse(text).ok()?;
    if !matches!(url.scheme(), "http" | "https" | "ftp" | "ftps") {
        return None;
    }
    let _ = url.set_username("");
    let _ = url.set_password(None);
    if url.query().is_some() {
        url.set_query(Some("REDACTED"));
    }
    url.set_fragment(None);
    Some(url.to_string())
}

/// Byte offset of a URL that starts inside the word (`url=https://...`, `[https://...]`,
/// `Referer:https://...`), past the first byte.
fn embedded_url_start(word: &str) -> Option<usize> {
    let lower = word.to_ascii_lowercase();
    ["http://", "https://", "ftp://", "ftps://"]
        .iter()
        .filter_map(|scheme| {
            lower
                .match_indices(scheme)
                .map(|(index, _)| index)
                .find(|index| *index > 0)
        })
        .min()
}

fn redact_word(word: &str) -> String {
    if let Some(redacted) = redact_url(word.trim_matches(WORD_TRIM)) {
        return redacted;
    }
    if let Some(index) = embedded_url_start(word) {
        let (head, tail) = word.split_at(index);
        if let Some(redacted) = redact_url(tail.trim_matches(WORD_TRIM)) {
            return format!("{head}{redacted}");
        }
    }
    // A long opaque token is treated as a credential and never written.
    if word.len() > 64
        && word
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_.".contains(character))
    {
        return "REDACTED".into();
    }
    word.to_owned()
}

/// Host name of an address, for events that record which site was involved.
pub fn host_of(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()?
        .host_str()
        .map(|host| host.to_ascii_lowercase())
}

/// User-visible file name of a local path; never the full path.
pub fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Shortens an identifier used in a file name.
fn safe_name(value: &str, limit: usize) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '_'
            }
        })
        .take(limit)
        .collect()
}

fn run(logs: PathBuf, session: Session, lines: Receiver<Line>) {
    let mut sink = Sink::new(logs);
    while let Ok(line) = lines.recv() {
        sink.report_drops(&session);
        sink.report_write_failures(&session);
        match line {
            Line::Event(event) => sink.event_line(&session, &event),
            Line::Job(job, event) => sink.job_line(&job, &event),
        }
    }
    sink.report_drops(&session);
    sink.report_write_failures(&session);
}

struct Sink {
    logs: PathBuf,
    day: String,
    events: Option<Rotating>,
    human: Option<Rotating>,
    jobs: HashMap<String, Rotating>,
    reported_drops: u64,
    reported_write_failures: u64,
    /// Earliest time a stream that failed to open is tried again.
    retry_at: Instant,
}

impl Sink {
    fn new(logs: PathBuf) -> Self {
        Self {
            logs,
            day: String::new(),
            events: None,
            human: None,
            jobs: HashMap::new(),
            reported_drops: 0,
            reported_write_failures: 0,
            retry_at: Instant::now(),
        }
    }

    /// Opens the stream files, following the calendar day. A stream that could not be opened
    /// is tried again (at most every `OPEN_RETRY`), so one locked or unwritable moment does not
    /// silence the rest of the day; every failed attempt counts as a write failure.
    fn roll_day(&mut self, day: &str) {
        let new_day = self.day != day;
        if !new_day && self.events.is_some() && self.human.is_some() {
            return;
        }
        if !new_day && Instant::now() < self.retry_at {
            return;
        }
        if new_day {
            self.day = day.to_owned();
            self.events = None;
            self.human = None;
            self.jobs.clear();
            if let Ok(today) = chrono::NaiveDate::parse_from_str(day, "%Y%m%d") {
                prune_day_files(&self.logs, today, RETENTION_DAYS);
            }
        }
        if self.events.is_none() {
            self.events = Rotating::open(
                &self.logs,
                &format!("events-{day}"),
                "jsonl",
                FILE_LIMIT,
                FILE_FAMILIES,
            );
        }
        if self.human.is_none() {
            self.human = Rotating::open(
                &self.logs,
                &format!("ssdownload-{day}"),
                "log",
                FILE_LIMIT,
                FILE_FAMILIES,
            );
        }
        if self.events.is_none() || self.human.is_none() {
            write_failures().fetch_add(1, Ordering::Relaxed);
            self.retry_at = Instant::now() + OPEN_RETRY;
        }
    }

    fn report_drops(&mut self, session: &Session) {
        if level() == Some(Level::Off) {
            return;
        }
        let total = dropped_events();
        if total == self.reported_drops {
            return;
        }
        let delta = total - self.reported_drops;
        self.reported_drops = total;
        let event = Event::warn("log.dropped").detail(crate::i18n::ui_owned!(
            format!("{delta} olay kuyruk dolduğu için yazılamadı"),
            format!("{delta} events were not written because the queue was full")
        ));
        self.event_line(session, &event);
    }

    fn report_write_failures(&mut self, session: &Session) {
        if level() == Some(Level::Off) {
            return;
        }
        let total = write_failure_count();
        if total == self.reported_write_failures {
            return;
        }
        let delta = total - self.reported_write_failures;
        self.reported_write_failures = total;
        let event = Event::warn("log.write_failed").detail(crate::i18n::ui_owned!(
            format!("{delta} günlük satırı diske yazılamadı"),
            format!("{delta} log lines could not be written to disk")
        ));
        self.event_line(session, &event);
    }

    fn event_line(&mut self, session: &Session, event: &Event) {
        let stamp = chrono::Local::now();
        self.roll_day(&stamp.format("%Y%m%d").to_string());
        let line = json_line(stamp, &session.id, session.channel, session.version, event);
        if let Some(file) = &mut self.events {
            file.write_line(&line);
        }
        let human = format!(
            "{} {:<5} {} | {} | {}",
            stamp.format("%Y-%m-%d %H:%M:%S"),
            event.level.as_str(),
            session.channel,
            event.event,
            human_detail(event)
        );
        if let Some(file) = &mut self.human {
            file.write_line(&human);
        }
        {
            let mut ring = RECENT.lock().unwrap_or_else(|error| error.into_inner());
            if ring.len() >= RECENT_KEEP {
                ring.pop_front();
            }
            ring.push_back(human);
        }
    }

    fn job_line(&mut self, job: &str, event: &Event) {
        let stamp = chrono::Local::now();
        let name = safe_name(job, 64);
        let dir = self.logs.join("jobs");
        if !self.jobs.contains_key(&name) {
            // Two families, so a long-running job's timeline really rotates instead of
            // resetting a counter on a file that keeps growing.
            let file = Rotating::open(
                &dir,
                &format!("job-{name}"),
                "jsonl",
                FILE_LIMIT,
                JOB_FAMILIES,
            );
            if let Some(file) = file {
                self.jobs.insert(name.clone(), file);
            }
            prune_job_files(&dir);
        }
        if let Some(file) = self.jobs.get_mut(&name) {
            file.write_line(&json_line(
                stamp,
                job,
                "job",
                env!("CARGO_PKG_VERSION"),
                event,
            ));
        }
    }
}

fn json_line(
    stamp: chrono::DateTime<chrono::Local>,
    session: &str,
    channel: &str,
    version: &str,
    event: &Event,
) -> String {
    #[derive(Serialize)]
    struct Envelope<'a> {
        ts: String,
        session: &'a str,
        channel: &'a str,
        version: &'a str,
        #[serde(flatten)]
        event: &'a Event,
    }
    let envelope = Envelope {
        ts: stamp.to_rfc3339_opts(chrono::SecondsFormat::Millis, false),
        session,
        channel,
        version,
        event,
    };
    serde_json::to_string(&envelope).unwrap_or_else(|_| {
        format!(
            "{{\"ts\":\"{}\",\"event\":\"log.serialize_failed\"}}",
            stamp.to_rfc3339_opts(chrono::SecondsFormat::Millis, false)
        )
    })
}

fn human_detail(event: &Event) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(code) = event.code {
        parts.push(code.to_owned());
    }
    if let Some(owner) = event.owner {
        parts.push(format!("owner={owner}"));
    }
    if let Some(outcome) = event.outcome {
        parts.push(outcome.as_str().to_owned());
    }
    if let Some(host) = &event.host {
        parts.push(format!("site={host}"));
    }
    if let Some(job) = &event.job {
        parts.push(format!("job={job}"));
    }
    if let Some(ms) = event.ms {
        parts.push(format!("{ms}ms"));
    }
    if let Some(detail) = &event.detail {
        parts.push(detail.clone());
    }
    parts.join(" | ")
}

/// Size- and count-bounded append target: `stem.log`, `stem.1.log`, ... keeping the newest
/// `families` files. A new file starts once the current one passes `limit`; `families` must be
/// at least two for any rotation to happen, which `rotate` relies on.
struct Rotating {
    dir: PathBuf,
    stem: String,
    extension: &'static str,
    limit: u64,
    families: usize,
    file: File,
    path: PathBuf,
    written: u64,
}

impl Rotating {
    fn open(
        dir: &Path,
        stem: &str,
        extension: &'static str,
        limit: u64,
        families: usize,
    ) -> Option<Self> {
        fs::create_dir_all(dir).ok()?;
        let path = dir.join(format!("{stem}.{extension}"));
        let mut written = fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
        if written > limit {
            let archived = dir.join(format!("{stem}.1.{extension}"));
            fs::remove_file(&archived).ok();
            if fs::rename(&path, &archived).is_ok() {
                written = 0;
            } else {
                // The file cannot be moved (open elsewhere); keep appending to it.
            }
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok()?;
        Some(Self {
            dir: dir.to_path_buf(),
            stem: stem.to_owned(),
            extension,
            limit,
            families: families.max(1),
            file,
            path,
            written,
        })
    }

    fn write_line(&mut self, line: &str) {
        if self.written > self.limit {
            self.rotate();
        }
        let mut bytes = line.as_bytes().to_vec();
        bytes.push(b'\n');
        if self.file.write_all(&bytes).is_ok() {
            self.written += bytes.len() as u64;
        } else {
            write_failures().fetch_add(1, Ordering::Relaxed);
        }
    }

    fn rotate(&mut self) {
        for index in (1..self.families).rev() {
            let from = if index == 1 {
                self.path.clone()
            } else {
                self.dir
                    .join(format!("{}.{}.{}", self.stem, index - 1, self.extension))
            };
            let to = self
                .dir
                .join(format!("{}.{}.{}", self.stem, index, self.extension));
            if index == self.families - 1 {
                fs::remove_file(&to).ok();
            }
            fs::rename(from, to).ok();
        }
        if let Ok(file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            self.file = file;
            self.written = 0;
        }
    }
}

/// Removes the per-day session files (`events-YYYYMMDD[.N].jsonl`, `ssdownload-YYYYMMDD[.N].log`)
/// whose day is more than `keep_days` before `today`. Other files in the directory, including
/// the `jobs` folder, are never touched.
fn prune_day_files(dir: &Path, today: chrono::NaiveDate, keep_days: i64) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let cutoff = today - chrono::Duration::days(keep_days);
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(rest) = name
            .strip_prefix("events-")
            .filter(|_| name.ends_with(".jsonl"))
            .or_else(|| {
                name.strip_prefix("ssdownload-")
                    .filter(|_| name.ends_with(".log"))
            })
        else {
            continue;
        };
        let Some(digits) = rest.get(..8) else {
            continue;
        };
        let Ok(day) = chrono::NaiveDate::parse_from_str(digits, "%Y%m%d") else {
            continue;
        };
        if day < cutoff && entry.file_type().is_ok_and(|kind| kind.is_file()) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn prune_job_files(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, path))
        })
        .collect();
    if files.len() <= JOB_FILE_KEEP {
        return;
    }
    files.sort_by_key(|(modified, _)| *modified);
    let remove = files.len() - JOB_FILE_KEEP;
    for (_, path) in files.iter().take(remove) {
        fs::remove_file(path).ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("ssdownload-log-{name}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_job_timeline_renders_only_that_job_and_keeps_the_newest_lines() {
        let dir = temp_dir("job-timeline");
        fs::write(
            dir.join("job-abc.1.jsonl"),
            "{\"ts\":\"2026-09-11T05:30:36.972+03:00\",\"session\":\"abc\",\"channel\":\"job\",\"version\":\"1.4.13\",\"event\":\"job.add\",\"level\":\"info\",\"outcome\":\"ok\",\"host\":\"example.test\",\"detail\":\"kind=video height=800\"}\n",
        )
        .unwrap();
        fs::write(
            dir.join("job-abc.jsonl"),
            [
                "{\"ts\":\"2026-09-11T05:30:42.004+03:00\",\"session\":\"abc\",\"channel\":\"job\",\"version\":\"1.4.13\",\"event\":\"job.fail\",\"level\":\"error\",\"code\":\"SSD-MED-015\",\"owner\":\"app\",\"outcome\":\"failed\",\"host\":\"example.test\",\"ms\":5100,\"detail\":\"Seçilen kalite indirme anında kaynakta bulunamadı\"}",
                "not json at all",
                "",
            ]
            .join("\n"),
        )
        .unwrap();
        fs::write(dir.join("job-other.jsonl"), "").unwrap();

        let timeline = read_job_timeline(&dir, "abc", 10);
        assert_eq!(timeline.len(), 2, "{timeline:?}");
        assert!(
            timeline[0].contains("INFO  job | job.add"),
            "{}",
            timeline[0]
        );
        assert!(timeline[0].contains("site=example.test"), "{}", timeline[0]);
        let failure = &timeline[1];
        assert!(failure.contains("ERROR job | job.fail"), "{failure}");
        assert!(failure.contains("SSD-MED-015"), "{failure}");
        assert!(failure.contains("owner=app"), "{failure}");
        assert!(failure.contains("failed"), "{failure}");
        assert!(failure.contains("5100ms"), "{failure}");

        // The newest line wins when the limit truncates, and another job is never mixed in.
        let newest_only = read_job_timeline(&dir, "abc", 1);
        assert_eq!(newest_only.len(), 1);
        assert!(newest_only[0].contains("job.fail"), "{}", newest_only[0]);
        assert!(read_job_timeline(&dir, "missing", 10).is_empty());
        assert!(read_job_timeline(&dir, "abc", 0).is_empty());

        fs::remove_dir_all(dir).unwrap();
    }

    fn session() -> Session {
        Session {
            id: "session".into(),
            channel: "app",
            version: "1.0.0",
        }
    }

    #[test]
    fn urls_reach_the_log_without_credentials_query_or_fragment() {
        let sanitized = sanitize(
            "indirme https://user:secret@cdn.example.com/film.mp4?token=abc#part başarısız 'https://site.example/watch?v=1'",
        );
        assert!(!sanitized.contains("secret"), "{sanitized}");
        assert!(!sanitized.contains("token=abc"), "{sanitized}");
        assert!(!sanitized.contains("#part"), "{sanitized}");
        assert!(
            sanitized.contains("https://cdn.example.com/film.mp4?REDACTED"),
            "{sanitized}"
        );
    }

    #[test]
    fn urls_embedded_in_a_word_lose_their_query_and_credentials() {
        let sanitized = sanitize(
            "url=https://u:pw@cdn.example.com/a.mp4?sig=SECRET1 [https://cdn.example.com/b.mp4?sig=SECRET2] Referer:https://site.example/p?t=SECRET3#x {\"url\":\"https://site.example/q?k=SECRET4\"}",
        );
        for secret in ["SECRET1", "SECRET2", "SECRET3", "SECRET4", "pw@", "#x"] {
            assert!(!sanitized.contains(secret), "{secret} leaked: {sanitized}");
        }
        assert!(
            sanitized.contains("url=https://cdn.example.com/a.mp4?REDACTED"),
            "{sanitized}"
        );
        assert!(
            sanitized.contains("Referer:https://site.example/p?REDACTED"),
            "{sanitized}"
        );
        assert_eq!(sanitize("Çıktı yolu: dosya.mp4"), "Çıktı yolu: dosya.mp4");
    }

    #[test]
    fn newlines_are_collapsed_and_long_credential_tokens_are_dropped() {
        let token = "A".repeat(80);
        let sanitized = sanitize(&format!("satır1\nsatır2 yetki {token} reddedildi"));
        assert!(!sanitized.contains('\n'));
        assert!(sanitized.contains("satır1 satır2"));
        assert!(sanitized.contains("REDACTED"), "{sanitized}");
        assert!(!sanitized.contains(&token));
    }

    #[test]
    fn a_failure_event_carries_its_code_level_owner_and_area() {
        let event = Event::failure("inspect.blocked", error_codes::MED_007, "403 Cloudflare")
            .host("site.example");
        assert_eq!(event.code, Some("SSD-MED-007"));
        assert_eq!(event.owner, Some("site"));
        assert_eq!(event.area, Some("media"));
        assert_eq!(event.level, EventLevel::Error);
        let line = json_line(chrono::Local::now(), &session().id, "app", "1.0.0", &event);
        assert!(line.contains("\"code\":\"SSD-MED-007\""), "{line}");
        assert!(line.contains("\"owner\":\"site\""), "{line}");
        assert!(line.contains("\"outcome\":\"failed\""), "{line}");
        assert!(line.contains("\"event\":\"inspect.blocked\""), "{line}");
    }

    #[test]
    fn drm_and_observation_owners_are_reported_as_such() {
        assert_eq!(
            Event::failure("inspect.drm", error_codes::MED_003, "widevine").owner,
            Some("drm")
        );
        assert_eq!(
            Event::failure(
                "ext.observe.limit",
                error_codes::EXT_007,
                "kapalı gölge kök"
            )
            .owner,
            Some("observation")
        );
    }

    #[test]
    fn a_running_writer_follows_a_settings_change() {
        let (lines, _receiver) = sync_channel(4);
        let writer = Writer {
            level: AtomicU8::new(level_code(Level::Detailed)),
            hosts: AtomicBool::new(true),
            session: Session {
                id: "test".into(),
                channel: "app",
                version: "1.0.0",
            },
            lines,
            drops: AtomicU64::new(0),
            logs: PathBuf::from("logs"),
        };
        let event = || Event::info("job.stage").host("site.example");
        assert!(writer.prepare(event()).is_some());

        writer
            .level
            .store(level_code(Level::Error), Ordering::Relaxed);
        assert!(
            writer.prepare(event()).is_none(),
            "INFO must be filtered at the error level"
        );

        writer
            .level
            .store(level_code(Level::Normal), Ordering::Relaxed);
        writer.hosts.store(false, Ordering::Relaxed);
        let prepared = writer
            .prepare(event())
            .expect("INFO passes at the normal level");
        assert_eq!(
            prepared.host, None,
            "the host switch must strip the site name"
        );
    }

    #[test]
    fn switching_host_recording_off_also_hides_hosts_inside_detail_urls() {
        let (lines, _receiver) = sync_channel(4);
        let writer = Writer {
            level: AtomicU8::new(level_code(Level::Detailed)),
            hosts: AtomicBool::new(false),
            session: session(),
            lines,
            drops: AtomicU64::new(0),
            logs: PathBuf::from("logs"),
        };
        let event = Event::info("job.stage").host("site.example").detail(
            "GET https://cdn.site.example:8443/a/b.mp4?token=x failed url=http://other.example/p",
        );
        let prepared = writer.prepare(event).expect("INFO passes");
        assert_eq!(prepared.host, None);
        let detail = prepared.detail.expect("detail kept");
        assert!(!detail.contains("site.example"), "{detail}");
        assert!(!detail.contains("other.example"), "{detail}");
        assert!(
            detail.contains("https://host-hidden.invalid:8443/a/b.mp4?REDACTED"),
            "{detail}"
        );
        assert!(detail.contains("failed"), "{detail}");

        writer.hosts.store(true, Ordering::Relaxed);
        let kept = writer
            .prepare(Event::info("job.stage").detail("https://cdn.site.example/a"))
            .unwrap();
        assert_eq!(
            kept.detail.as_deref(),
            Some("https://cdn.site.example/a"),
            "hosts stay when recording is on"
        );
    }

    #[test]
    fn day_files_older_than_the_retention_window_are_removed_and_nothing_else() {
        let dir = temp_dir("retention");
        fs::create_dir_all(dir.join("jobs")).unwrap();
        let old = [
            "events-20260101.jsonl",
            "events-20260101.1.jsonl",
            "ssdownload-20260101.log",
            "ssdownload-20260101.4.log",
        ];
        let kept = [
            "events-20260915.jsonl",
            "ssdownload-20260915.log",
            "events-20261014.jsonl",
            "events-probe.jsonl",
            "ssdownload.log",
            "events-20260101.txt",
            "notes.txt",
        ];
        for name in old.iter().chain(&kept) {
            fs::write(dir.join(name), "x").unwrap();
        }
        let today = chrono::NaiveDate::from_ymd_opt(2026, 10, 14).unwrap();
        prune_day_files(&dir, today, 30);
        for name in old {
            assert!(!dir.join(name).exists(), "{name} is past the window");
        }
        for name in kept {
            assert!(dir.join(name).exists(), "{name} must stay");
        }
        assert!(dir.join("jobs").is_dir());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_day_file_that_failed_to_open_is_retried_and_counted() {
        let dir = temp_dir("retry");
        // A directory in place of the file makes the open fail.
        fs::create_dir_all(dir.join("events-20260101.jsonl")).unwrap();
        let mut sink = Sink::new(dir.clone());
        let before = write_failure_count();
        sink.roll_day("20260101");
        assert!(sink.events.is_none() && sink.human.is_some());
        assert!(
            write_failure_count() > before,
            "a failed open is a counted write failure"
        );

        // Inside the pause nothing is retried.
        fs::remove_dir(dir.join("events-20260101.jsonl")).unwrap();
        sink.roll_day("20260101");
        assert!(sink.events.is_none(), "the retry waits for its pause");

        sink.retry_at = Instant::now();
        sink.roll_day("20260101");
        assert!(
            sink.events.is_some(),
            "the rest of the day is not lost once the file can be opened"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn level_filter_keeps_errors_but_drops_detail() {
        assert!(!Level::Off.allows(EventLevel::Error));
        assert!(Level::Error.allows(EventLevel::Error));
        assert!(!Level::Error.allows(EventLevel::Info));
        assert!(Level::Normal.allows(EventLevel::Warn));
        assert!(!Level::Normal.allows(EventLevel::Debug));
        assert!(Level::Detailed.allows(EventLevel::Debug));
        assert_eq!(Level::parse("ayrıntılı"), Some(Level::Detailed));
        assert_eq!(Level::parse("kapalı"), Some(Level::Off));
        assert_eq!(Level::parse("kalıcı"), None);
    }

    #[test]
    fn rotation_keeps_the_newest_files_within_the_limit() {
        let dir = temp_dir("rotate");
        let mut rotating =
            Rotating::open(&dir, "events-20260101", "jsonl", 64, 3).expect("günlük açılamadı");
        for index in 0..40 {
            rotating.write_line(&format!("{{\"n\":{index:03}}}"));
        }
        let files: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(files.len() <= 3, "{files:?}");
        assert!(
            files.iter().any(|name| name == "events-20260101.jsonl"),
            "{files:?}"
        );
        for name in &files {
            let size = fs::metadata(dir.join(name)).unwrap().len();
            assert!(size <= 96, "{name} kept {size} bytes");
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_job_timeline_rotates_instead_of_growing_without_bound() {
        let dir = temp_dir("job-rotate");
        let limit = 256;
        // Uses the same family count as `Sink::job_line`; the const assertion above rejects a
        // value that cannot rotate.
        let mut rotating = Rotating::open(&dir, "job-abc", "jsonl", limit, JOB_FAMILIES)
            .expect("zaman çizelgesi açılamadı");
        for index in 0..80 {
            rotating.write_line(&format!("{{\"step\":{index:03}}}"));
        }
        let archived = dir.join("job-abc.1.jsonl");
        assert!(archived.is_file(), "the previous part must be archived");
        for name in ["job-abc.jsonl", "job-abc.1.jsonl"] {
            let size = fs::metadata(dir.join(name)).unwrap().len();
            assert!(size <= limit * 2, "{name} kept {size} bytes");
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn job_timelines_are_kept_per_job_and_bounded() {
        let dir = temp_dir("jobs");
        let jobs = dir.join("jobs");
        fs::create_dir_all(&jobs).unwrap();
        for index in 0..(JOB_FILE_KEEP + 5) {
            let mut rotating =
                Rotating::open(&jobs, &format!("job-{index}"), "jsonl", FILE_LIMIT, 1).unwrap();
            rotating.write_line("{\"event\":\"job.stage\"}");
            prune_job_files(&jobs);
        }
        let kept = fs::read_dir(&jobs).unwrap().count();
        assert!(kept <= JOB_FILE_KEEP, "kept {kept}");
        assert!(kept > 10, "kept {kept}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bare_host_names_in_free_text_are_hidden_with_the_site_switch_off() {
        let hide = |host: &str, detail: &str| {
            filtered(
                Level::Detailed,
                false,
                Event::info("job.fail").host(host).detail(detail),
            )
            .and_then(|event| event.detail)
            .expect("the detail is kept")
        };
        assert_eq!(
            hide(
                "files.example.com",
                "Could not resolve host: files.example.com"
            ),
            "Could not resolve host: host-hidden.invalid"
        );
        // Wherever the message puts the job's own host: quoted, in other case, before a port or
        // a full stop, or as the parent of a subdomain.
        assert_eq!(
            hide(
                "files.example.com",
                "connect 'Files.Example.com':443 via cdn.files.example.com."
            ),
            "connect 'host-hidden.invalid':443 via host-hidden.invalid."
        );
        // A redirect can name another host than the job's; curl's own wording gives it away.
        assert_eq!(
            hide("site.example", "Could not resolve host: cdn.other.example"),
            "Could not resolve host: host-hidden.invalid"
        );
        // Text that only looks similar stays readable.
        let unrelated = "ghost: not.a.host; host: refused; saved site.example.org.bak";
        assert_eq!(hide("site.example", unrelated), unrelated);

        let kept = filtered(
            Level::Detailed,
            true,
            Event::info("job.fail")
                .host("files.example.com")
                .detail("Could not resolve host: files.example.com"),
        )
        .expect("the event passes");
        assert_eq!(
            kept.detail.as_deref(),
            Some("Could not resolve host: files.example.com"),
            "names stay when recording is on"
        );
    }

    fn read_events(root: &Path) -> Vec<serde_json::Value> {
        let mut values = Vec::new();
        let Ok(entries) = fs::read_dir(root.join("logs")) else {
            return values;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("events-") && name.ends_with(".jsonl") {
                for line in fs::read_to_string(entry.path()).unwrap().lines() {
                    values.push(serde_json::from_str(line).expect("each line is one JSON event"));
                }
            }
        }
        values
    }

    #[test]
    fn a_host_log_opens_its_session_with_the_first_event_and_shares_the_day_files() {
        let root = temp_dir("host-log");
        let paths = AppPaths::new(root.clone()).unwrap();
        let mut host = HostLog::new(&paths);
        assert!(
            read_events(&root).is_empty(),
            "a host that records nothing leaves no trace"
        );

        // The desktop session keeps the same day files open while the host appends to them.
        let mut desktop = Sink::new(root.join("logs"));
        desktop.event_line(&session(), &Event::info("job.stage"));
        host.record(Event::failure(
            "bridge.protocol_mismatch",
            error_codes::BRG_006,
            "client version 9",
        ));
        desktop.event_line(&session(), &Event::info("job.done"));
        host.record(Event::warn("bridge.listen_failed"));

        let events = read_events(&root);
        let names: Vec<&str> = events
            .iter()
            .map(|event| event["event"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "job.stage",
                "app.session",
                "bridge.protocol_mismatch",
                "job.done",
                "bridge.listen_failed"
            ]
        );
        assert_eq!(events[0]["channel"], "app");
        for index in [1, 2, 4] {
            assert_eq!(
                events[index]["channel"], "native-host",
                "{:?}",
                events[index]
            );
            assert_eq!(events[index]["session"], events[1]["session"]);
        }
        assert_eq!(events[2]["code"], "SSD-BRG-006");
        assert_eq!(events[2]["outcome"], "failed");

        let human: String = fs::read_dir(root.join("logs"))
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".log"))
            .map(|entry| fs::read_to_string(entry.path()).unwrap())
            .collect();
        assert!(
            human.contains("native-host | bridge.protocol_mismatch"),
            "{human}"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_host_log_follows_the_level_and_site_switch_stored_in_the_profile() {
        let root = temp_dir("host-log-settings");
        let paths = AppPaths::new(root.clone()).unwrap();
        let mut store = crate::store::Store::open(&paths.database).unwrap();
        store
            .save_settings(&crate::model::Settings {
                logging_level: "normal".into(),
                logging_hosts: false,
                ..Default::default()
            })
            .unwrap();
        drop(store);

        let mut host = HostLog::new(&paths);
        host.record(
            Event::failure(
                "bridge.browser_add_rejected",
                error_codes::MED_005,
                "Could not resolve host: files.example.com",
            )
            .host("files.example.com"),
        );
        host.record(Event::debug("bridge.noise"));

        let events = read_events(&root);
        let names: Vec<&str> = events
            .iter()
            .map(|event| event["event"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            ["app.session", "bridge.browser_add_rejected"],
            "the debug event is below the stored level"
        );
        assert!(events[0]["detail"]
            .as_str()
            .unwrap()
            .contains("level=normal"));
        assert!(events[1].get("host").is_none(), "{:?}", events[1]);
        assert!(
            !events[1]["detail"]
                .as_str()
                .unwrap()
                .contains("example.com"),
            "{:?}",
            events[1]
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn host_and_file_helpers_never_echo_the_full_address() {
        assert_eq!(
            host_of("https://www.Example.COM/watch?v=1").as_deref(),
            Some("www.example.com")
        );
        assert_eq!(host_of("yerel-dosya"), None);
        assert_eq!(
            file_name(Path::new(r"C:\Users\kişi\Downloads\video.mp4")),
            "video.mp4"
        );
    }
}
