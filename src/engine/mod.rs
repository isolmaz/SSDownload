use crate::{
    install, media,
    model::*,
    network::{ConnectionLease, NetworkGovernor},
    paths::AppPaths,
    store::Store,
    transfer,
};
use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Datelike, Local, TimeZone, Timelike, Utc};
use curl::easy::List;
use sha2::{Digest, Sha256};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex, RwLock,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use url::Url;
use uuid::Uuid;
use windows_sys::Win32::{
    Foundation::{CloseHandle, FILETIME, HANDLE, WAIT_TIMEOUT},
    System::Threading::{
        GetProcessTimes, OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
        PROCESS_SYNCHRONIZE,
    },
};

mod refresh;
mod scheduling;
mod sync;
use refresh::*;
use scheduling::*;
pub(crate) use sync::verify_file_result;
use sync::*;

type Reply<T> = mpsc::SyncSender<std::result::Result<T, String>>;

#[derive(Clone)]
pub struct Engine {
    inner: Arc<EngineInner>,
}

struct EngineInner {
    sender: mpsc::SyncSender<Message>,
    snapshot: Arc<RwLock<EngineSnapshot>>,
    network: NetworkGovernor,
    actor: Mutex<Option<JoinHandle<()>>>,
    closed: AtomicBool,
}

// Commands cross a bounded actor channel; the largest payload carries Settings/Job
// and boxing every variant would churn every match arm for a stack-sized value.
#[allow(clippy::large_enum_variant)]
enum Message {
    Command(Command),
    Worker(WorkerEvent),
}

enum Command {
    Add(Box<AddRequest>, Reply<Vec<String>>),
    Pause(String, Reply<()>),
    Resume(String, Reply<()>),
    Remove(String, bool, Reply<()>),
    PauseAll(Reply<()>),
    SetSpeedLimit(Vec<String>, u64, Reply<()>),
    Reorder(Vec<String>, bool, Reply<()>),
    MoveBefore(Vec<String>, String, Reply<()>),
    StartNow(String, Reply<()>),
    SetOpenWhenDone(String, bool, Reply<()>),
    Rename(String, String, Reply<()>),
    RetryFailed(Reply<()>),
    ResumeAll(Reply<()>),
    ClearCompleted(Reply<()>),
    UpdateSettings(Box<Settings>, Reply<()>),
    BeginSourceRefresh(String, Reply<SourceRefreshTicket>),
    CompleteSourceRefresh(String, String, Box<AddRequest>, bool, Reply<()>),
    CreateQueue(String, Reply<String>),
    RenameQueue(String, String, Reply<()>),
    DeleteQueue(String, Reply<()>),
    MoveToQueue(Vec<String>, String, Reply<()>),
    UpdateQueuePolicy(QueuePolicy, Reply<()>),
    CancelCompletionAction(String, Reply<()>),
    RunSynchronization(String, Reply<()>),
    BeginBrowserTransfer(String, u32, u64, Reply<(Job, u64)>),
    BrowserTransferJob(String, Reply<(Job, u64)>),
    BrowserTransferProgress(String, u64, Option<u64>, Reply<()>),
    TryBeginBrowserRequest(String, u64, String, Reply<bool>),
    EndBrowserRequest(String, u64, String, Reply<()>),
    PauseBrowserTransfer(String, u64, Reply<()>),
    ReleaseBrowserTransfer(String, u64, Reply<()>),
    FinishBrowserTransfer(String, u64, PathBuf, u64, Reply<()>),
    Shutdown,
}

enum WorkerEvent {
    Progress {
        id: String,
        generation: u64,
        value: TransferProgress,
    },
    Finished {
        id: String,
        generation: u64,
        result: std::result::Result<TransferResult, TransferFailure>,
    },
    SourceRefreshPrepared {
        generation: u64,
        result: std::result::Result<PreparedSourceRefresh, String>,
    },
    SynchronizationFinished {
        id: String,
        result: std::result::Result<SyncOutcome, String>,
    },
}

#[derive(Debug)]
enum PreparedSourceRefresh {
    Media(media::PreparedSourceRefresh),
    Direct(transfer::PreparedSourceRefresh),
}

struct SyncOutcome {
    etag: Option<String>,
    last_modified: Option<String>,
    content_sha256: Option<String>,
    changed: bool,
}

#[derive(Debug)]
enum DesiredStop {
    None,
    Pause,
    Resume,
    Remove { delete_file: bool },
    Shutdown,
}

/// Host of a job's source, for the event log (empty when the address has no host).
fn source_host_of(job: &Job) -> String {
    crate::logging::host_of(&job.request.url).unwrap_or_default()
}

/// Failure event for a job: the stable code when the message carries one, the classified
/// stage and kind otherwise, so the log states where the transfer broke.
fn failure_event(job: &Job, message: &str) -> crate::logging::Event {
    let (stage, kind) = crate::media::classify_download_error(message, &job.request);
    let detail = format!("aşama={stage} tür={kind} {message}");
    match crate::error_codes::from_text(message) {
        Some(code) => {
            crate::logging::Event::failure("job.fail", code, detail).host(source_host_of(job))
        }
        None => crate::logging::Event::new("job.fail", crate::logging::EventLevel::Error)
            .outcome(crate::logging::Outcome::Failed)
            .host(source_host_of(job))
            .detail(detail),
    }
}

/// Short, human-readable summary of what was requested, for `job.add`.
fn job_selection(job: &Job) -> String {
    let request = &job.request;
    let mut parts = vec![format!("kind={:?}", request.kind).to_lowercase()];
    if let Some(format) = &request.format_id {
        parts.push(format!("format={format}"));
    }
    if let Some(height) = request.exact_height.or(request.max_height) {
        parts.push(format!("height={height}"));
    }
    if let Some(container) = &request.container {
        parts.push(format!("container={container}"));
    }
    if let Some(language) = &request.audio_language {
        parts.push(format!("audio={language}"));
    }
    if !request.subtitle_languages.is_empty() {
        parts.push(format!(
            "subtitles={}",
            request.subtitle_languages.join(",")
        ));
    }
    parts.push(format!("name={}", job.name));
    parts.join(" ")
}

struct ActiveJob {
    /// Configured direct HTTP Range worker ceiling. Actual sockets acquire the
    /// shared governor individually rather than reserving guessed capacity.
    direct_connections: u8,
    /// The yt-dlp `--concurrent-fragments` value, if this job may use media.
    media_fragment_connections: u8,
    generation: u64,
    control: TransferControl,
    handle: JoinHandle<()>,
    desired: DesiredStop,
    last_persist: Instant,
    /// Last time a periodic progress sample was written to the event log.
    last_log: Instant,
    /// Start of the current attempt, reported as the age of a progress sample.
    attempt_started: Instant,
    /// Started through "Hemen başlat": exempt from the concurrency limits.
    forced: bool,
}

#[derive(Clone, Copy)]
struct ConnectionAllocation {
    direct: u8,
    media_fragments: u8,
    usage_modes: UsageModes,
}

struct ActiveSourceRefresh {
    id: String,
    control: TransferControl,
    handle: JoinHandle<()>,
    reply: Option<Reply<()>>,
    token: String,
    request: AddRequest,
    restart: bool,
    expected_path: PathBuf,
    expected_work_dir: Option<PathBuf>,
    expected_updated_at: i64,
}

#[derive(Clone)]
struct SourceRefreshGrant {
    token: String,
    expires_at: i64,
    generation: u64,
    request: AddRequest,
}

struct BrowserTransferSender(HANDLE);
unsafe impl Send for BrowserTransferSender {}

impl BrowserTransferSender {
    fn open(pid: u32, started_at: u64) -> Result<Self> {
        if pid == 0 {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Tarayıcı aktarım gönderici işlemi geçersiz",
                    "Browser transfer sender process is invalid"
                )
            );
        }
        let handle = unsafe {
            OpenProcess(
                PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
                0,
                pid,
            )
        };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error())
                .context("Tarayıcı aktarım gönderici işlemi açılamadı");
        }
        let sender = Self(handle);
        if process_started_at(sender.0)? != started_at || !sender.is_alive() {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Tarayıcı aktarım göndericisi artık aynı işlem değil",
                    "The browser transfer sender is no longer the same process"
                )
            );
        }
        Ok(sender)
    }

    fn is_alive(&self) -> bool {
        (unsafe { WaitForSingleObject(self.0, 0) }) == WAIT_TIMEOUT
    }
}

impl Drop for BrowserTransferSender {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

struct BrowserTransferLease {
    generation: u64,
    sender: BrowserTransferSender,
    requests: BTreeMap<String, ConnectionLease>,
    ended_requests: VecDeque<String>,
    sender_exited: bool,
}

fn process_started_at(handle: HANDLE) -> Result<u64> {
    let zero = || FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut created = zero();
    let mut exited = zero();
    let mut kernel = zero();
    let mut user = zero();
    if unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) } == 0 {
        return Err(std::io::Error::last_os_error())
            .context("Tarayıcı aktarım gönderici başlangıcı okunamadı");
    }
    Ok(u64::from(created.dwLowDateTime) | (u64::from(created.dwHighDateTime) << 32))
}

struct Actor {
    paths: AppPaths,
    network: NetworkGovernor,
    store: Store,
    jobs: BTreeMap<String, Job>,
    settings: Settings,
    snapshot: Arc<RwLock<EngineSnapshot>>,
    sender: mpsc::SyncSender<Message>,
    receiver: mpsc::Receiver<Message>,
    active: HashMap<String, ActiveJob>,
    retry_at: HashMap<String, Instant>,
    retry_connection_caps: HashMap<String, u8>,
    source_refresh: HashMap<String, SourceRefreshGrant>,
    source_refresh_workers: HashMap<u64, ActiveSourceRefresh>,
    browser_transfers: BTreeMap<String, BrowserTransferLease>,
    /// Requests removed before explicit browser completion remain counted rather
    /// than treating native-host death as proof that Chrome stopped fetching.
    browser_orphans: Vec<ConnectionLease>,
    /// The requested host cap is already applied to new acquisitions, while
    /// this records whether pre-existing actual sockets still need to drain.
    network_limit_pending: bool,
    queue_cursor: usize,
    quota_accounted: HashMap<String, u64>,
    completion_armed: BTreeSet<String>,
    completion_countdown: Option<CompletionCountdown>,
    completion_events: Vec<CompletionEvent>,
    synchronization_running: BTreeSet<String>,
    next_generation: u64,
    stopping: bool,
    persistence_error: Option<String>,
    warning: Option<String>,
    revision: u64,
    last_publish: Instant,
    dirty: bool,
    last_recovery: Instant,
    /// Whether this thread currently asks Windows to stay awake.
    keeping_awake: bool,
    /// Last metered-network probe and its answer.
    metered: bool,
    metered_checked: Instant,
    /// Last history pruning pass.
    history_checked: Instant,
}

impl Engine {
    pub fn open(paths: AppPaths) -> Result<Self> {
        let mut store = Store::open(&paths.database)?;
        let mut settings = store.load_settings()?.unwrap_or_default();
        let mut settings_migrated = false;
        if settings.download_dir.as_os_str().is_empty() {
            settings.download_dir = paths.download_dir.clone();
            settings_migrated = true;
        }
        // serde fills an absent Vec with empty for a 1.3.3 record. Introduce the
        // named default queue once without marking onboarding as completed.
        if settings.queues.is_empty() {
            let queue = QueuePolicy {
                concurrency: settings.max_active.max(1),
                ..QueuePolicy::default()
            };
            settings.queues.push(queue);
            settings_migrated = true;
        }
        if settings_migrated {
            store.save_settings(&settings)?;
        }
        if let Err(error) = validate_settings(&settings) {
            store.warnings.push(crate::i18n::ui_owned!(
                format!("Ayar sınırları düzeltildi: {error}"),
                format!("Settings limits were corrected: {error}")
            ));
            repair_settings(&mut settings, &paths);
            store.save_settings(&settings)?;
        }
        crate::proxy::configure_site_logins(&settings.site_logins);
        if let Err(error) = crate::proxy::configure(&settings.proxy) {
            store.warnings.push(crate::i18n::ui_owned!(
                format!("Proxy ayarı uygulanamadı, doğrudan bağlanılıyor: {error:#}"),
                format!("The proxy setting could not be applied; connecting directly: {error:#}")
            ));
        }
        let network = NetworkGovernor::new(settings.per_host_limit);
        if std::fs::create_dir_all(&settings.download_dir).is_err() {
            store
                .warnings
                .push(crate::i18n::ui(
                    "İndirme klasörüne erişilemiyor. Ayarlar'dan yeni bir hedef seçin; mevcut hedef değişmedi.",
                    "The download folder is not accessible. Choose a new destination in Settings; the current destination was unchanged.",
                )
                    .into());
        }
        crate::media::sweep_stale_cookie_jars();
        let mut jobs = store.recover_interrupted()?;
        let queue_ids = settings
            .queues
            .iter()
            .map(|queue| queue.id.as_str())
            .collect::<BTreeSet<_>>();
        let mut queue_migrations = Vec::new();
        for job in &mut jobs {
            if job
                .request
                .queue_id
                .as_deref()
                .is_none_or(|id| !queue_ids.contains(id))
            {
                job.request.queue_id = Some(DEFAULT_QUEUE_ID.into());
                queue_migrations.push(job.clone());
            }
        }
        if !queue_migrations.is_empty() {
            store.save_jobs(&queue_migrations)?;
        }
        for job in &mut jobs {
            if let Some(delete) = job.remove_requested {
                match crate::recovery::remove(&paths, job, delete) {
                    Ok(()) => {
                        store.delete_job(&job.id)?;
                        job.phase = "Kaldırıldı".into();
                    }
                    Err(error) => {
                        job.state = JobState::Cancelled;
                        job.error = Some(error.to_string());
                        store.save_job(job)?;
                    }
                }
            } else if job.state != JobState::Completed {
                match crate::recovery::recovered(&paths, &job.id) {
                    Ok(Some(done)) => {
                        job.path = done.path;
                        job.downloaded = done.bytes;
                        job.total = Some(done.bytes);
                        job.state = JobState::Completed;
                        job.phase = "Tamamlandı (kurtarıldı)".into();
                        job.request.forget_session();
                        store.save_job(job)?;
                        crate::logging::record_job(
                            &job.id,
                            crate::logging::Event::info("job.recovered")
                                .host(source_host_of(job))
                                .outcome(crate::logging::Outcome::Ok)
                                .detail(format!(
                                    "kayıttan tamamlandı dosya={} bayt={}",
                                    crate::logging::file_name(&job.path),
                                    job.downloaded
                                )),
                        );
                        if let Err(error) = crate::recovery::acknowledge(&paths, job) {
                            store.warnings.push(crate::i18n::ui_owned!(
                                format!("Geçici dosyalar temizlenemedi: {error}"),
                                format!("Temporary files could not be cleaned up: {error}")
                            ));
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        job.state = JobState::Paused;
                        job.error = Some(error.to_string());
                        store.save_job(job)?;
                    }
                }
            }
        }
        jobs.retain(|job| job.phase != "Kaldırıldı");
        // Completed records keep no session material (see `AddRequest::forget_session`).
        let mut forgotten = Vec::new();
        for job in jobs
            .iter_mut()
            .filter(|job| job.state == JobState::Completed)
        {
            if job.request.holds_session() {
                job.request.forget_session();
                forgotten.push(job.clone());
            }
        }
        if !forgotten.is_empty() {
            store.save_jobs(&forgotten)?;
        }
        let local = Local::now();
        let quota_rolled = refresh_queue_quota_periods_at(&mut settings, &local);
        if quota_rolled {
            settings.settings_revision = settings.settings_revision.wrapping_add(1).max(1);
        }
        let mut scheduling_changes = Vec::new();
        for job in &mut jobs {
            if reconcile_job_scheduling_state_at(&settings, job, &local) {
                scheduling_changes.push(job.clone());
            }
        }
        if quota_rolled || !scheduling_changes.is_empty() {
            store.save_jobs_and_settings(&scheduling_changes, &settings)?;
        }
        let warning = (!store.warnings.is_empty()).then(|| store.warnings.join("\n"));
        let job_map = jobs
            .into_iter()
            .map(|job| (job.id.clone(), job))
            .collect::<BTreeMap<_, _>>();
        let quota_accounted = job_map
            .iter()
            .map(|(id, job)| (id.clone(), job.downloaded))
            .collect::<HashMap<_, _>>();
        let completion_armed = job_map
            .values()
            .filter(|job| !job.state.is_terminal())
            .map(|job| job_queue_id(job).to_owned())
            .collect::<BTreeSet<_>>();
        let mut initial = make_snapshot(&job_map, &settings, None, &[]);
        initial.warning = warning.clone();
        let snapshot = Arc::new(RwLock::new(initial));
        let (sender, receiver) = mpsc::sync_channel(512);
        let actor_sender = sender.clone();
        let actor_snapshot = snapshot.clone();
        let actor_network = network.clone();
        let handle = thread::Builder::new()
            .name("ssdownload-engine".into())
            .spawn(move || {
                Actor {
                    paths,
                    network: actor_network,
                    store,
                    jobs: job_map,
                    settings,
                    snapshot: actor_snapshot,
                    sender: actor_sender,
                    receiver,
                    active: HashMap::new(),
                    retry_at: HashMap::new(),
                    retry_connection_caps: HashMap::new(),
                    source_refresh: HashMap::new(),
                    source_refresh_workers: HashMap::new(),
                    browser_transfers: BTreeMap::new(),
                    browser_orphans: Vec::new(),
                    network_limit_pending: false,
                    queue_cursor: 0,
                    quota_accounted,
                    completion_armed,
                    completion_countdown: None,
                    completion_events: Vec::new(),
                    synchronization_running: BTreeSet::new(),
                    next_generation: 1,
                    stopping: false,
                    persistence_error: None,
                    warning,
                    revision: 1,
                    last_publish: Instant::now() - Duration::from_secs(1),
                    dirty: false,
                    last_recovery: Instant::now(),
                    keeping_awake: false,
                    metered: false,
                    metered_checked: Instant::now() - Duration::from_secs(60),
                    history_checked: Instant::now() - Duration::from_secs(7200),
                }
                .run();
            })
            .context("İndirme motoru başlatılamadı")?;
        Ok(Self {
            inner: Arc::new(EngineInner {
                sender,
                snapshot,
                network,
                actor: Mutex::new(Some(handle)),
                closed: AtomicBool::new(false),
            }),
        })
    }

    pub fn snapshot(&self) -> EngineSnapshot {
        match self.inner.snapshot.try_read() {
            Ok(value) => value.clone(),
            Err(_) => self
                .inner
                .snapshot
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone(),
        }
    }

    pub(crate) fn network_governor(&self) -> NetworkGovernor {
        self.inner.network.clone()
    }

    pub fn add(&self, request: AddRequest) -> Result<Vec<String>> {
        // Creating the destination folder can be slow (a network share, a waking disk).
        // It happens here, on the caller's thread, so the actor that also serves every
        // running transfer only finds the folder in place. The actor still decides and
        // validates the final path.
        if let Ok(url) = Url::parse(&request.url) {
            let settings = self.snapshot().settings;
            let directory = request
                .directory
                .clone()
                .filter(|directory| {
                    directory.is_absolute()
                        && !directory
                            .components()
                            .any(|part| matches!(part, std::path::Component::ParentDir))
                })
                .or_else(|| matching_folder(&settings.folder_rules, &url, request.kind))
                .unwrap_or(settings.download_dir);
            let _ = std::fs::create_dir_all(directory);
        }
        self.request(|reply| Command::Add(Box::new(request), reply))
    }
    pub fn pause(&self, id: &str) -> Result<()> {
        self.request(|reply| Command::Pause(id.into(), reply))
    }
    pub fn resume(&self, id: &str) -> Result<()> {
        self.request(|reply| Command::Resume(id.into(), reply))
    }
    pub fn remove(&self, id: &str, delete_file: bool) -> Result<()> {
        self.request(|reply| Command::Remove(id.into(), delete_file, reply))
    }
    pub fn pause_all(&self) -> Result<()> {
        self.request(Command::PauseAll)
    }
    pub fn set_speed_limit(&self, ids: Vec<String>, kib: u64) -> Result<()> {
        self.request(|reply| Command::SetSpeedLimit(ids, kib, reply))
    }
    pub fn reorder(&self, ids: Vec<String>, top: bool) -> Result<()> {
        self.request(|reply| Command::Reorder(ids, top, reply))
    }
    pub fn move_before(&self, ids: Vec<String>, target: String) -> Result<()> {
        self.request(|reply| Command::MoveBefore(ids, target, reply))
    }
    pub fn start_now(&self, id: &str) -> Result<()> {
        self.request(|reply| Command::StartNow(id.into(), reply))
    }
    pub fn set_open_when_done(&self, id: &str, value: bool) -> Result<()> {
        self.request(|reply| Command::SetOpenWhenDone(id.into(), value, reply))
    }
    pub fn rename(&self, id: &str, name: &str) -> Result<()> {
        self.request(|reply| Command::Rename(id.into(), name.into(), reply))
    }
    pub fn retry_failed(&self) -> Result<()> {
        self.request(Command::RetryFailed)
    }
    pub fn resume_all(&self) -> Result<()> {
        self.request(Command::ResumeAll)
    }
    pub fn clear_completed(&self) -> Result<()> {
        self.request(Command::ClearCompleted)
    }
    pub fn update_settings(&self, settings: Settings) -> Result<()> {
        self.request(|reply| Command::UpdateSettings(Box::new(settings), reply))
    }
    pub fn begin_source_refresh(&self, id: &str) -> Result<SourceRefreshTicket> {
        self.request(|reply| Command::BeginSourceRefresh(id.into(), reply))
    }
    pub fn complete_source_refresh(
        &self,
        id: &str,
        token: &str,
        request: AddRequest,
        restart: bool,
    ) -> Result<()> {
        self.request(|reply| {
            Command::CompleteSourceRefresh(
                id.into(),
                token.into(),
                Box::new(request),
                restart,
                reply,
            )
        })
    }
    pub fn create_queue(&self, name: String) -> Result<String> {
        self.request(|reply| Command::CreateQueue(name, reply))
    }
    pub fn rename_queue(&self, id: String, name: String) -> Result<()> {
        self.request(|reply| Command::RenameQueue(id, name, reply))
    }
    pub fn delete_queue(&self, id: String) -> Result<()> {
        self.request(|reply| Command::DeleteQueue(id, reply))
    }
    pub fn move_to_queue(&self, ids: Vec<String>, queue_id: String) -> Result<()> {
        self.request(|reply| Command::MoveToQueue(ids, queue_id, reply))
    }
    pub fn update_queue_policy(&self, queue: QueuePolicy) -> Result<()> {
        self.request(|reply| Command::UpdateQueuePolicy(queue, reply))
    }
    pub fn cancel_completion_action(&self, queue_id: String) -> Result<()> {
        self.request(|reply| Command::CancelCompletionAction(queue_id, reply))
    }
    pub fn run_synchronization(&self, id: String) -> Result<()> {
        self.request(|reply| Command::RunSynchronization(id, reply))
    }
    pub fn begin_browser_transfer(
        &self,
        id: &str,
        sender_pid: u32,
        sender_started_at: u64,
    ) -> Result<(Job, u64)> {
        self.request(|reply| {
            Command::BeginBrowserTransfer(id.into(), sender_pid, sender_started_at, reply)
        })
    }
    pub fn browser_transfer_job(&self, id: &str) -> Result<(Job, u64)> {
        self.request(|reply| Command::BrowserTransferJob(id.into(), reply))
    }
    pub fn browser_transfer_progress(
        &self,
        id: &str,
        downloaded: u64,
        total: Option<u64>,
    ) -> Result<()> {
        self.request(|reply| Command::BrowserTransferProgress(id.into(), downloaded, total, reply))
    }
    pub(crate) fn try_begin_browser_request(
        &self,
        id: &str,
        generation: u64,
        request_id: &str,
    ) -> Result<bool> {
        self.request(|reply| {
            Command::TryBeginBrowserRequest(id.into(), generation, request_id.into(), reply)
        })
    }
    pub(crate) fn end_browser_request(
        &self,
        id: &str,
        generation: u64,
        request_id: &str,
    ) -> Result<()> {
        self.request(|reply| {
            Command::EndBrowserRequest(id.into(), generation, request_id.into(), reply)
        })
    }
    pub fn pause_browser_transfer(&self, id: &str, generation: u64) -> Result<()> {
        self.request(|reply| Command::PauseBrowserTransfer(id.into(), generation, reply))
    }
    pub fn release_browser_transfer(&self, id: &str, generation: u64) -> Result<()> {
        self.request(|reply| Command::ReleaseBrowserTransfer(id.into(), generation, reply))
    }
    pub fn finish_browser_transfer(
        &self,
        id: &str,
        generation: u64,
        path: PathBuf,
        bytes: u64,
    ) -> Result<()> {
        self.request(|reply| {
            Command::FinishBrowserTransfer(id.into(), generation, path, bytes, reply)
        })
    }

    pub fn shutdown(&self) {
        if !self.inner.closed.swap(true, Ordering::AcqRel) {
            let _ = self.inner.sender.send(Message::Command(Command::Shutdown));
        }
        let mut actor = self.inner.actor.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(handle) = actor.take() {
            let _ = handle.join();
        }
    }

    fn request<T>(&self, make: impl FnOnce(Reply<T>) -> Command) -> Result<T> {
        if self.inner.closed.load(Ordering::Acquire) {
            bail!(
                "{}",
                crate::i18n::ui("İndirme motoru kapalı", "The download engine is closed")
            );
        }
        let (sender, receiver) = mpsc::sync_channel(1);
        self.inner
            .sender
            .send(Message::Command(make(sender)))
            .map_err(|_| anyhow!("İndirme motoru kapalı"))?;
        receiver
            .recv()
            .map_err(|_| anyhow!("İndirme motoru yanıt vermedi"))?
            .map_err(anyhow::Error::msg)
    }
}

impl Drop for EngineInner {
    fn drop(&mut self) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            let _ = self.sender.send(Message::Command(Command::Shutdown));
        }
        let mut actor = self.actor.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(handle) = actor.take() {
            let _ = handle.join();
        }
    }
}

impl Actor {
    fn run(mut self) {
        self.publish();
        while !self.stopping {
            let received = self.receiver.recv_timeout(Duration::from_millis(150));
            // One faulty message must not end the engine: a panic is recorded, surfaced as a
            // warning and the loop keeps serving. A command's caller sees its reply dropped.
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                match received {
                    Ok(Message::Command(command)) => self.command(command),
                    Ok(Message::Worker(event)) => self.worker_event(event),
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => self.begin_shutdown(),
                }
                if self.dirty && self.last_publish.elapsed() >= Duration::from_millis(200) {
                    self.publish();
                }
                if !self.stopping {
                    self.schedule();
                    self.rebalance_speed_limits();
                }
            }));
            if let Err(panic) = outcome {
                self.record_panic(panic.as_ref());
            }
        }
        while !self.active.is_empty() || !self.source_refresh_workers.is_empty() {
            match self.receiver.recv_timeout(Duration::from_millis(100)) {
                Ok(Message::Worker(event)) => self.worker_event(event),
                Ok(Message::Command(_)) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(_) => break,
            }
        }
        self.finish_shutdown();
    }

    fn record_panic(&mut self, panic: &(dyn std::any::Any + Send)) {
        let detail = panic
            .downcast_ref::<&str>()
            .map(|value| (*value).to_owned())
            .or_else(|| panic.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".into());
        crate::logging::record(
            crate::logging::Event::warn("engine.panic")
                .outcome(crate::logging::Outcome::Failed)
                .detail(crate::logging::sanitize(&detail)),
        );
        self.warning = Some(
            crate::i18n::ui(
                "İndirme motorunda beklenmeyen bir hata oluştu; motor çalışmaya devam ediyor. Tanılama paketi oluşturup bildirin.",
                "The download engine hit an unexpected error and keeps running. Create a diagnostic package and report it.",
            )
            .into(),
        );
        self.dirty = true;
    }

    fn command(&mut self, command: Command) {
        self.last_publish = Instant::now() - Duration::from_secs(1);
        match command {
            Command::Add(request, reply) => respond(reply, self.add(*request)),
            Command::Pause(id, reply) => respond(reply, self.pause(&id)),
            Command::Resume(id, reply) => respond(reply, self.resume(&id)),
            Command::Remove(id, delete, reply) => respond(reply, self.remove(&id, delete)),
            Command::PauseAll(reply) => respond(reply, self.pause_all()),
            Command::SetSpeedLimit(ids, kib, reply) => {
                respond(reply, self.set_speed_limit(ids, kib))
            }
            Command::Reorder(ids, top, reply) => respond(reply, self.reorder(ids, top)),
            Command::MoveBefore(ids, target, reply) => {
                respond(reply, self.move_before(ids, &target))
            }
            Command::StartNow(id, reply) => respond(reply, self.start_now(&id)),
            Command::SetOpenWhenDone(id, value, reply) => {
                respond(reply, self.set_open_when_done(&id, value))
            }
            Command::Rename(id, name, reply) => respond(reply, self.rename(&id, &name)),
            Command::RetryFailed(reply) => respond(reply, self.retry_failed()),
            Command::ResumeAll(reply) => respond(reply, self.resume_all()),
            Command::ClearCompleted(reply) => respond(reply, self.clear_completed()),
            Command::UpdateSettings(settings, reply) => {
                respond(reply, self.update_settings(*settings))
            }
            Command::BeginSourceRefresh(id, reply) => {
                respond(reply, self.begin_source_refresh(&id))
            }
            Command::CompleteSourceRefresh(id, token, request, restart, reply) => {
                self.start_source_refresh(id, token, *request, restart, reply)
            }
            Command::CreateQueue(name, reply) => respond(reply, self.create_queue(name)),
            Command::RenameQueue(id, name, reply) => respond(reply, self.rename_queue(&id, name)),
            Command::DeleteQueue(id, reply) => respond(reply, self.delete_queue(&id)),
            Command::MoveToQueue(ids, queue_id, reply) => {
                respond(reply, self.move_to_queue(ids, &queue_id))
            }
            Command::UpdateQueuePolicy(queue, reply) => {
                respond(reply, self.update_queue_policy(queue))
            }
            Command::CancelCompletionAction(queue_id, reply) => {
                respond(reply, self.cancel_completion_action(&queue_id))
            }
            Command::RunSynchronization(id, reply) => respond(reply, self.run_synchronization(&id)),
            Command::BeginBrowserTransfer(id, sender_pid, sender_started_at, reply) => respond(
                reply,
                self.begin_browser_transfer(&id, sender_pid, sender_started_at),
            ),
            Command::BrowserTransferJob(id, reply) => {
                respond(reply, self.browser_transfer_job(&id))
            }
            Command::BrowserTransferProgress(id, downloaded, total, reply) => respond(
                reply,
                self.browser_transfer_progress(&id, downloaded, total),
            ),
            Command::TryBeginBrowserRequest(id, generation, request_id, reply) => respond(
                reply,
                self.try_begin_browser_request(&id, generation, &request_id),
            ),
            Command::EndBrowserRequest(id, generation, request_id, reply) => respond(
                reply,
                self.end_browser_request(&id, generation, &request_id),
            ),
            Command::PauseBrowserTransfer(id, generation, reply) => {
                respond(reply, self.pause_browser_transfer(&id, generation))
            }
            Command::ReleaseBrowserTransfer(id, generation, reply) => {
                respond(reply, self.release_browser_transfer(&id, generation))
            }
            Command::FinishBrowserTransfer(id, generation, path, bytes, reply) => respond(
                reply,
                self.finish_browser_transfer(&id, generation, path, bytes),
            ),
            Command::Shutdown => self.begin_shutdown(),
        }
    }

    fn add(&mut self, mut request: AddRequest) -> Result<Vec<String>> {
        crate::validation::validate_url(&request.url, false)?;
        let url = transfer::validate_source_url(&request.url)?;
        request.url = url.to_string();
        validate_request_headers(&request)?;
        crate::validation::validate_headers(&request.headers)?;
        if request
            .connections
            .is_some_and(|v| v == 0 || v > crate::validation::MAX_CONNECTIONS)
        {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Bağlantı sayısı 1–16 olmalıdır",
                    "Connection count must be between 1 and 16"
                )
            )
        }
        media::validate_options(&request)?;
        if request.queue_id.is_none() {
            request.queue_id = Some(DEFAULT_QUEUE_ID.into());
        }
        crate::validation::validate_new_job(&self.settings, &request)?;
        if let Some(key) = &request.request_id {
            if key.is_empty() || key.len() > 128 {
                bail!(
                    "{}",
                    crate::i18n::ui("Geçersiz istek kimliği", "Invalid request id")
                );
            }
            if let Some(existing) = self
                .jobs
                .values()
                .find(|job| job.request.request_id.as_ref() == Some(key))
            {
                if serde_json::to_value(existing.request.idempotency_view())?
                    != serde_json::to_value(request.idempotency_view())?
                {
                    bail!(
                        "{}",
                        crate::i18n::ui(
                            "Bu istek kimliği başka indirme seçenekleri için kullanılmış",
                            "This request id was already used for different download options"
                        )
                    );
                }
                return Ok(vec![existing.id.clone()]);
            }
        }
        if let Some(checksum) = request.checksum.as_deref() {
            validate_checksum_text(checksum)?;
        }
        let id = Uuid::new_v4().to_string();
        if request.request_id.is_none() {
            request.request_id = Some(id.clone());
        }
        if request.directory.as_deref().is_some_and(|directory| {
            !directory.is_absolute()
                || directory
                    .components()
                    .any(|part| matches!(part, std::path::Component::ParentDir))
        }) {
            bail!(
                "{}",
                crate::i18n::ui(
                    "İndirme klasörü güvenli, mutlak bir yol olmalı.",
                    "The download folder must be a safe, absolute path."
                )
            );
        }
        let directory = request
            .directory
            .clone()
            .or_else(|| matching_folder(&self.settings.folder_rules, &url, request.kind))
            .unwrap_or_else(|| self.settings.download_dir.clone());
        std::fs::create_dir_all(&directory)
            .with_context(|| format!("İndirme klasörü oluşturulamadı: {}", directory.display()))?;
        let suggested = request
            .filename
            .as_deref()
            .map(str::to_owned)
            .or_else(|| {
                url.path_segments()
                    .and_then(|mut segments| segments.next_back())
                    .filter(|v| !v.is_empty())
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| format!("download-{}", &id[..8]));
        let name = safe_filename(&suggested, &id);
        let desired = if request.playlist {
            directory.join(&name).with_extension("")
        } else {
            directory.join(&name)
        };
        let mut path = self.available_path(&desired);
        for attempt in 0..10000 {
            match crate::recovery::claim(&path, &id) {
                Ok(()) => break,
                Err(error) => {
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|e| e.kind() != std::io::ErrorKind::AlreadyExists)
                    {
                        return Err(error);
                    }
                }
            }
            if attempt == 9999 {
                bail!(
                    "{}",
                    crate::i18n::ui(
                        "Hedef adı ayrılamadı; klasör erişimini kontrol edin",
                        "The target name could not be reserved; check folder access"
                    )
                )
            }
            path = self.available_path(&crate::output::numbered(&desired, attempt + 1));
        }
        let local = Local::now();
        let now = local.timestamp();
        let state = job_scheduling_state_at(&self.settings, &request, &local);
        let job = Job {
            id: id.clone(),
            request,
            name: path
                .file_name()
                .and_then(|v| v.to_str())
                .unwrap_or(&name)
                .to_owned(),
            state,
            path: path.clone(),
            downloaded: 0,
            total: None,
            speed: 0,
            eta: None,
            error: None,
            phase: if state == JobState::Scheduled {
                crate::i18n::ui("Zamanlandı", "Scheduled").into()
            } else {
                crate::i18n::ui("Sırada", "Queued").into()
            },
            created_at: now,
            updated_at: now,
            attempts: 0,
            claim: Some(path.clone()),
            legacy_completed: false,
            browser_transfer_authorized: false,
            work_dir: Some(crate::recovery::work_dir(&path, &id)),
            remove_requested: None,
            priority: 0,
            force_start: false,
            open_when_done: false,
        };
        if let Err(error) = self.store.save_job(&job) {
            let _ = crate::recovery::release(&job.path, &id);
            return Err(error);
        }
        let queue_id = job_queue_id(&job).to_owned();
        crate::logging::record_job(
            &id,
            crate::logging::Event::info("job.add")
                .host(source_host_of(&job))
                .detail(job_selection(&job))
                .outcome(crate::logging::Outcome::Ok),
        );
        self.quota_accounted.insert(id.clone(), 0);
        self.jobs.insert(id.clone(), job);
        self.completion_armed.insert(queue_id);
        self.publish();
        Ok(vec![id])
    }

    fn transition(&mut self, ids: Vec<String>, pause: bool) -> Result<()> {
        // The per-job events below carry the outcome; this marker keeps the call visible
        // even when the requested job had already reached the requested state.
        let local = Local::now();
        let now = local.timestamp();
        let mut changed = Vec::new();
        for id in &ids {
            let mut job = self.jobs.get(id).cloned().context("İş bulunamadı")?;
            if job.state == JobState::Completed || job.remove_requested.is_some() {
                bail!(
                    "{}",
                    crate::i18n::ui(
                        "Bu işin durumu değiştirilemez",
                        "The state of this job cannot be changed"
                    )
                )
            }
            if !pause && job.browser_transfer_authorized {
                bail!(
                    "{}",
                    crate::i18n::ui(
                        "Bu işi kaynak sekmesindeki tarayıcı aktarımından sürdürün; aktarım yöntemi değiştirilmedi.",
                        "Resume this job from the browser transfer on its source tab; the transfer method was not changed."
                    )
                );
            }
            if !pause && job.state == JobState::AwaitingSource {
                bail!(
                    "{}",
                    crate::i18n::ui(
                        "Bu iş devam etmeden önce kaynak adresi yenilenmeli.",
                        "The source URL must be refreshed before this job can continue."
                    )
                )
            }
            job.state = if pause {
                JobState::Paused
            } else {
                job_scheduling_state_at(&self.settings, &job.request, &local)
            };
            job.speed = 0;
            job.eta = None;
            job.updated_at = now;
            job.phase = job.state.label().into();
            if !pause {
                job.error = None;
                job.attempts = 0;
            }
            changed.push(job);
        }
        self.store.save_jobs(&changed)?;
        for job in changed {
            if pause {
                self.remove_browser_transfer(&job.id);
                self.cancel_source_refresh_workers(&job.id);
                self.source_refresh.remove(&job.id);
            }
            if let Some(active) = self.active.get_mut(&job.id) {
                if pause {
                    active.control.pause();
                    active.desired = DesiredStop::Pause;
                } else {
                    active.desired = DesiredStop::Resume;
                }
            }
            if !pause {
                self.completion_armed.insert(job_queue_id(&job).to_owned());
                self.retry_connection_caps.remove(&job.id);
            }
            self.retry_at.remove(&job.id);
            crate::logging::record_job(
                &job.id,
                crate::logging::Event::info(if pause { "job.paused" } else { "job.resumed" })
                    .host(source_host_of(&job))
                    .detail(job.phase.clone()),
            );
            self.jobs.insert(job.id.clone(), job);
        }
        self.publish();
        Ok(())
    }
    fn pause(&mut self, id: &str) -> Result<()> {
        self.transition(vec![id.into()], true)
    }
    fn resume(&mut self, id: &str) -> Result<()> {
        self.transition(vec![id.into()], false)
    }
    /// Moves jobs to the top or bottom of the start order; the list shows the same order.
    fn reorder(&mut self, ids: Vec<String>, top: bool) -> Result<()> {
        let bound = if top {
            self.jobs
                .values()
                .map(|job| job.priority)
                .max()
                .unwrap_or(0)
                + 1
        } else {
            self.jobs
                .values()
                .map(|job| job.priority)
                .min()
                .unwrap_or(0)
                - 1
        };
        let mut changed = Vec::new();
        for (offset, id) in ids.iter().enumerate() {
            let job = self.jobs.get_mut(id).context("İş bulunamadı")?;
            // Keep the selection's own order: the first id ends up first.
            job.priority = if top {
                bound + (ids.len() - offset) as i64
            } else {
                bound - offset as i64
            };
            changed.push(job.clone());
        }
        self.store.save_jobs(&changed)?;
        self.publish();
        Ok(())
    }

    /// Renumbers the whole queue order so `ids` sit right before `target`.
    fn move_before(&mut self, ids: Vec<String>, target: &str) -> Result<()> {
        if ids.iter().any(|id| id == target) {
            return Ok(());
        }
        let mut order = self.jobs.values().collect::<Vec<_>>();
        order.sort_by(|left, right| {
            (-left.priority, left.created_at, &left.id).cmp(&(
                -right.priority,
                right.created_at,
                &right.id,
            ))
        });
        let mut order = order
            .into_iter()
            .map(|job| job.id.clone())
            .filter(|id| !ids.contains(id))
            .collect::<Vec<_>>();
        let at = order
            .iter()
            .position(|id| id == target)
            .context("İş bulunamadı")?;
        for (offset, id) in ids.iter().enumerate() {
            if !self.jobs.contains_key(id) {
                bail!("İş bulunamadı");
            }
            order.insert(at + offset, id.clone());
        }
        let count = order.len() as i64;
        let mut changed = Vec::new();
        for (index, id) in order.iter().enumerate() {
            if let Some(job) = self.jobs.get_mut(id) {
                let priority = count - index as i64;
                if job.priority != priority {
                    job.priority = priority;
                    changed.push(job.clone());
                }
            }
        }
        self.store.save_jobs(&changed)?;
        self.publish();
        Ok(())
    }

    fn start_now(&mut self, id: &str) -> Result<()> {
        let job = self.jobs.get_mut(id).context("İş bulunamadı")?;
        if job.state.is_terminal() && job.state != JobState::Failed
            || job.remove_requested.is_some()
            || job.browser_transfer_authorized
        {
            bail!(
                "{}",
                crate::i18n::ui("Bu iş hemen başlatılamaz", "This job cannot be started now")
            );
        }
        if self.active.contains_key(id) {
            return Ok(());
        }
        job.force_start = true;
        job.state = JobState::Queued;
        job.request.start_at = None;
        job.error = None;
        job.phase = crate::i18n::ui("Hemen başlatılıyor", "Starting now").into();
        let job = job.clone();
        self.retry_at.remove(id);
        self.store.save_job(&job)?;
        self.publish();
        Ok(())
    }

    fn set_open_when_done(&mut self, id: &str, value: bool) -> Result<()> {
        let job = self.jobs.get_mut(id).context("İş bulunamadı")?;
        job.open_when_done = value;
        let job = job.clone();
        self.store.save_job(&job)?;
        self.publish();
        Ok(())
    }

    /// Renames a completed download in its own folder, never over an existing file,
    /// and moves the publication journal with it.
    fn rename(&mut self, id: &str, name: &str) -> Result<()> {
        let job = self.jobs.get(id).cloned().context("İş bulunamadı")?;
        if job.state != JobState::Completed || !job.path.is_file() {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Yalnız tamamlanmış dosyalar yeniden adlandırılabilir.",
                    "Only completed files can be renamed."
                )
            );
        }
        let name = safe_filename(name.trim(), id);
        let target = job.path.with_file_name(&name);
        if target == job.path {
            return Ok(());
        }
        if target.exists() {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Bu adla bir dosya zaten var.",
                    "A file with this name already exists."
                )
            );
        }
        std::fs::rename(&job.path, &target).with_context(|| {
            crate::i18n::ui(
                "Dosya yeniden adlandırılamadı",
                "The file could not be renamed",
            )
        })?;
        if let Err(error) = crate::recovery::retarget(&self.paths, id, &target) {
            self.warning = Some(error.to_string());
        }
        let job = self.jobs.get_mut(id).context("İş bulunamadı")?;
        job.path = target;
        job.name = name;
        let job = job.clone();
        self.store.save_job(&job)?;
        self.publish();
        Ok(())
    }

    fn retry_failed(&mut self) -> Result<()> {
        let ids = self
            .jobs
            .values()
            .filter(|job| job.state == JobState::Failed && job.remove_requested.is_none())
            .map(|job| job.id.clone())
            .collect::<Vec<_>>();
        if ids.is_empty() {
            return Ok(());
        }
        self.transition(ids, false)
    }

    /// Whether the network state lets work run: with `pause_on_metered`, a metered
    /// connection (Windows connectivity hint) holds new and running downloads.
    fn network_allows_work(&mut self) -> bool {
        if !self.settings.pause_on_metered {
            self.metered = false;
            return true;
        }
        if self.metered_checked.elapsed() >= Duration::from_secs(15) {
            self.metered_checked = Instant::now();
            self.metered = connection_is_metered();
        }
        !self.metered
    }

    /// Removes completed jobs older than `history_days` from the list; files stay.
    fn prune_history(&mut self) {
        if self.settings.history_days == 0
            || self.history_checked.elapsed() < Duration::from_secs(3600)
        {
            return;
        }
        self.history_checked = Instant::now();
        let cutoff = Utc::now().timestamp() - i64::from(self.settings.history_days) * 86_400;
        let old = self
            .jobs
            .values()
            .filter(|job| job.state == JobState::Completed && job.updated_at < cutoff)
            .map(|job| job.id.clone())
            .collect::<Vec<_>>();
        if old.is_empty() {
            return;
        }
        if self.store.delete_jobs(&old).is_ok() {
            for id in old {
                self.jobs.remove(&id);
                let _ = crate::recovery::forget(&self.paths, &id);
            }
            self.publish();
        }
    }

    fn set_speed_limit(&mut self, ids: Vec<String>, kib: u64) -> Result<()> {
        if kib > u64::MAX / 1024 {
            bail!(
                "{}",
                crate::i18n::ui("Hız sınırı çok büyük.", "The speed limit is too large.")
            );
        }
        let mut changed = Vec::new();
        for id in &ids {
            let job = self.jobs.get_mut(id).context("İş bulunamadı")?;
            job.request.speed_limit_kib = (kib > 0).then_some(kib);
            changed.push(job.clone());
        }
        self.store.save_jobs(&changed)?;
        self.rebalance_speed_limits();
        self.publish();
        Ok(())
    }

    fn pause_all(&mut self) -> Result<()> {
        let ids = self
            .jobs
            .values()
            .filter(|j| j.state != JobState::Completed && j.remove_requested.is_none())
            .map(|j| j.id.clone())
            .collect();
        self.transition(ids, true)
    }
    fn resume_all(&mut self) -> Result<()> {
        let ids = self
            .jobs
            .values()
            .filter(|j| {
                matches!(
                    j.state,
                    JobState::Paused | JobState::Failed | JobState::Cancelled
                ) && j.remove_requested.is_none()
                    && !j.browser_transfer_authorized
            })
            .map(|j| j.id.clone())
            .collect();
        self.transition(ids, false)
    }
    fn remove(&mut self, id: &str, delete_file: bool) -> Result<()> {
        let mut job = self.jobs.get(id).cloned().context("İş bulunamadı")?;
        let queue_id = job_queue_id(&job).to_owned();
        job.legacy_completed |= job.work_dir.is_none() && job.state == JobState::Completed;
        job.remove_requested = Some(delete_file);
        job.state = JobState::Cancelled;
        job.phase = "Kaldırılıyor".into();
        self.store.save_job(&job)?;
        self.jobs.insert(id.into(), job.clone());
        self.retry_at.remove(id);
        self.retry_connection_caps.remove(id);
        crate::logging::record_job(
            id,
            crate::logging::Event::info("job.cancelled")
                .host(source_host_of(&job))
                .outcome(crate::logging::Outcome::Skipped)
                .detail(if delete_file {
                    "kaldırıldı, dosya silinecek"
                } else {
                    "kaldırıldı, dosya korunacak"
                }),
        );
        let refresh_running = self
            .source_refresh_workers
            .values()
            .any(|active| active.id == id);
        self.cancel_source_refresh_workers(id);
        self.source_refresh.remove(id);
        self.remove_browser_transfer(id);
        self.quota_accounted.remove(id);
        self.completion_armed.remove(&queue_id);
        if self
            .completion_countdown
            .as_ref()
            .is_some_and(|countdown| countdown.queue_id == queue_id)
        {
            self.completion_countdown = None;
        }
        if let Some(active) = self.active.get_mut(id) {
            active.control.cancel();
            active.desired = DesiredStop::Remove { delete_file };
            self.publish();
            return Ok(());
        }
        if refresh_running {
            self.publish();
            return Ok(());
        }
        self.finish_remove(id, delete_file)
    }
    fn finish_remove(&mut self, id: &str, delete_file: bool) -> Result<()> {
        let job = self.jobs.get(id).cloned().context("İş bulunamadı")?;
        let result = crate::recovery::remove(&self.paths, &job, delete_file);
        if let Err(error) = result {
            if let Some(job) = self.jobs.get_mut(id) {
                job.error = Some(error.to_string());
                job.phase = crate::i18n::ui(
                    "Silinemedi; yeniden deneyebilirsiniz",
                    "Could not be deleted; try again",
                )
                .into();
            }
            self.persist_job(id);
            self.publish();
            return Err(error);
        }
        self.store.delete_job(id)?;
        self.jobs.remove(id);
        if let Err(error) = crate::recovery::forget(&self.paths, id) {
            self.warning = Some(format!("Eski tamamlama kaydı silinemedi: {error}"));
        }
        self.publish();
        Ok(())
    }

    fn clear_completed(&mut self) -> Result<()> {
        let completed = self
            .jobs
            .iter()
            .filter(|&(_, job)| job.state == JobState::Completed)
            .map(|(id, job)| (id.clone(), job_queue_id(job).to_owned()))
            .collect::<Vec<_>>();
        let ids = completed
            .iter()
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        self.store.delete_jobs(&ids)?;
        for queue_id in completed.iter().map(|(_, queue_id)| queue_id) {
            self.completion_armed.remove(queue_id);
            if self
                .completion_countdown
                .as_ref()
                .is_some_and(|countdown| countdown.queue_id == queue_id.as_str())
            {
                self.completion_countdown = None;
            }
        }
        for (id, _) in completed {
            self.jobs.remove(&id);
            if let Err(error) = crate::recovery::forget(&self.paths, &id) {
                self.warning = Some(format!("Eski tamamlama kaydı silinemedi: {error}"));
            }
        }
        self.publish();
        Ok(())
    }

    fn update_settings(&mut self, mut settings: Settings) -> Result<()> {
        if settings.download_dir.as_os_str().is_empty() {
            settings.download_dir = self.paths.download_dir.clone();
        }
        settings.onboarding_version = settings
            .onboarding_version
            .max(self.settings.onboarding_version);
        // Quota consumption and synchronization validators are engine-owned runtime
        // state. A settings dialog may have been open while they advanced; copying
        // that stale snapshot back must not grant bytes or forget remote identity.
        for queue in &mut settings.queues {
            if let Some(saved) = self
                .settings
                .queues
                .iter()
                .find(|saved| saved.id == queue.id)
            {
                if let (Some(incoming), Some(authoritative)) = (&mut queue.quota, &saved.quota) {
                    incoming.consumed_bytes = authoritative.consumed_bytes;
                    incoming.period_key = authoritative.period_key.clone();
                }
            }
        }
        for policy in &mut settings.synchronization_policies {
            if let Some(saved) = self.settings.synchronization_policies.iter().find(|saved| {
                saved.id == policy.id
                    && saved.url == policy.url
                    && saved.destination == policy.destination
            }) {
                policy.etag = saved.etag.clone();
                policy.last_modified = saved.last_modified.clone();
                policy.content_sha256 = saved.content_sha256.clone();
                policy.last_checked_at = saved.last_checked_at;
                policy.version = saved.version;
                policy.last_error = saved.last_error.clone();
            }
        }
        settings.settings_revision = self.settings.settings_revision.wrapping_add(1).max(1);
        validate_settings(&settings)?;
        let queue_ids = settings
            .queues
            .iter()
            .map(|queue| queue.id.as_str())
            .collect::<BTreeSet<_>>();
        if self
            .jobs
            .values()
            .any(|job| !queue_ids.contains(job_queue_id(job)))
        {
            bail!(
                "{}",
                crate::i18n::ui(
                    "İşi bulunan bir kuyruk Ayarlar üzerinden kaldırılamaz; Kuyruğu Sil eylemini kullanın.",
                    "A queue that contains jobs cannot be removed from Settings; use the Delete Queue action."
                )
            );
        }
        std::fs::create_dir_all(&settings.download_dir).with_context(|| {
            format!(
                "İndirme klasörü oluşturulamadı: {}",
                settings.download_dir.display()
            )
        })?;
        let local = Local::now();
        refresh_queue_quota_periods_at(&mut settings, &local);
        let scheduling_changes = self
            .jobs
            .values()
            .filter(|job| !self.active.contains_key(&job.id))
            .filter_map(|job| {
                let mut updated = job.clone();
                reconcile_job_scheduling_state_at(&settings, &mut updated, &local)
                    .then_some(updated)
            })
            .collect::<Vec<_>>();
        let proxy_changed = settings.proxy != self.settings.proxy;
        if proxy_changed {
            if let Err(error) = crate::proxy::configure(&settings.proxy) {
                let _ = crate::proxy::configure(&self.settings.proxy);
                return Err(error.context(crate::i18n::ui(
                    "Proxy ayarı uygulanamadı",
                    "The proxy setting could not be applied",
                )));
            }
        }
        if let Err(error) = self
            .store
            .save_jobs_and_settings(&scheduling_changes, &settings)
        {
            if proxy_changed {
                let _ = crate::proxy::configure(&self.settings.proxy);
            }
            return Err(error);
        }
        if self.completion_countdown.as_ref().is_some_and(|countdown| {
            settings
                .queues
                .iter()
                .find(|queue| queue.id == countdown.queue_id)
                .is_none_or(|queue| !queue.enabled || queue.completion != countdown.action)
        }) {
            self.completion_countdown = None;
        }
        for job in scheduling_changes {
            self.jobs.insert(job.id.clone(), job);
        }
        self.network.set_per_host_limit(settings.per_host_limit);
        self.network_limit_pending = self.network.per_host_limit_pending();
        crate::proxy::configure_site_logins(&settings.site_logins);
        self.settings = settings;
        self.rebalance_speed_limits();
        self.publish();
        Ok(())
    }

    fn begin_source_refresh(&mut self, id: &str) -> Result<SourceRefreshTicket> {
        let now = Utc::now().timestamp();
        let mut job = self.jobs.get(id).cloned().context("İş bulunamadı")?;
        if job.state == JobState::Completed
            || job.remove_requested.is_some()
            || self.active.contains_key(id)
            || self.browser_transfers.contains_key(id)
        {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Etkin, tamamlanmış veya kaldırılan işin kaynağı yenilenemez.",
                    "The source of an active, completed, or removed job cannot be refreshed."
                )
            );
        }
        let page_url = job
            .request
            .source_identity
            .as_ref()
            .map(|identity| identity.page_url.clone())
            .filter(|value| !value.is_empty())
            .or_else(|| {
                job.request
                    .page_url
                    .clone()
                    .filter(|value| !value.is_empty())
            })
            .context("Bu iş için yenilenebilecek kaynak sayfası kaydedilmemiş.")?;
        crate::validation::validate_url(&page_url, true)?;
        let generation = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        let grant = SourceRefreshGrant {
            token: Uuid::new_v4().to_string(),
            expires_at: now + 5 * 60,
            generation,
            request: job.request.clone(),
        };
        job.state = JobState::AwaitingSource;
        job.speed = 0;
        job.eta = None;
        job.phase = "Kaynak adresinin yenilenmesi bekleniyor".into();
        job.updated_at = now;
        self.store.save_job(&job)?;
        let ticket = SourceRefreshTicket {
            job_id: id.into(),
            token: grant.token.clone(),
            expires_at: grant.expires_at,
            page_url,
            request: Box::new(grant.request.clone()),
        };
        self.jobs.insert(id.into(), job);
        self.cancel_source_refresh_workers(id);
        self.source_refresh.insert(id.into(), grant);
        self.publish();
        Ok(ticket)
    }

    fn start_source_refresh(
        &mut self,
        id: String,
        token: String,
        request: AddRequest,
        restart: bool,
        reply: Reply<()>,
    ) {
        match self.spawn_source_refresh(id, token, request, restart) {
            Ok((generation, mut active)) => {
                active.reply = Some(reply);
                self.source_refresh_workers.insert(generation, active);
            }
            Err(error) => respond(reply, Err(error)),
        }
    }

    fn spawn_source_refresh(
        &mut self,
        id: String,
        token: String,
        mut request: AddRequest,
        restart: bool,
    ) -> Result<(u64, ActiveSourceRefresh)> {
        let now = Utc::now().timestamp();
        let grant = self
            .source_refresh
            .get(&id)
            .cloned()
            .context("Kaynak yenileme isteği bulunamadı")?;
        if grant.token != token || grant.expires_at <= now {
            self.source_refresh.remove(&id);
            bail!(
                "{}",
                crate::i18n::ui(
                    "Kaynak yenileme anahtarı geçersiz veya süresi dolmuş.",
                    "The source-refresh grant is invalid or expired."
                )
            );
        }
        if self.source_refresh_workers.contains_key(&grant.generation) {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Bu işin kaynak yenileme denetimi zaten sürüyor.",
                    "A source-refresh check for this job is already running."
                )
            );
        }
        let job = self.jobs.get(&id).cloned().context("İş bulunamadı")?;
        if job.state != JobState::AwaitingSource || job.remove_requested.is_some() {
            bail!(
                "{}",
                crate::i18n::ui(
                    "İş artık kaynak yenileme beklemiyor.",
                    "The job is no longer waiting for a source refresh."
                )
            );
        }
        crate::validation::validate_url(&request.url, false)?;
        request.url = transfer::validate_source_url(&request.url)?.to_string();
        validate_request_headers(&request)?;
        crate::validation::validate_headers(&request.headers)?;
        media::validate_options(&request)?;
        if !refresh_identity_matches(&grant.request, &request) {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Yeni kaynak farklı bir sayfa, oynatıcı, tarayıcı profili veya indirme seçimine ait.",
                    "The new source belongs to a different page, player, browser profile, or download selection."
                )
            );
        }
        preserve_refresh_contract(&grant.request, &mut request);

        let output = refresh_output_path(&job)?;
        let use_media = uses_media_refresh(&request);
        let control = TransferControl::default();
        let thread_control = control.clone();
        let paths = self.paths.clone();
        let network = self.network.clone();
        let previous = grant.request.clone();
        let next = request.clone();
        let worker_output = output.clone();
        let sender = self.sender.clone();
        let generation = grant.generation;
        let error_request = request.clone();
        let handle = thread::Builder::new()
            .name(format!("ssdownload-refresh-{}", &id[..id.len().min(8)]))
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    if use_media {
                        media::prepare_source_refresh(
                            &paths,
                            &previous,
                            &next,
                            &worker_output,
                            restart,
                            &thread_control,
                            &network,
                        )
                        .map(PreparedSourceRefresh::Media)
                    } else {
                        transfer::prepare_source_refresh(
                            &previous,
                            &next,
                            &worker_output,
                            restart,
                            &thread_control,
                            &network,
                        )
                        .map(PreparedSourceRefresh::Direct)
                    }
                }))
                .unwrap_or_else(|_| {
                    Err(anyhow!("Kaynak yenileme işçisi beklenmedik şekilde durdu"))
                })
                .map_err(|error| {
                    let message = safe_error(&error_request, &format!("{error:#}"));
                    safe_error(&previous, &message)
                });
                let _ = sender.send(Message::Worker(WorkerEvent::SourceRefreshPrepared {
                    generation,
                    result,
                }));
            })
            .context("Kaynak yenileme işçisi başlatılamadı")?;
        Ok((
            generation,
            ActiveSourceRefresh {
                id,
                control,
                handle,
                reply: None,
                token,
                request,
                restart,
                expected_path: job.path,
                expected_work_dir: job.work_dir,
                expected_updated_at: job.updated_at,
            },
        ))
    }

    fn commit_source_refresh(
        &mut self,
        generation: u64,
        active: &ActiveSourceRefresh,
        prepared: PreparedSourceRefresh,
    ) -> Result<()> {
        let local = Local::now();
        let now = local.timestamp();
        let grant = self
            .source_refresh
            .get(&active.id)
            .cloned()
            .context("Kaynak yenileme isteği artık geçerli değil")?;
        let job = self
            .jobs
            .get(&active.id)
            .cloned()
            .context("İş artık bulunamadı")?;
        if self.stopping
            || grant.generation != generation
            || grant.token != active.token
            || grant.expires_at <= now
            || job.state != JobState::AwaitingSource
            || job.remove_requested.is_some()
            || job.path != active.expected_path
            || job.work_dir != active.expected_work_dir
            || job.updated_at != active.expected_updated_at
            || !refresh_identity_matches(&grant.request, &active.request)
        {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Kaynak yenileme hazırlığı artık geçerli değil; çalışma verileri değiştirilmedi.",
                    "The source-refresh preparation is no longer valid; work data was not modified."
                )
            );
        }
        let output = refresh_output_path(&job)?;
        let state_path = append_path_suffix(&output, ".ssdownload.state");
        let state_backup = (!active.restart && state_path.exists())
            .then(|| fs::read(&state_path).context("Kaynak kimliği kaydı yedeklenemedi"))
            .transpose()?;

        let (media_staged, direct_staged) = match &prepared {
            PreparedSourceRefresh::Media(prepared) => {
                (media::stage_source_refresh(prepared)?, false)
            }
            PreparedSourceRefresh::Direct(prepared) if !active.restart => {
                (false, transfer::stage_source_refresh(prepared)?)
            }
            PreparedSourceRefresh::Direct(_) => (false, false),
        };
        let archive = if active.restart {
            archive_refresh_artifacts(&job, &output)?
        } else {
            None
        };

        let mut updated = job;
        updated.request = active.request.clone();
        updated.browser_transfer_authorized = false;
        updated.state = job_scheduling_state_at(&self.settings, &updated.request, &local);
        updated.error = None;
        updated.attempts = 0;
        updated.downloaded = if active.restart {
            0
        } else {
            updated.downloaded
        };
        updated.total = if active.restart { None } else { updated.total };
        updated.phase = if updated.state == JobState::Scheduled {
            updated.state.label().into()
        } else if active.restart {
            "Kaynak yenilendi; önceki parçalar arşivlendi, baştan başlayacak".into()
        } else {
            "Kaynak yenilendi; indirme yeniden planlandı.".into()
        };
        updated.updated_at = now;
        if let Err(save_error) = self.store.save_job(&updated) {
            let mut rollback_error = None;
            if let Some(archive) = &archive {
                if let Err(error) = restore_refresh_archive(archive) {
                    rollback_error = Some(error);
                }
            }
            if media_staged {
                if let Err(error) = media::cancel_source_refresh(&output) {
                    rollback_error = Some(error);
                }
            } else if direct_staged && archive.is_none() {
                let rollback = match state_backup {
                    Some(bytes) => crate::recovery::atomic_write(&state_path, &bytes),
                    None => match fs::remove_file(&state_path) {
                        Ok(()) => Ok(()),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                        Err(error) => Err(error.into()),
                    },
                };
                if let Err(error) = rollback {
                    rollback_error = Some(error);
                }
            }
            if let Some(error) = rollback_error {
                return Err(save_error).context(format!(
                    "Kaynak yenileme kaydedilemedi ve çalışma durumu geri alınamadı: {error:#}"
                ));
            }
            return Err(save_error).context("Kaynak yenileme atomik olarak kaydedilemedi");
        }

        let finalization_error = if media_staged {
            media::finish_source_refresh(&output).err().map(|error| {
                format!(
                    "Kaynak yenileme kaydedildi ancak medya çalışma kaydı henüz tamamlanamadı: {error:#}"
                )
            })
        } else {
            None
        };
        if let Some(message) = &finalization_error {
            updated.state = JobState::Paused;
            updated.error = Some(message.clone());
            updated.phase = "Kaynak yenileme kaydı tamamlanamadı; iş güvenle duraklatıldı".into();
            updated.updated_at = Utc::now().timestamp();
            if let Err(error) = self.store.save_job(&updated) {
                self.warning = Some(format!(
                    "{message} Ayrıca güvenli duraklatma kaydedilemedi: {error:#}"
                ));
            }
        }

        self.quota_accounted
            .insert(active.id.clone(), updated.downloaded);
        let queue_id = job_queue_id(&updated).to_owned();
        self.jobs.insert(active.id.clone(), updated);
        self.source_refresh.remove(&active.id);
        self.completion_armed.insert(queue_id);
        self.publish();
        if let Some(message) = finalization_error {
            bail!(message);
        }
        Ok(())
    }

    fn cancel_source_refresh_workers(&self, id: &str) {
        for active in self
            .source_refresh_workers
            .values()
            .filter(|active| active.id == id)
        {
            active.control.cancel();
        }
    }

    fn create_queue(&mut self, name: String) -> Result<String> {
        let id = Uuid::new_v4().to_string();
        let queue = QueuePolicy {
            id: id.clone(),
            name: name.trim().to_owned(),
            concurrency: self.settings.max_active.max(1),
            ..QueuePolicy::default()
        };
        crate::validation::validate_queue(&queue)?;
        let mut settings = self.settings.clone();
        settings.queues.push(queue);
        settings.settings_revision = settings.settings_revision.wrapping_add(1).max(1);
        validate_settings(&settings)?;
        self.store.save_settings(&settings)?;
        self.settings = settings;
        self.publish();
        Ok(id)
    }

    fn rename_queue(&mut self, id: &str, name: String) -> Result<()> {
        let mut settings = self.settings.clone();
        let queue = settings
            .queues
            .iter_mut()
            .find(|queue| queue.id == id)
            .context("Kuyruk bulunamadı")?;
        queue.name = name.trim().to_owned();
        crate::validation::validate_queue(queue)?;
        settings.settings_revision = settings.settings_revision.wrapping_add(1).max(1);
        self.store.save_settings(&settings)?;
        self.settings = settings;
        self.publish();
        Ok(())
    }

    fn delete_queue(&mut self, id: &str) -> Result<()> {
        if id == DEFAULT_QUEUE_ID {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Varsayılan kuyruk silinemez.",
                    "The default queue cannot be deleted."
                )
            );
        }
        if self
            .active
            .keys()
            .chain(self.browser_transfers.keys())
            .any(|job_id| {
                self.jobs
                    .get(job_id)
                    .is_some_and(|job| job_queue_id(job) == id)
            })
        {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Etkin işi bulunan kuyruk silinmeden önce işler duraklatılmalı.",
                    "Jobs must be paused before deleting a queue that has active jobs."
                )
            );
        }
        let mut settings = self.settings.clone();
        let before = settings.queues.len();
        settings.queues.retain(|queue| queue.id != id);
        if settings.queues.len() == before {
            bail!(
                "{}",
                crate::i18n::ui("Kuyruk bulunamadı.", "Queue not found.")
            );
        }
        settings.settings_revision = settings.settings_revision.wrapping_add(1).max(1);
        let local = Local::now();
        let now = local.timestamp();
        let mut changed = Vec::new();
        for job in self.jobs.values() {
            if job_queue_id(job) == id {
                let mut moved = job.clone();
                moved.request.queue_id = Some(DEFAULT_QUEUE_ID.into());
                if !reconcile_job_scheduling_state_at(&settings, &mut moved, &local) {
                    moved.updated_at = now;
                }
                changed.push(moved);
            }
        }
        self.store.save_jobs_and_settings(&changed, &settings)?;
        let moved_pending = changed.iter().any(|job| !job.state.is_terminal());
        for job in changed {
            self.jobs.insert(job.id.clone(), job);
        }
        self.settings = settings;
        self.completion_armed.remove(id);
        if moved_pending {
            self.completion_armed.insert(DEFAULT_QUEUE_ID.into());
        }
        if self
            .completion_countdown
            .as_ref()
            .is_some_and(|countdown| countdown.queue_id == id)
        {
            self.completion_countdown = None;
        }
        self.publish();
        Ok(())
    }

    fn move_to_queue(&mut self, ids: Vec<String>, queue_id: &str) -> Result<()> {
        if !self
            .settings
            .queues
            .iter()
            .any(|queue| queue.id == queue_id)
        {
            bail!(
                "{}",
                crate::i18n::ui("Hedef kuyruk bulunamadı.", "Target queue not found.")
            );
        }
        let unique = ids.into_iter().collect::<BTreeSet<_>>();
        if unique.is_empty() {
            bail!(
                "{}",
                crate::i18n::ui("Taşınacak iş seçilmedi.", "No jobs were selected to move.")
            );
        }
        let local = Local::now();
        let now = local.timestamp();
        let mut changed = Vec::new();
        let mut source_queues = BTreeSet::new();
        for id in unique {
            if self.active.contains_key(&id) || self.browser_transfers.contains_key(&id) {
                bail!(
                    "{}",
                    crate::i18n::ui(
                        "Etkin iş kuyruklar arasında taşınmadan önce duraklatılmalı.",
                        "Active jobs must be paused before moving them between queues."
                    )
                );
            }
            let mut job = self.jobs.get(&id).cloned().context("İş bulunamadı")?;
            source_queues.insert(job_queue_id(&job).to_owned());
            job.request.queue_id = Some(queue_id.into());
            if !reconcile_job_scheduling_state_at(&self.settings, &mut job, &local) {
                job.updated_at = now;
            }
            changed.push(job);
        }
        self.store.save_jobs(&changed)?;
        for job in changed {
            self.jobs.insert(job.id.clone(), job);
        }
        for source_queue in source_queues {
            self.completion_armed.remove(&source_queue);
            if self
                .completion_countdown
                .as_ref()
                .is_some_and(|countdown| countdown.queue_id == source_queue)
            {
                self.completion_countdown = None;
            }
        }
        self.completion_armed.insert(queue_id.into());
        self.publish();
        Ok(())
    }

    fn update_queue_policy(&mut self, mut queue: QueuePolicy) -> Result<()> {
        let mut settings = self.settings.clone();
        let saved = settings
            .queues
            .iter_mut()
            .find(|saved| saved.id == queue.id)
            .context("Kuyruk bulunamadı")?;
        if let (Some(incoming), Some(authoritative)) = (&mut queue.quota, &saved.quota) {
            incoming.consumed_bytes = authoritative.consumed_bytes;
            incoming.period_key = authoritative.period_key.clone();
        }
        crate::validation::validate_queue(&queue)?;
        let completion_changed = saved.completion != queue.completion || !queue.enabled;
        let queue_id = saved.id.clone();
        *saved = queue;
        settings.settings_revision = settings.settings_revision.wrapping_add(1).max(1);
        validate_settings(&settings)?;
        let local = Local::now();
        refresh_queue_quota_periods_at(&mut settings, &local);
        let scheduling_changes = self
            .jobs
            .values()
            .filter(|job| !self.active.contains_key(&job.id))
            .filter_map(|job| {
                let mut updated = job.clone();
                reconcile_job_scheduling_state_at(&settings, &mut updated, &local)
                    .then_some(updated)
            })
            .collect::<Vec<_>>();
        self.store
            .save_jobs_and_settings(&scheduling_changes, &settings)?;
        if completion_changed
            && self
                .completion_countdown
                .as_ref()
                .is_some_and(|countdown| countdown.queue_id == queue_id)
        {
            self.completion_countdown = None;
            self.completion_armed.remove(&queue_id);
        }
        for job in scheduling_changes {
            self.jobs.insert(job.id.clone(), job);
        }
        self.settings = settings;
        self.publish();
        Ok(())
    }

    fn cancel_completion_action(&mut self, queue_id: &str) -> Result<()> {
        if self
            .completion_countdown
            .as_ref()
            .is_none_or(|countdown| countdown.queue_id != queue_id)
        {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Bu kuyruk için etkin bitiş geri sayımı yok.",
                    "There is no active completion countdown for this queue."
                )
            );
        }
        self.completion_countdown = None;
        self.completion_armed.remove(queue_id);
        self.completion_events.push(CompletionEvent {
            queue_id: queue_id.into(),
            occurred_at: Utc::now().timestamp(),
            message: "Bitiş eylemi kullanıcı tarafından iptal edildi.".into(),
        });
        self.trim_completion_events();
        self.publish();
        Ok(())
    }

    fn begin_browser_transfer(
        &mut self,
        id: &str,
        sender_pid: u32,
        sender_started_at: u64,
    ) -> Result<(Job, u64)> {
        if !self.settings.experimental_browser_transfer {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Deneysel tarayıcı aktarımı bu profilde açık değil.",
                    "Experimental browser transfer is not enabled for this profile."
                )
            );
        }
        let mut job = self.jobs.get(id).cloned().context("İş bulunamadı")?;
        if job.state == JobState::Completed
            || job.remove_requested.is_some()
            || self.active.contains_key(id)
            || self.browser_transfers.contains_key(id)
        {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Bu iş tarayıcı aktarımı için ayrılamaz.",
                    "This job cannot be set aside for a browser transfer."
                )
            );
        }
        if !job.browser_transfer_authorized
            && !self
                .source_refresh
                .get(id)
                .is_some_and(|grant| grant.expires_at > Utc::now().timestamp())
        {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Tarayıcı aktarımı önce masaüstünde bu iş için yetkilendirilmeli.",
                    "Browser transfer must be authorized on the desktop for this job first."
                )
            );
        }
        let queue_id = job_queue_id(&job).to_owned();
        let local = Local::now();
        if job_scheduling_state_at(&self.settings, &job.request, &local) != JobState::Queued {
            bail!(
                "{}",
                crate::i18n::ui(
                    "İşin başlangıç zamanı, günlük pencere veya kuyruk politikası henüz çalışmaya izin vermiyor.",
                    "The job's start time, daily window, or queue policy does not allow it to run yet."
                )
            );
        }
        let queue_limit = self
            .settings
            .queues
            .iter()
            .find(|queue| queue.id == queue_id)
            .map(|queue| queue.concurrency.max(1) as usize)
            .unwrap_or(1);
        if self.active.len() + self.browser_transfers.len()
            >= self.settings.max_active.max(1) as usize
            || self
                .active_queue_counts()
                .get(&queue_id)
                .copied()
                .unwrap_or(0)
                >= queue_limit
        {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Etkin iş veya kuyruk eşzamanlılık sınırı dolu.",
                    "The active-job or queue concurrency limit is reached."
                )
            );
        }
        let host = job_host(&job.request.url);
        if self.active_host_counts().get(&host).copied().unwrap_or(0)
            >= self.settings.per_host_limit.max(1) as usize
        {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Bu sunucu için etkin iş sınırı dolu.",
                    "The active-job limit for this server is reached."
                )
            );
        }
        let sender = BrowserTransferSender::open(sender_pid, sender_started_at)?;
        let acknowledged = if job.browser_transfer_authorized {
            job.downloaded
        } else {
            0
        };
        job.browser_transfer_authorized = true;
        job.downloaded = 0;
        job.state = JobState::Downloading;
        job.phase = crate::i18n::ui(
            "Yetkili tarayıcı aktarımı bekleniyor",
            "Waiting for authorized browser transfer",
        )
        .into();
        job.error = None;
        job.updated_at = Utc::now().timestamp();
        self.store.save_job(&job)?;
        self.jobs.insert(id.into(), job.clone());
        self.cancel_source_refresh_workers(id);
        self.source_refresh.remove(id);
        let generation = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        self.browser_transfers.insert(
            id.into(),
            BrowserTransferLease {
                generation,
                sender,
                requests: BTreeMap::new(),
                ended_requests: VecDeque::new(),
                sender_exited: false,
            },
        );
        self.quota_accounted.insert(id.into(), acknowledged);
        self.completion_armed.insert(queue_id);
        self.publish();
        Ok((job, generation))
    }

    fn browser_transfer_job(&mut self, id: &str) -> Result<(Job, u64)> {
        if !self.browser_transfers.contains_key(id) {
            bail!(
                "{}",
                crate::i18n::ui(
                    "İş için etkin tarayıcı aktarımı yok.",
                    "There is no active browser transfer for this job."
                )
            );
        }
        let job = self.jobs.get(id).cloned().context("İş bulunamadı")?;
        if job.remove_requested.is_some()
            || job.state == JobState::Completed
            || self.active.contains_key(id)
        {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Tarayıcı aktarımı artık bu işe yazamaz.",
                    "The browser transfer can no longer write to this job."
                )
            );
        }
        Ok((job, self.browser_transfers[id].generation))
    }

    fn try_begin_browser_request(
        &mut self,
        id: &str,
        generation: u64,
        request_id: &str,
    ) -> Result<bool> {
        if generation == 0 {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Tarayıcı aktarım nesli geçersiz",
                    "Invalid browser transfer generation"
                )
            );
        }
        validate_browser_request_id(request_id)?;
        let job = self.jobs.get(id).context("İş bulunamadı")?;
        if job.remove_requested.is_some()
            || job.state == JobState::Completed
            || self.active.contains_key(id)
        {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Tarayıcı isteği artık bu işe ait değil.",
                    "The browser request no longer belongs to this job."
                )
            );
        }
        let transfer = self
            .browser_transfers
            .get_mut(id)
            .filter(|transfer| transfer.generation == generation)
            .context("İş için etkin tarayıcı aktarımı yok.")?;
        if transfer.sender_exited || !transfer.sender.is_alive() {
            transfer.sender_exited = true;
            bail!(
                "{}",
                crate::i18n::ui(
                    "Tarayıcı aktarım göndericisi kapandı; yeni istek yetkisi verilemez.",
                    "The browser transfer sender exited; a new request cannot be authorized."
                )
            );
        }
        if transfer.requests.contains_key(request_id) {
            return Ok(true);
        }
        if transfer
            .ended_requests
            .iter()
            .any(|known| known == request_id)
        {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Tarayıcı istek kimliği yeniden kullanılamaz.",
                    "The browser request id cannot be reused."
                )
            );
        }
        let Some(lease) = self.network.try_acquire_wildcard() else {
            return Ok(false);
        };
        transfer.requests.insert(request_id.to_owned(), lease);
        Ok(true)
    }

    fn end_browser_request(&mut self, id: &str, generation: u64, request_id: &str) -> Result<()> {
        if generation == 0 {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Tarayıcı aktarım nesli geçersiz",
                    "Invalid browser transfer generation"
                )
            );
        }
        validate_browser_request_id(request_id)?;
        let transfer = self
            .browser_transfers
            .get_mut(id)
            .filter(|transfer| transfer.generation == generation)
            .context("İş için etkin tarayıcı aktarımı yok.")?;
        if let Some(lease) = transfer.requests.remove(request_id) {
            // Dropping only after the browser reports body consumption/cancel
            // closes this conservative wildcard reservation.
            drop(lease);
            transfer.ended_requests.push_back(request_id.to_owned());
            while transfer.ended_requests.len() > 1024 {
                transfer.ended_requests.pop_front();
            }
        }
        // A lost native-messaging response can retry an already-ended request,
        // and an uncertain Begin can be ended safely. Generation validation above
        // prevents either case from releasing a newer transfer's capacity.
        Ok(())
    }

    fn browser_transfer_progress(
        &mut self,
        id: &str,
        downloaded: u64,
        total: Option<u64>,
    ) -> Result<()> {
        if !self.browser_transfers.contains_key(id) {
            bail!(
                "{}",
                crate::i18n::ui(
                    "İş için etkin tarayıcı aktarımı yok.",
                    "There is no active browser transfer for this job."
                )
            );
        }
        let mut job = self.jobs.get(id).cloned().context("İş bulunamadı")?;
        if total.is_some_and(|total| downloaded > total) || downloaded < job.downloaded {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Tarayıcı aktarım ilerlemesi geriye gidemez veya toplamı aşamaz.",
                    "Browser transfer progress cannot go backwards or exceed the total."
                )
            );
        }
        let queue_id = job_queue_id(&job).to_owned();
        let local = Local::now();
        let accounted = self
            .quota_accounted
            .get(id)
            .copied()
            .unwrap_or(job.downloaded);
        consume_queue_bytes_at(
            &mut self.settings,
            &queue_id,
            downloaded.saturating_sub(accounted),
            &local,
        );
        job.downloaded = downloaded;
        job.total = total;
        job.speed = 0;
        job.eta = None;
        let queue_limit = self
            .settings
            .queues
            .iter()
            .find(|queue| queue.id == queue_id)
            .map(|queue| queue.concurrency.max(1) as usize)
            .unwrap_or(1);
        let host = job_host(&job.request.url);
        let transfer_complete = total == Some(downloaded) && downloaded > 0;
        let accepts_more = transfer_complete
            || job_scheduling_state_at(&self.settings, &job.request, &local) == JobState::Queued
                && self.active.len() + self.browser_transfers.len()
                    <= self.settings.max_active.max(1) as usize
                && self
                    .active_queue_counts()
                    .get(&queue_id)
                    .copied()
                    .unwrap_or(0)
                    <= queue_limit
                && self.active_host_counts().get(&host).copied().unwrap_or(0)
                    <= self.settings.per_host_limit.max(1) as usize;
        job.phase = if accepts_more {
            "Tarayıcıdan güvenli aktarım alınıyor".into()
        } else {
            "Tarayıcı aktarımı kuyruk kotası, zaman veya eşzamanlılık sınırında durdu".into()
        };
        if !accepts_more {
            job.state = job_scheduling_state_at(&self.settings, &job.request, &local);
            job.speed = 0;
            job.eta = None;
        }
        job.updated_at = Utc::now().timestamp();
        self.store.save_job_and_settings(&job, &self.settings)?;
        self.quota_accounted.insert(id.into(), downloaded);
        self.jobs.insert(id.into(), job);
        if !accepts_more {
            self.remove_browser_transfer(id);
        }
        self.publish();
        if !accepts_more {
            crate::bail_code!(crate::error_codes::TRF_013);
        }
        Ok(())
    }

    fn remove_browser_transfer(&mut self, id: &str) -> Option<BrowserTransferLease> {
        let mut transfer = self.browser_transfers.remove(id)?;
        let retained = transfer.requests.len();
        self.browser_orphans
            .extend(std::mem::take(&mut transfer.requests).into_values());
        if retained > 0 {
            self.warning = Some(format!(
                "Tarayıcı isteği kapanmadan iş durdu; {retained} ağ izni Chrome'un durduğu doğrulanana kadar güvenli olarak tutuluyor"
            ));
        }
        Some(transfer)
    }

    fn revoke_exited_browser_transfer(&mut self, id: &str, generation: u64) -> Result<()> {
        if self
            .browser_transfers
            .get(id)
            .is_none_or(|transfer| transfer.generation != generation)
        {
            bail!(
                "{}",
                crate::i18n::ui(
                    "İş için etkin tarayıcı aktarımı yok.",
                    "There is no active browser transfer for this job."
                )
            );
        }
        self.remove_browser_transfer(id);
        let job = self.jobs.get_mut(id).context("İş bulunamadı")?;
        // With no granted request there is no Fetch body whose authorization
        // must survive the native sender. Revoke this generation instead of
        // leaving a later native connection able to revive it implicitly.
        job.browser_transfer_authorized = false;
        job.state = JobState::Paused;
        job.speed = 0;
        job.eta = None;
        job.phase = "Tarayıcı aktarım göndericisi kapandı; yetki geri alındı".into();
        job.updated_at = Utc::now().timestamp();
        self.store.save_job(job)?;
        self.publish();
        Ok(())
    }

    fn pause_browser_transfer(&mut self, id: &str, generation: u64) -> Result<()> {
        if self
            .browser_transfers
            .get(id)
            .is_none_or(|transfer| transfer.generation != generation)
        {
            bail!(
                "{}",
                crate::i18n::ui(
                    "İş için etkin tarayıcı aktarımı yok.",
                    "There is no active browser transfer for this job."
                )
            );
        }
        self.remove_browser_transfer(id);
        let job = self.jobs.get_mut(id).context("İş bulunamadı")?;
        job.state = JobState::Paused;
        job.speed = 0;
        job.eta = None;
        job.phase = "Tarayıcı aktarımı duraklatıldı".into();
        job.updated_at = Utc::now().timestamp();
        self.store.save_job(job)?;
        self.publish();
        Ok(())
    }

    fn release_browser_transfer(&mut self, id: &str, generation: u64) -> Result<()> {
        if self
            .browser_transfers
            .get(id)
            .is_none_or(|transfer| transfer.generation != generation)
        {
            return Ok(());
        }
        self.remove_browser_transfer(id);
        let job = self.jobs.get_mut(id).context("İş bulunamadı")?;
        job.state = JobState::Paused;
        job.speed = 0;
        job.eta = None;
        job.phase = crate::i18n::ui(
            "Tarayıcı bağlantısı koptu; açıkça sürdürmeniz gerekiyor",
            "The browser connection was lost; resume it explicitly",
        )
        .into();
        job.updated_at = Utc::now().timestamp();
        let saved = self.store.save_job(job);
        self.publish();
        saved?;
        Ok(())
    }

    fn finish_browser_transfer(
        &mut self,
        id: &str,
        generation: u64,
        path: PathBuf,
        bytes: u64,
    ) -> Result<()> {
        if self
            .browser_transfers
            .get(id)
            .is_none_or(|transfer| transfer.generation != generation)
        {
            bail!(
                "{}",
                crate::i18n::ui(
                    "İş için yetkili tarayıcı aktarımı yok.",
                    "There is no authorized browser transfer for this job."
                )
            );
        }
        let job = self.jobs.get(id).cloned().context("İş bulunamadı")?;
        let work = job.work_dir.as_ref().context("İş çalışma klasörü eksik")?;
        let canonical_work =
            std::fs::canonicalize(work).context("İş çalışma klasörü bulunamadı")?;
        let canonical_path =
            std::fs::canonicalize(&path).context("Tarayıcı aktarım dosyası bulunamadı")?;
        if !canonical_path.starts_with(&canonical_work) || !canonical_path.is_file() {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Tarayıcı aktarımı yalnız işe ait çalışma dosyasını tamamlayabilir.",
                    "Browser transfer can only complete the job's own work file."
                )
            );
        }
        let actual = std::fs::metadata(&canonical_path)?.len();
        if actual != bytes || bytes == 0 {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Tarayıcı aktarımı boyutu doğrulanamadı.",
                    "The browser transfer size could not be verified."
                )
            );
        }
        let published = crate::recovery::publish(
            &self.paths,
            &job,
            TransferResult {
                path: canonical_path,
                bytes,
            },
        )?;
        self.remove_browser_transfer(id);
        self.finish_job(id, Ok(published));
        self.publish();
        Ok(())
    }

    fn run_synchronization(&mut self, id: &str) -> Result<()> {
        let policy = self
            .settings
            .synchronization_policies
            .iter()
            .find(|policy| policy.id == id)
            .cloned()
            .context("Senkronizasyon bulunamadı")?;
        self.start_synchronization(policy)
    }

    fn schedule_synchronizations(&mut self, _force: bool) {
        let now = Utc::now().timestamp();
        let due = self
            .settings
            .synchronization_policies
            .iter()
            .filter(|policy| {
                policy.enabled
                    && !self.synchronization_running.contains(&policy.id)
                    && policy.last_checked_at.is_none_or(|checked| {
                        now.saturating_sub(checked) >= i64::from(policy.interval_minutes) * 60
                    })
            })
            .cloned()
            .collect::<Vec<_>>();
        for policy in due {
            if let Err(error) = self.start_synchronization(policy) {
                self.warning = Some(error.to_string());
            }
        }
    }

    fn start_synchronization(&mut self, policy: SyncPolicy) -> Result<()> {
        if self.synchronization_running.contains(&policy.id) {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Senkronizasyon zaten çalışıyor.",
                    "Synchronization is already running."
                )
            );
        }
        crate::validation::validate_url(&policy.url, true)?;
        let id = policy.id.clone();
        let worker_id = id.clone();
        let sender = self.sender.clone();
        let network = self.network.clone();
        thread::Builder::new()
            .name(format!("ssdownload-sync-{}", &id[..id.len().min(8)]))
            .spawn(move || {
                let result = perform_synchronization(&policy, &network)
                    .map_err(|error| format!("{error:#}"));
                let _ = sender.send(Message::Worker(WorkerEvent::SynchronizationFinished {
                    id: worker_id,
                    result,
                }));
            })
            .context("Senkronizasyon işçisi başlatılamadı")?;
        self.synchronization_running.insert(id);
        Ok(())
    }

    fn refresh_quota_periods(&mut self, now: &DateTime<Local>) -> bool {
        let changed = refresh_queue_quota_periods_at(&mut self.settings, now);
        if changed {
            self.settings.settings_revision =
                self.settings.settings_revision.wrapping_add(1).max(1);
        }
        changed
    }

    fn maybe_begin_completion(&mut self, queue_id: &str) {
        if !self.completion_armed.contains(queue_id)
            || self.completion_countdown.is_some()
            || !queue_completed_successfully(&self.jobs, queue_id)
        {
            return;
        }
        let Some(queue) = self
            .settings
            .queues
            .iter()
            .find(|queue| queue.id == queue_id)
        else {
            return;
        };
        if !queue.enabled {
            self.completion_armed.remove(queue_id);
            return;
        }
        match &queue.completion {
            CompletionAction::None => {
                self.completion_armed.remove(queue_id);
            }
            CompletionAction::Notify => {
                self.completion_events.push(CompletionEvent {
                    queue_id: queue_id.into(),
                    occurred_at: Utc::now().timestamp(),
                    message: format!("{} kuyruğu tamamlandı.", queue.name),
                });
                self.completion_armed.remove(queue_id);
                self.trim_completion_events();
            }
            action @ (CompletionAction::ShutdownComputer { .. }
            | CompletionAction::RunProgram { .. }) => {
                // Destructive/external actions are global-idle only. A manually paused,
                // waiting-source, queued, or active job blocks the trigger.
                if !self.active.is_empty()
                    || !self.browser_transfers.is_empty()
                    || self
                        .jobs
                        .values()
                        .any(|job| job.state != JobState::Completed)
                {
                    return;
                }
                let seconds = match action {
                    CompletionAction::ShutdownComputer { countdown_seconds }
                    | CompletionAction::RunProgram {
                        countdown_seconds, ..
                    } => *countdown_seconds,
                    _ => unreachable!(),
                };
                self.completion_countdown = Some(CompletionCountdown {
                    queue_id: queue_id.into(),
                    deadline: Utc::now().timestamp() + i64::from(seconds),
                    action: action.clone(),
                });
            }
        }
    }

    fn process_completion_countdown(&mut self) {
        let Some(countdown) = self.completion_countdown.clone() else {
            return;
        };
        if !self.active.is_empty()
            || !self.browser_transfers.is_empty()
            || self
                .jobs
                .values()
                .any(|job| job.state != JobState::Completed)
        {
            self.completion_countdown = None;
            self.publish();
            return;
        }
        if Utc::now().timestamp() < countdown.deadline {
            return;
        }
        self.completion_countdown = None;
        self.completion_armed.remove(&countdown.queue_id);
        let outcome = match countdown.action {
            CompletionAction::ShutdownComputer { .. } => std::process::Command::new("shutdown.exe")
                .args(["/s", "/t", "0", "/d", "p:0:0"])
                .spawn()
                .map(|_| "Bilgisayarı kapatma isteği Windows'a gönderildi.".to_owned()),
            CompletionAction::RunProgram {
                program, arguments, ..
            } => std::process::Command::new(program)
                .args(arguments)
                .spawn()
                .map(|_| "Kuyruk bitiş programı başlatıldı.".to_owned()),
            _ => return,
        };
        self.completion_events.push(CompletionEvent {
            queue_id: countdown.queue_id,
            occurred_at: Utc::now().timestamp(),
            message: outcome.unwrap_or_else(|error| format!("Bitiş eylemi başlatılamadı: {error}")),
        });
        self.trim_completion_events();
        self.publish();
    }

    fn trim_completion_events(&mut self) {
        if self.completion_events.len() > 32 {
            self.completion_events
                .drain(..self.completion_events.len().saturating_sub(32));
        }
    }

    fn worker_event(&mut self, event: WorkerEvent) {
        match event {
            WorkerEvent::Progress {
                id,
                generation,
                value,
            } => {
                let Some(active) = self.active.get_mut(&id) else {
                    return;
                };
                if active.generation != generation || !matches!(active.desired, DesiredStop::None) {
                    return;
                }
                let Some(job) = self.jobs.get_mut(&id) else {
                    return;
                };
                let queue_id = job_queue_id(job).to_owned();
                let accounted = self
                    .quota_accounted
                    .get(&id)
                    .copied()
                    .unwrap_or(job.downloaded);
                let delta = value.downloaded.saturating_sub(accounted);
                let stage = value.phase.clone();
                let stage_changed = job.phase != stage;
                job.downloaded = value.downloaded;
                job.total = value.total;
                job.speed = value.speed;
                job.eta = value.eta;
                job.phase = stage.clone();
                job.state = if processing_phase(&job.phase) {
                    JobState::Processing
                } else {
                    JobState::Downloading
                };
                job.updated_at = Utc::now().timestamp();
                self.quota_accounted.insert(id.clone(), value.downloaded);
                consume_queue_bytes(&mut self.settings, &queue_id, delta);
                // Progress (and the quota bytes it consumed) is persisted at most once a
                // second per job: a crash can forget at most that last second, while every
                // progress event no longer costs a DPAPI seal and a synchronous commit.
                if active.last_persist.elapsed() >= Duration::from_secs(1) {
                    if let Err(error) = self.store.save_job_and_settings(job, &self.settings) {
                        self.persistence_error = Some(format!(
                            "Kuyruk kaydedilemedi: {error}. İndirmeler durduruldu."
                        ));
                    }
                    active.last_persist = Instant::now();
                }
                if stage_changed {
                    crate::logging::record_job(
                        &id,
                        crate::logging::Event::info("job.stage")
                            .host(source_host_of(job))
                            .detail(format!("{stage} toplam={:?}", value.total)),
                    );
                } else if active.last_log.elapsed() >= Duration::from_secs(15) {
                    active.last_log = Instant::now();
                    crate::logging::record_job(
                        &id,
                        crate::logging::Event::debug("job.progress")
                            .host(source_host_of(job))
                            .ms(active.attempt_started.elapsed().as_millis() as u64)
                            .detail(format!(
                                "indirilen={} toplam={:?} hız={} eta={:?}",
                                value.downloaded, value.total, value.speed, value.eta
                            )),
                    );
                }
                self.publish();
            }
            WorkerEvent::Finished {
                id,
                generation,
                result,
            } => {
                let Some(active) = self.active.remove(&id) else {
                    return;
                };
                if active.generation != generation {
                    self.active.insert(id, active);
                    return;
                }
                let retry_connection_cap = active
                    .direct_connections
                    .max(active.media_fragment_connections)
                    .max(1);
                let _ = active.handle.join();
                match active.desired {
                    DesiredStop::Remove { delete_file, .. } => {
                        if let Err(error) = self.finish_remove(&id, delete_file) {
                            self.warning = Some(error.to_string());
                        }
                    }
                    DesiredStop::Shutdown if result.is_ok() => self.finish_job(&id, result),
                    DesiredStop::Shutdown => {}
                    DesiredStop::Pause if result.is_ok() => self.finish_job(&id, result),
                    DesiredStop::Resume if result.is_ok() => self.finish_job(&id, result),
                    DesiredStop::Pause => {
                        if let Some(job) = self.jobs.get_mut(&id) {
                            job.state = JobState::Paused;
                            job.speed = 0;
                            job.eta = None;
                            job.updated_at = Utc::now().timestamp();
                            if let Err(error) = self.store.save_job(job) {
                                self.persistence_error = Some(format!(
                                    "Kuyruk kaydedilemedi: {error}. İndirmeler durduruldu."
                                ));
                            }
                        }
                    }
                    DesiredStop::Resume => {
                        if let Some(job) = self.jobs.get_mut(&id) {
                            let local = Local::now();
                            let state =
                                job_scheduling_state_at(&self.settings, &job.request, &local);
                            job.state = state;
                            job.speed = 0;
                            job.eta = None;
                            job.phase = state.label().into();
                            job.updated_at = local.timestamp();
                            if let Err(error) = self.store.save_job(job) {
                                self.persistence_error = Some(format!(
                                    "Kuyruk kaydedilemedi: {error}. İndirmeler durduruldu."
                                ));
                            }
                        }
                    }
                    DesiredStop::None => {
                        self.update_retry_connection_cap(&id, retry_connection_cap, &result);
                        self.finish_job(&id, result)
                    }
                }
                self.publish();
            }
            WorkerEvent::SourceRefreshPrepared { generation, result } => {
                let Some(mut active) = self.source_refresh_workers.remove(&generation) else {
                    return;
                };
                let outcome = match result {
                    Ok(prepared) => self.commit_source_refresh(generation, &active, prepared),
                    Err(error) => Err(anyhow!(error)),
                };
                let _ = active.handle.join();
                if let Some(reply) = active.reply.take() {
                    respond(reply, outcome);
                }
                let deferred_remove = self
                    .jobs
                    .get(&active.id)
                    .and_then(|job| job.remove_requested);
                let another_refresh = self
                    .source_refresh_workers
                    .values()
                    .any(|other| other.id == active.id);
                if let Some(delete_file) = deferred_remove.filter(|_| !another_refresh) {
                    if let Err(error) = self.finish_remove(&active.id, delete_file) {
                        self.warning = Some(error.to_string());
                    }
                }
            }
            WorkerEvent::SynchronizationFinished { id, result } => {
                self.synchronization_running.remove(&id);
                if let Some(policy) = self
                    .settings
                    .synchronization_policies
                    .iter_mut()
                    .find(|policy| policy.id == id)
                {
                    policy.last_checked_at = Some(Utc::now().timestamp());
                    match result {
                        Ok(outcome) => {
                            policy.etag = outcome.etag.or_else(|| policy.etag.clone());
                            policy.last_modified = outcome
                                .last_modified
                                .or_else(|| policy.last_modified.clone());
                            policy.content_sha256 = outcome
                                .content_sha256
                                .or_else(|| policy.content_sha256.clone());
                            if outcome.changed {
                                policy.version = policy.version.saturating_add(1);
                            }
                            policy.last_error = None;
                        }
                        Err(error) => policy.last_error = Some(error),
                    }
                    self.settings.settings_revision =
                        self.settings.settings_revision.wrapping_add(1).max(1);
                    if let Err(error) = self.store.save_settings(&self.settings) {
                        self.persistence_error = Some(error.to_string());
                    }
                }
                self.publish();
            }
        }
    }

    fn update_retry_connection_cap(
        &mut self,
        id: &str,
        attempted_connections: u8,
        result: &std::result::Result<TransferResult, TransferFailure>,
    ) {
        match result {
            Ok(_)
            | Err(TransferFailure {
                retryable: false, ..
            }) => {
                self.retry_connection_caps.remove(id);
            }
            Err(TransferFailure {
                retryable: true, ..
            }) => {
                let reduced = (attempted_connections / 2).max(1);
                self.retry_connection_caps
                    .entry(id.into())
                    .and_modify(|cap| *cap = (*cap).min(reduced))
                    .or_insert(reduced);
            }
        }
    }

    fn finish_job(
        &mut self,
        id: &str,
        result: std::result::Result<TransferResult, TransferFailure>,
    ) {
        let Some(mut job) = self.jobs.get(id).cloned() else {
            return;
        };
        let now = Utc::now().timestamp();
        let queue_id = job_queue_id(&job).to_owned();
        match result {
            Ok(result) => {
                let accounted = self
                    .quota_accounted
                    .get(id)
                    .copied()
                    .unwrap_or(job.downloaded);
                consume_queue_bytes(
                    &mut self.settings,
                    &queue_id,
                    result.bytes.saturating_sub(accounted),
                );
                self.quota_accounted.insert(id.into(), result.bytes);
                job.path = result.path;
                job.name = job
                    .path
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or(&job.name)
                    .to_owned();
                job.downloaded = result.bytes;
                job.total = Some(result.bytes);
                job.speed = 0;
                job.eta = Some(0);
                job.state = JobState::Completed;
                job.phase = crate::i18n::ui("Tamamlandı", "Completed").into();
                job.request.forget_session();
                job.error = install::mark_download(&job.path, &job.request.url)
                    .err()
                    .map(|error| {
                        crate::i18n::ui_owned!(
                            format!("Windows indirme işareti uygulanamadı: {error:#}"),
                            format!("Could not apply the Windows download mark: {error:#}")
                        )
                    });
                self.retry_at.remove(id);
                let elapsed = now.saturating_sub(job.created_at).max(0) as u64 * 1000;
                crate::logging::record_job(
                    id,
                    crate::logging::Event::info("job.done")
                        .host(source_host_of(&job))
                        .ms(elapsed)
                        .outcome(crate::logging::Outcome::Ok)
                        .detail(format!(
                            "dosya={} bayt={} deneme={}",
                            crate::logging::file_name(&job.path),
                            result.bytes,
                            job.attempts
                        )),
                );
                if let Some(warning) = &job.error {
                    crate::logging::record_job(
                        id,
                        crate::logging::Event::warn("job.mark_failed")
                            .host(source_host_of(&job))
                            .detail(warning.clone()),
                    );
                }
            }
            Err(failure) => {
                let retryable = failure.retryable;
                let retry_after = failure.retry_after;
                let message = failure.message;
                job.speed = 0;
                job.eta = None;
                job.error = Some(message.clone());
                if failure.source_refresh {
                    job.state = JobState::AwaitingSource;
                    job.phase = crate::i18n::ui(
                        "Kaynak adresinin yenilenmesi gerekiyor",
                        "The source URL needs to be refreshed",
                    )
                    .into();
                    self.retry_at.remove(id);
                    crate::logging::record_job(
                        id,
                        crate::logging::Event::warn("job.source_lost")
                            .host(source_host_of(&job))
                            .outcome(crate::logging::Outcome::Failed)
                            .detail(message.clone()),
                    );
                } else if retryable && job.attempts <= self.settings.retry_limit as u32 {
                    let delay = retry_after.unwrap_or_else(|| {
                        Duration::from_secs(
                            1u64.checked_shl(job.attempts.saturating_sub(1).min(6))
                                .unwrap_or(60)
                                .min(60),
                        )
                    });
                    if let Some(retry_at) = Instant::now().checked_add(delay) {
                        self.retry_at.insert(id.into(), retry_at);
                        job.state = JobState::Queued;
                        job.phase = crate::i18n::ui_owned!(
                            format!("{} saniye sonra yeniden denenecek", delay.as_secs()),
                            format!("Retrying in {} seconds", delay.as_secs())
                        );
                        crate::logging::record_job(
                            id,
                            failure_event(&job, &message)
                                .outcome(crate::logging::Outcome::Retry)
                                .detail(format!(
                                    "{message} ({} sn sonra, deneme {})",
                                    delay.as_secs(),
                                    job.attempts
                                )),
                        );
                    } else {
                        job.state = JobState::Failed;
                        job.phase = crate::i18n::ui(
                            "Sunucu yeniden deneme süresi desteklenen aralığı aşıyor",
                            "The server's retry delay exceeds the supported range",
                        )
                        .into();
                        self.retry_at.remove(id);
                    }
                } else {
                    job.state = JobState::Failed;
                    job.phase = crate::i18n::ui("Başarısız", "Failed").into();
                    self.retry_at.remove(id);
                    crate::logging::record_job(id, failure_event(&job, &message));
                }
            }
        }
        if job.state == JobState::Failed {
            self.retry_connection_caps.remove(id);
            self.completion_armed.remove(&queue_id);
            if self
                .completion_countdown
                .as_ref()
                .is_some_and(|countdown| countdown.queue_id == queue_id)
            {
                self.completion_countdown = None;
            }
        }
        job.updated_at = now;
        match self.store.save_job_and_settings(&job, &self.settings) {
            Ok(()) => {
                self.jobs.insert(id.into(), job.clone());
                if job.state == JobState::Completed {
                    if let Err(error) = crate::recovery::acknowledge(&self.paths, &job) {
                        job.error = Some(crate::i18n::ui_owned!(
                            format!("Geçici dosyalar temizlenemedi: {error}"),
                            format!("Temporary files could not be cleaned up: {error}")
                        ));
                        self.warning = job.error.clone();
                        self.jobs.insert(id.into(), job.clone());
                        if let Err(error) = self.store.save_job(&job) {
                            self.persistence_error = Some(error.to_string());
                        }
                    }
                    self.maybe_begin_completion(&queue_id);
                }
            }
            Err(error) => {
                job.state = JobState::Paused;
                job.phase = "Tamamlama kaydı bekliyor".into();
                self.jobs.insert(id.into(), job);
                self.persistence_error = Some(crate::i18n::ui_owned!(
                    format!("Kuyruk kaydedilemedi: {error}. Son dosya kurtarma kaydı korundu."),
                    format!("Queue could not be saved: {error}. Last recovery record was kept.")
                ));
            }
        }
    }

    fn persist_job(&mut self, id: &str) {
        if let Some(job) = self.jobs.get(id) {
            if let Err(error) = self.store.save_job(job) {
                self.persistence_error = Some(format!("Kuyruk kaydedilemedi: {error}"));
            }
        }
    }
    fn pause_exited_browser_senders(&mut self) {
        // Native-host death is not evidence that Chrome stopped a Fetch body.
        // Keep every granted wildcard permit until the authenticated background
        // explicitly ends it. A sender with no request grants has nothing that
        // Chrome could still be fetching, so revoke that generation immediately.
        let mut grantless = Vec::new();
        let mut newly_orphaned = 0usize;
        for (id, transfer) in &mut self.browser_transfers {
            if !transfer.sender_exited && !transfer.sender.is_alive() {
                transfer.sender_exited = true;
                if transfer.requests.is_empty() {
                    grantless.push((id.clone(), transfer.generation));
                } else {
                    newly_orphaned = newly_orphaned.saturating_add(transfer.requests.len());
                }
            }
        }
        for (id, generation) in grantless {
            if let Err(error) = self.revoke_exited_browser_transfer(&id, generation) {
                self.persistence_error = Some(format!(
                    "Kapanan tarayıcı aktarımı duraklatılamadı: {error}"
                ));
                self.publish();
            }
        }
        if newly_orphaned > 0 {
            self.warning = Some(format!(
                "Tarayıcı aktarım göndericisi kapandı; Chrome isteği doğrulanana kadar {newly_orphaned} ağ izni güvenli olarak tutuluyor"
            ));
        }
    }

    fn refresh_network_limit_pending(&mut self) -> bool {
        let pending = self.network.per_host_limit_pending();
        if pending == self.network_limit_pending {
            return false;
        }
        self.network_limit_pending = pending;
        true
    }

    fn schedule(&mut self) {
        self.pause_exited_browser_senders();
        if self.refresh_network_limit_pending() {
            self.publish();
        }
        self.process_completion_countdown();
        let local = Local::now();
        let now = local.timestamp();
        self.source_refresh
            .retain(|_, grant| grant.expires_at >= now);
        let quota_rolled = self.refresh_quota_periods(&local);
        self.schedule_synchronizations(false);
        if self.persistence_error.is_some() {
            for active in self.active.values_mut() {
                active.control.pause();
                if matches!(active.desired, DesiredStop::None) {
                    active.desired = DesiredStop::Pause;
                }
            }
            if self.last_recovery.elapsed() > Duration::from_secs(3) {
                self.last_recovery = Instant::now();
                let mut reconciled = self.jobs.clone();
                for job in reconciled
                    .values_mut()
                    .filter(|job| job.remove_requested.is_none())
                {
                    if let Ok(Some(done)) = crate::recovery::recovered(&self.paths, &job.id) {
                        job.path = done.path;
                        job.downloaded = done.bytes;
                        job.total = Some(done.bytes);
                        job.state = JobState::Completed;
                        job.phase = "Tamamlandı (kurtarıldı)".into();
                        job.request.forget_session();
                    }
                }
                if self
                    .store
                    .save_jobs(&reconciled.values().cloned().collect::<Vec<_>>())
                    .is_ok()
                {
                    self.jobs = reconciled;
                    self.persistence_error = None;
                    for job in self
                        .jobs
                        .values()
                        .filter(|job| job.state == JobState::Completed)
                    {
                        if let Err(error) = crate::recovery::acknowledge(&self.paths, job) {
                            self.warning = Some(error.to_string());
                        }
                    }
                }
            }
            self.publish();
            return;
        }
        self.prune_history();
        let schedule_open =
            daily_schedule_open_at(&self.settings, &local) && self.network_allows_work();
        let mut state_changes = Vec::new();
        for job in self.jobs.values_mut() {
            if !self.active.contains_key(&job.id)
                && reconcile_job_scheduling_state_at(&self.settings, job, &local)
            {
                state_changes.push(job.clone());
            }
        }
        if quota_rolled || !state_changes.is_empty() {
            if let Err(error) = self
                .store
                .save_jobs_and_settings(&state_changes, &self.settings)
            {
                self.persistence_error = Some(error.to_string());
            }
            self.publish();
        }
        if self.persistence_error.is_some() {
            return;
        }
        self.enforce_running_limits(&local);
        if self.persistence_error.is_some() {
            return;
        }
        // "Hemen başlat" jobs start now, outside every concurrency limit.
        let forced = self
            .jobs
            .values()
            .filter(|job| {
                job.force_start
                    && job.state == JobState::Queued
                    && !self.active.contains_key(&job.id)
            })
            .map(|job| job.id.clone())
            .collect::<Vec<_>>();
        for id in forced {
            if self.hold_for_disk_space(&id) {
                continue;
            }
            if let Err(error) = self.start_job(&id) {
                self.fail_start(&id, error);
            }
        }
        if !schedule_open {
            return;
        }

        let capacity = self.settings.max_active.max(1) as usize;
        while self.persistence_error.is_none()
            && self.active.len() + self.browser_transfers.len() < capacity
        {
            let host_counts = self.active_host_counts();
            let queue_counts = self.active_queue_counts();
            let queue_len = self.settings.queues.len();
            let mut selected = None;
            for offset in 0..queue_len {
                let index = (self.queue_cursor + offset) % queue_len;
                let queue = &self.settings.queues[index];
                if !queue_accepts_work(&self.settings, &queue.id, &local)
                    || queue_counts.get(&queue.id).copied().unwrap_or(0)
                        >= queue.concurrency.max(1) as usize
                {
                    continue;
                }
                let candidate = self
                    .jobs
                    .values()
                    .filter(|job| {
                        job.state == JobState::Queued
                            && !job.browser_transfer_authorized
                            && job.remove_requested.is_none()
                            && !self.active.contains_key(&job.id)
                            && !self.browser_transfers.contains_key(&job.id)
                            && job_queue_id(job) == queue.id
                    })
                    .filter(|job| job.request.start_at.map(|at| at <= now).unwrap_or(true))
                    .filter(|job| {
                        self.retry_at
                            .get(&job.id)
                            .map(|at| Instant::now() >= *at)
                            .unwrap_or(true)
                    })
                    .filter(|job| {
                        let host = job_host(&job.request.url);
                        host_counts.get(&host).copied().unwrap_or(0)
                            < self.settings.per_host_limit.max(1) as usize
                    })
                    .min_by_key(|job| (-job.priority, job.created_at, job.id.clone()))
                    .map(|job| job.id.clone());
                if let Some(id) = candidate {
                    self.queue_cursor = (index + 1) % queue_len.max(1);
                    selected = Some(id);
                    break;
                }
            }
            let Some(id) = selected else {
                break;
            };
            if self.hold_for_disk_space(&id) {
                continue;
            }
            if let Err(error) = self.start_job(&id) {
                self.fail_start(&id, error);
            }
        }
    }

    fn fail_start(&mut self, id: &str, error: anyhow::Error) {
        if let Some(job) = self.jobs.get_mut(id) {
            job.state = JobState::Failed;
            job.force_start = false;
            job.error = Some(safe_error(&job.request, &format!("{error:#}")));
            job.phase = crate::i18n::ui("Başlatılamadı", "Could not start").into();
            job.updated_at = Utc::now().timestamp();
            self.completion_armed.remove(job_queue_id(job));
            if let Err(error) = self.store.save_job(job) {
                self.persistence_error = Some(format!(
                    "Kuyruk kaydedilemedi: {error}. İndirmeler durduruldu."
                ));
            }
        }
        self.publish();
    }

    /// Pauses a job that would start on a destination without room for it: the known
    /// remaining size, or at least `MIN_FREE_SPACE` when the size is unknown.
    fn hold_for_disk_space(&mut self, id: &str) -> bool {
        let Some(job) = self.jobs.get(id) else {
            return false;
        };
        let remaining = job
            .total
            .map(|total| total.saturating_sub(job.downloaded))
            .unwrap_or(0)
            .max(MIN_FREE_SPACE);
        let Some(free) = free_space(&job.path) else {
            return false;
        };
        if free >= remaining {
            return false;
        }
        let message = crate::i18n::ui_owned!(
            format!(
                "Hedef diskte yeterli yer yok: {} MiB gerekli, {} MiB boş. Yer açıp sürdürün.",
                remaining.div_ceil(1024 * 1024),
                free / (1024 * 1024)
            ),
            format!(
                "Not enough space on the destination disk: {} MiB needed, {} MiB free. Free some space and resume.",
                remaining.div_ceil(1024 * 1024),
                free / (1024 * 1024)
            )
        );
        if let Some(job) = self.jobs.get_mut(id) {
            job.state = JobState::Paused;
            job.force_start = false;
            job.error = Some(message.clone());
            job.phase = crate::i18n::ui("Disk alanı yetersiz", "Not enough disk space").into();
            job.updated_at = Utc::now().timestamp();
            let job = job.clone();
            if let Err(error) = self.store.save_job(&job) {
                self.persistence_error = Some(error.to_string());
            }
        }
        self.warning = Some(message);
        self.publish();
        true
    }

    fn start_job(&mut self, id: &str) -> Result<()> {
        let allocation = {
            let request = &self
                .jobs
                .get(id)
                .ok_or_else(|| anyhow!("İş bulunamadı"))?
                .request;
            self.connection_allocation(id, request)
        };
        let job = self
            .jobs
            .get_mut(id)
            .ok_or_else(|| anyhow!("İş bulunamadı"))?;
        let forced = std::mem::take(&mut job.force_start);
        if job.work_dir.is_none() {
            let marker = crate::recovery::claim_path(&job.path);
            if std::fs::read_to_string(&marker).ok().as_deref() != Some(&job.id) {
                crate::recovery::claim(&job.path, &job.id)?;
            }
            job.claim = Some(job.path.clone());
            job.work_dir = Some(crate::recovery::work_dir(&job.path, &job.id));
        }
        let generation = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        let control = TransferControl::default();
        job.state = JobState::Connecting;
        job.phase = "Bağlanıyor".into();
        crate::logging::record_job(
            id,
            crate::logging::Event::info("job.stage")
                .host(source_host_of(job))
                .detail(format!(
                    "Bağlanıyor deneme={}",
                    job.attempts.saturating_add(1)
                )),
        );
        job.error = None;
        job.speed = 0;
        job.eta = None;
        job.attempts = job.attempts.saturating_add(1);
        job.updated_at = Utc::now().timestamp();
        self.store.save_job(job)?;
        let job_copy = job.clone();
        let output = job.path.clone();
        let paths = self.paths.clone();
        let network = self.network.clone();
        let thread_control = control.clone();
        let sender = self.sender.clone();
        let worker_id = id.to_owned();
        let handle = thread::Builder::new()
            .name(format!("ssdownload-{}", &id[..id.len().min(8)]))
            .spawn(move || {
                let progress_id = worker_id.clone();
                let mut progress = |value: TransferProgress| {
                    let _ = sender.try_send(Message::Worker(WorkerEvent::Progress {
                        id: progress_id.clone(),
                        generation,
                        value,
                    }));
                };
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_job(
                        &paths,
                        &job_copy,
                        &output,
                        &thread_control,
                        &network,
                        allocation,
                        &mut progress,
                    )
                }))
                .unwrap_or_else(|_| Err(anyhow!("İndirme işçisi beklenmedik şekilde durdu")))
                .map_err(|error| transfer_failure(&job_copy.request, error));
                let _ = sender.send(Message::Worker(WorkerEvent::Finished {
                    id: worker_id,
                    generation,
                    result,
                }));
            })
            .context("İndirme işçisi başlatılamadı")?;
        self.active.insert(
            id.into(),
            ActiveJob {
                direct_connections: allocation.direct,
                media_fragment_connections: allocation.media_fragments,
                generation,
                control,
                handle,
                desired: DesiredStop::None,
                last_persist: Instant::now(),
                last_log: Instant::now(),
                attempt_started: Instant::now(),
                forced,
            },
        );
        self.retry_at.remove(id);
        self.publish();
        Ok(())
    }

    fn enforce_running_limits(&mut self, local: &DateTime<Local>) {
        let now = local.timestamp();
        let network_open = !(self.settings.pause_on_metered && self.metered);
        let mut candidates = self
            .active
            .iter()
            .filter(|(_, active)| matches!(active.desired, DesiredStop::None) && !active.forced)
            .filter_map(|(id, _)| {
                self.jobs.get(id).map(|job| {
                    (
                        (-job.priority, job.created_at),
                        id.clone(),
                        job_host(&job.request.url),
                    )
                })
            })
            .collect::<Vec<_>>();
        candidates.sort_by_key(|(order, id, _)| (*order, id.clone()));
        let mut kept = self.browser_transfers.len();
        let mut hosts = HashMap::<String, usize>::new();
        let mut queues = HashMap::<String, usize>::new();
        for id in self.browser_transfers.keys() {
            if let Some(job) = self.jobs.get(id) {
                *hosts.entry(job_host(&job.request.url)).or_insert(0) += 1;
                *queues.entry(job_queue_id(job).to_owned()).or_insert(0) += 1;
            }
        }
        let mut stop = Vec::new();
        for (_, id, host) in candidates {
            let host_count = hosts.get(&host).copied().unwrap_or(0);
            let queue_id = self
                .jobs
                .get(&id)
                .map(job_queue_id)
                .unwrap_or(DEFAULT_QUEUE_ID);
            let queue_count = queues.get(queue_id).copied().unwrap_or(0);
            let queue_limit = self
                .settings
                .queues
                .iter()
                .find(|queue| queue.id == queue_id)
                .map(|queue| queue.concurrency.max(1) as usize)
                .unwrap_or(1);
            let allowed = network_open
                && daily_schedule_open_at(&self.settings, local)
                && queue_accepts_work(&self.settings, queue_id, local)
                && kept < self.settings.max_active.max(1) as usize
                && queue_count < queue_limit
                && host_count < self.settings.per_host_limit.max(1) as usize;
            if allowed {
                kept += 1;
                *hosts.entry(host).or_insert(0) += 1;
                *queues.entry(queue_id.to_owned()).or_insert(0) += 1;
            } else {
                stop.push(id);
            }
        }
        if stop.is_empty() {
            return;
        }
        let mut changed = Vec::new();
        for id in stop {
            if let Some(active) = self.active.get_mut(&id) {
                active.control.pause();
                active.desired = DesiredStop::Resume;
            }
            if let Some(job) = self.jobs.get_mut(&id) {
                let state = job_scheduling_state_at(&self.settings, &job.request, local);
                job.state = state;
                job.speed = 0;
                job.eta = None;
                job.phase = if state == JobState::Scheduled {
                    state.label().into()
                } else {
                    "Sınır nedeniyle sırada".into()
                };
                job.updated_at = now;
                changed.push(job.clone());
            }
        }
        if !changed.is_empty() {
            if let Err(error) = self.store.save_jobs(&changed) {
                self.persistence_error = Some(error.to_string());
            }
            self.publish();
        }
    }

    fn active_host_counts(&self) -> HashMap<String, usize> {
        let mut counts = HashMap::new();
        for (id, active) in &self.active {
            if !matches!(active.desired, DesiredStop::None) {
                continue;
            }
            if let Some(job) = self.jobs.get(id) {
                *counts.entry(job_host(&job.request.url)).or_insert(0) += 1;
            }
        }
        for id in self.browser_transfers.keys() {
            if let Some(job) = self.jobs.get(id) {
                *counts.entry(job_host(&job.request.url)).or_insert(0) += 1;
            }
        }
        counts
    }

    fn active_queue_counts(&self) -> HashMap<String, usize> {
        let mut counts = HashMap::new();
        for (id, active) in &self.active {
            if !matches!(active.desired, DesiredStop::None) {
                continue;
            }
            if let Some(job) = self.jobs.get(id) {
                *counts.entry(job_queue_id(job).to_owned()).or_insert(0) += 1;
            }
        }
        for id in self.browser_transfers.keys() {
            if let Some(job) = self.jobs.get(id) {
                *counts.entry(job_queue_id(job).to_owned()).or_insert(0) += 1;
            }
        }
        counts
    }

    fn connection_allocation(&self, id: &str, request: &AddRequest) -> ConnectionAllocation {
        let retry_cap = self.retry_connection_caps.get(id).copied();
        let direct =
            requested_connection_count(request.connections, self.settings.connections, retry_cap);
        let media_fragments = requested_connection_count(
            request.connections,
            self.settings.media_fragment_connections,
            retry_cap,
        );
        match request.kind {
            DownloadKind::File if !transfer::request_requires_media_pipeline(request) => {
                ConnectionAllocation {
                    direct,
                    media_fragments: 0,
                    usage_modes: self.settings.usage_modes,
                }
            }
            DownloadKind::Video | DownloadKind::Audio | DownloadKind::File => {
                ConnectionAllocation {
                    direct: 0,
                    media_fragments,
                    usage_modes: self.settings.usage_modes,
                }
            }
            // Classification occurs in the worker so actor command processing
            // never waits for a socket permit. The inactive ceiling is ignored.
            DownloadKind::Auto => ConnectionAllocation {
                direct,
                media_fragments,
                usage_modes: self.settings.usage_modes,
            },
        }
    }

    /// The global limit in force now: the scheduled window's limit while it is open,
    /// otherwise the ordinary one (both KiB/s, 0 = unlimited).
    fn effective_speed_limit_kib(&self) -> u64 {
        let settings = &self.settings;
        if settings.speed_schedule_enabled {
            let now = Local::now();
            let minute = now.hour() * 60 + now.minute();
            if let (Some(start), Some(end)) = (
                parse_minute(&settings.speed_schedule_start),
                parse_minute(&settings.speed_schedule_end),
            ) {
                if window_contains(start, end, minute) {
                    return settings.speed_schedule_kib;
                }
            }
        }
        settings.speed_limit_kib
    }

    /// Jobs with their own limit run at it (capped by the global limit when one is set);
    /// the remaining running jobs share the global limit equally.
    fn rebalance_speed_limits(&mut self) {
        let global = self.effective_speed_limit_kib().saturating_mul(1024);
        let own_limit = |id: &str| {
            self.jobs
                .get(id)
                .and_then(|job| job.request.speed_limit_kib)
                .filter(|kib| *kib > 0)
                .map(|kib| kib.saturating_mul(1024))
        };
        let shared = self
            .active
            .iter()
            .filter(|(id, job)| matches!(job.desired, DesiredStop::None) && own_limit(id).is_none())
            .count() as u64;
        let per_job = if global == 0 || shared == 0 {
            0
        } else {
            (global / shared).max(1)
        };
        for (id, active) in &self.active {
            let limit = if !matches!(active.desired, DesiredStop::None) {
                0
            } else if let Some(own) = own_limit(id) {
                if global == 0 {
                    own
                } else {
                    own.min(global)
                }
            } else {
                per_job
            };
            active.control.set_speed_limit(limit);
        }
        self.update_keep_awake();
    }

    /// Asks Windows to stay awake (the display may still sleep) while a download runs.
    /// The request belongs to the actor thread and is withdrawn when the last job stops.
    fn update_keep_awake(&mut self) {
        use windows_sys::Win32::System::Power::{
            SetThreadExecutionState, ES_CONTINUOUS, ES_SYSTEM_REQUIRED,
        };
        let wanted = self.settings.keep_awake && !self.active.is_empty() && !self.stopping;
        if wanted == self.keeping_awake {
            return;
        }
        self.keeping_awake = wanted;
        unsafe {
            SetThreadExecutionState(if wanted {
                ES_CONTINUOUS | ES_SYSTEM_REQUIRED
            } else {
                ES_CONTINUOUS
            });
        }
    }

    fn available_path(&self, desired: &Path) -> PathBuf {
        // One pass over the queue per call instead of one per numbered candidate.
        let parent_key = desired
            .parent()
            .map(|parent| parent.to_string_lossy().to_lowercase());
        let queued = self
            .jobs
            .values()
            .filter(|job| {
                job.path
                    .parent()
                    .map(|parent| parent.to_string_lossy().to_lowercase())
                    == parent_key
            })
            .map(|job| job.path.to_string_lossy().to_lowercase())
            .collect::<std::collections::HashSet<_>>();
        let occupied = |path: &Path| {
            queued.contains(&path.to_string_lossy().to_lowercase())
                || transfer::has_artifacts(path)
                || crate::recovery::claim_path(path).exists()
        };
        if !occupied(desired) {
            return desired.to_path_buf();
        }
        let parent = desired.parent().unwrap_or_else(|| Path::new("."));
        let stem = desired
            .file_stem()
            .and_then(|v| v.to_str())
            .unwrap_or("download");
        let extension = desired.extension().and_then(|v| v.to_str());
        for index in 1..10_000u32 {
            let name = extension
                .map(|ext| format!("{stem} ({index}).{ext}"))
                .unwrap_or_else(|| format!("{stem} ({index})"));
            let candidate = parent.join(name);
            if !occupied(&candidate) {
                return candidate;
            }
        }
        parent.join(format!("{stem}-{}", Uuid::new_v4()))
    }

    fn publish(&mut self) {
        if self.last_publish.elapsed() < Duration::from_millis(200) {
            self.dirty = true;
            return;
        }
        self.dirty = false;
        self.last_publish = Instant::now();
        self.revision = self.revision.wrapping_add(1);
        let mut value = make_snapshot(
            &self.jobs,
            &self.settings,
            self.completion_countdown.clone(),
            &self.completion_events,
        );
        value.revision = self.revision;
        let mut warnings = Vec::new();
        if let Some(warning) = &self.persistence_error {
            warnings.push(warning.clone());
        }
        if let Some(warning) = &self.warning {
            warnings.push(warning.clone());
        }
        if !self.browser_orphans.is_empty() {
            warnings.push(format!(
                "{} doğrulanmamış tarayıcı ağ izni kapasitede tutuluyor; Chrome'un isteği kapattığı kanıtlanmadan serbest bırakılmaz",
                self.browser_orphans.len()
            ));
        }
        if self.network_limit_pending {
            warnings.push(format!(
                "Sunucu başına {} bağlantı sınırı yeni bağlantılara uygulanıyor; açık aktarım soketleri kapanana kadar tam olarak yürürlükte değil",
                self.settings.per_host_limit
            ));
        }
        value.warning = (!warnings.is_empty()).then(|| warnings.join("\n"));
        match self.snapshot.write() {
            Ok(mut snapshot) => *snapshot = value,
            Err(poisoned) => *poisoned.into_inner() = value,
        }
    }

    fn begin_shutdown(&mut self) {
        self.stopping = true;
        for active in self.source_refresh_workers.values() {
            active.control.cancel();
        }
        let now = Utc::now().timestamp();
        let mut changed = Vec::new();
        for (id, active) in &mut self.active {
            active.control.cancel();
            if matches!(
                active.desired,
                DesiredStop::Pause | DesiredStop::Remove { .. }
            ) {
                continue;
            }
            active.desired = DesiredStop::Shutdown;
            if let Some(job) = self.jobs.get_mut(id) {
                job.state = JobState::Queued;
                job.speed = 0;
                job.eta = None;
                job.phase = "Uygulama yeniden açılınca devam edecek".into();
                job.updated_at = now;
                changed.push(job.clone());
            }
        }
        if !changed.is_empty() {
            if let Err(error) = self.store.save_jobs(&changed) {
                self.persistence_error = Some(error.to_string());
            }
            self.publish();
        }
    }

    fn finish_shutdown(&mut self) {
        let active = std::mem::take(&mut self.active);
        for (_, job) in active {
            job.control.cancel();
            let _ = job.handle.join();
        }
        let refreshes = std::mem::take(&mut self.source_refresh_workers);
        for (_, mut refresh) in refreshes {
            refresh.control.cancel();
            let _ = refresh.handle.join();
            if let Some(reply) = refresh.reply.take() {
                respond(reply, Err(anyhow!("İndirme motoru kapalı")));
            }
        }
        if let Err(error) = self.store.flush() {
            self.persistence_error = Some(error.to_string());
            self.publish();
        }
    }
}

fn run_job(
    paths: &AppPaths,
    job: &Job,
    output: &Path,
    control: &TransferControl,
    network: &NetworkGovernor,
    allocation: ConnectionAllocation,
    progress: &mut dyn FnMut(TransferProgress),
) -> Result<TransferResult> {
    let _original_lock = crate::output::OutputLock::acquire(output)?;
    if let Some(done) = crate::recovery::recovered(paths, &job.id)? {
        return Ok(done);
    }
    let scratch = if job.work_dir.is_some() {
        crate::recovery::prepare_work(job)?
    } else {
        output.to_owned()
    };
    if scratch != output {
        transfer::migrate_legacy_parts(output, &scratch)?;
    }
    let output = scratch.as_path();
    let _output_lock = crate::output::OutputLock::acquire(output)?;
    let mut request = job.request.clone();
    let (use_media, prepared) = match request.kind {
        DownloadKind::Video | DownloadKind::Audio => (true, None),
        DownloadKind::File => (transfer::request_requires_media_pipeline(&request), None),
        DownloadKind::Auto => transfer::prepare_auto(&request, control, network)?,
    };
    if request.kind == DownloadKind::Auto
        && ((!use_media && !allocation.usage_modes.file)
            || (use_media && !allocation.usage_modes.video))
    {
        bail!(
            "{}",
            crate::i18n::ui(
                "Otomatik kaynak, kullanım amacı ayarlarında izin verilmeyen bir indirme türü olarak çözümlendi",
                "The automatic source resolved to a download type that is not allowed by your usage purpose settings"
            )
        );
    }
    let result = if use_media {
        request.connections = Some(allocation.media_fragments);
        media::download(paths, &request, output, control, network, progress, &job.id)
    } else {
        request.connections = Some(allocation.direct);
        transfer::download(
            &request,
            output,
            control,
            network,
            allocation.direct,
            prepared,
            progress,
        )
    }?;
    if job.work_dir.is_some() {
        crate::recovery::publish(paths, job, result)
    } else {
        Ok(result)
    }
}

fn job_queue_id(job: &Job) -> &str {
    job.request
        .queue_id
        .as_deref()
        .filter(|id| !id.is_empty())
        .unwrap_or(DEFAULT_QUEUE_ID)
}

fn queue_completed_successfully(jobs: &BTreeMap<String, Job>, queue_id: &str) -> bool {
    let mut found = false;
    for job in jobs.values().filter(|job| job_queue_id(job) == queue_id) {
        found = true;
        if job.state != JobState::Completed {
            return false;
        }
    }
    found
}

fn matching_folder(rules: &[FolderRule], url: &Url, kind: DownloadKind) -> Option<PathBuf> {
    let host = url.host_str()?.trim_end_matches('.').to_ascii_lowercase();
    rules
        .iter()
        .filter(|rule| rule.enabled)
        .filter_map(|rule| {
            let rule_host = rule.host.trim().trim_end_matches('.').to_ascii_lowercase();
            let exact = host == rule_host;
            let subdomain = rule.include_subdomains
                && host
                    .strip_suffix(&rule_host)
                    .is_some_and(|prefix| prefix.ends_with('.'));
            let kind_matches =
                rule.kinds.is_empty() || (kind != DownloadKind::Auto && rule.kinds.contains(&kind));
            (kind_matches && (exact || subdomain)).then_some((
                rule.priority,
                exact,
                rule_host.len(),
                &rule.id,
                &rule.destination,
            ))
        })
        .max_by(|left, right| {
            (left.0, left.1, left.2, left.3).cmp(&(right.0, right.1, right.2, right.3))
        })
        .map(|(_, _, _, _, destination)| destination.clone())
}

fn uses_media_refresh(request: &AddRequest) -> bool {
    let source_path = Url::parse(&request.url)
        .ok()
        .map(|url| url.path().to_ascii_lowercase())
        .unwrap_or_default();
    matches!(request.kind, DownloadKind::Video | DownloadKind::Audio)
        || transfer::request_requires_media_pipeline(request)
        || source_path.ends_with(".m3u8")
        || source_path.ends_with(".mpd")
}

/// Whether a failed job should wait for a renewed source instead of failing. The decision
/// uses the typed failure (HTTP status, kept partial data) and the external tools' own
/// English diagnostics, never the localized sentence, so it is the same in every
/// interface language.
fn source_refresh_needed(request: &AddRequest, error: &anyhow::Error) -> bool {
    let has_refresh_page = request
        .source_identity
        .as_ref()
        .map(|identity| identity.page_url.as_str())
        .or(request.page_url.as_deref())
        .and_then(normalized_page_identity)
        .is_some();
    if !has_refresh_page {
        return false;
    }
    if transfer::preserves_partial(error)
        || transfer::http_status(error).is_some_and(|status| matches!(status, 401 | 403 | 410))
    {
        return true;
    }
    let message = format!("{error:#}").to_ascii_lowercase();
    [
        "http error 401",
        "http error: 401",
        "http error 403",
        "http error: 403",
        "http error 410",
        "http error: 410",
        "forbidden",
        "unauthorized",
        "signature expired",
        "signature has expired",
        "url expired",
        "token expired",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

fn make_snapshot(
    jobs: &BTreeMap<String, Job>,
    settings: &Settings,
    completion_countdown: Option<CompletionCountdown>,
    completion_events: &[CompletionEvent],
) -> EngineSnapshot {
    let mut jobs = jobs.values().cloned().collect::<Vec<_>>();
    // Queue order: explicit priority first (higher first), then creation order.
    jobs.sort_by(|left, right| {
        (-left.priority, left.created_at, &left.id).cmp(&(
            -right.priority,
            right.created_at,
            &right.id,
        ))
    });
    EngineSnapshot {
        revision: 0,
        warning: None,
        jobs: Arc::new(jobs),
        settings: settings.clone(),
        completion_countdown,
        completion_events: completion_events.to_vec(),
    }
}

fn respond<T>(reply: Reply<T>, result: Result<T>) {
    let _ = reply.send(result.map_err(|error| format!("{error:#}")));
}

/// Repairs exactly the fields that fail validation, leaving every valid choice (a working
/// schedule included) as the user set it.
fn repair_settings(settings: &mut Settings, paths: &AppPaths) {
    let defaults = Settings::default();
    settings.max_active = settings.max_active.clamp(1, 16);
    settings.connections = settings.connections.clamp(1, 16);
    settings.media_fragment_connections = settings.media_fragment_connections.clamp(1, 16);
    settings.per_host_limit = settings.per_host_limit.clamp(1, 16);
    settings.retry_limit = settings.retry_limit.min(20);
    settings.speed_limit_kib = settings.speed_limit_kib.min(u64::MAX / 1024);
    if settings.download_dir.as_os_str().is_empty() {
        settings.download_dir = paths.download_dir.clone();
    } else if !settings.download_dir.is_absolute() {
        settings.download_dir = paths.base_dir.join(&settings.download_dir);
    }
    if !matches!(settings.last_video_container.as_str(), "mp4" | "mkv") {
        settings.last_video_container = defaults.last_video_container.clone();
    }
    if settings
        .last_video_height
        .is_some_and(|height| height == 0 || height > 16384)
    {
        settings.last_video_height = None;
    }
    if crate::logging::Level::parse(&settings.logging_level).is_none() {
        settings.logging_level = defaults.logging_level.clone();
    }
    let valid_time = |value: &str| chrono::NaiveTime::parse_from_str(value, "%H:%M").is_ok();
    if !valid_time(&settings.schedule_start) || !valid_time(&settings.schedule_end) {
        settings.schedule_enabled = false;
        settings.schedule_start = defaults.schedule_start.clone();
        settings.schedule_end = defaults.schedule_end.clone();
    }
    if settings.usage_modes.validate().is_err() {
        settings.usage_modes = defaults.usage_modes;
    }
}

fn validate_settings(settings: &Settings) -> Result<()> {
    crate::validation::validate_settings(settings)
}

fn daily_schedule_open_at<Tz: TimeZone>(settings: &Settings, now: &DateTime<Tz>) -> bool {
    schedule_open_at(settings, now.hour() * 60 + now.minute())
}

fn schedule_open_at(settings: &Settings, current: u32) -> bool {
    if !settings.schedule_enabled {
        return true;
    }
    let (Some(start), Some(end)) = (
        parse_minute(&settings.schedule_start),
        parse_minute(&settings.schedule_end),
    ) else {
        return false;
    };
    window_contains(start, end, current)
}

/// Daily window membership in minutes: start inclusive, end exclusive, `start > end`
/// crosses midnight and equal endpoints mean the whole day.
fn window_contains(start: u32, end: u32, current: u32) -> bool {
    if start == end {
        return true;
    }
    if start < end {
        current >= start && current < end
    } else {
        current >= start || current < end
    }
}

fn requested_connection_count(requested: Option<u8>, default: u8, retry_cap: Option<u8>) -> u8 {
    requested
        .unwrap_or(default)
        .clamp(1, crate::validation::MAX_CONNECTIONS)
        .min(
            retry_cap
                .unwrap_or(crate::validation::MAX_CONNECTIONS)
                .max(1),
        )
}

fn parse_minute(value: &str) -> Option<u32> {
    let (hour, minute) = value.trim().split_once(':')?;
    let hour: u32 = hour.parse().ok()?;
    let minute: u32 = minute.parse().ok()?;
    (hour < 24 && minute < 60).then_some(hour * 60 + minute)
}

fn validate_request_headers(request: &AddRequest) -> Result<()> {
    for (name, value) in &request.headers {
        if name.trim().is_empty()
            || name.contains(['\r', '\n', ':'])
            || value.contains(['\r', '\n'])
        {
            bail!(
                "{}",
                crate::i18n::ui("Geçersiz HTTP başlığı", "Invalid HTTP header")
            );
        }
    }
    for value in [request.referer.as_deref(), request.page_url.as_deref()]
        .into_iter()
        .flatten()
    {
        let parsed = Url::parse(value).context("Sayfa adresi geçersiz")?;
        if !matches!(parsed.scheme(), "http" | "https") {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Sayfa adresi HTTP veya HTTPS olmalıdır",
                    "The page URL must be HTTP or HTTPS"
                )
            );
        }
    }
    Ok(())
}

fn validate_checksum_text(value: &str) -> Result<()> {
    let value = value
        .trim()
        .strip_prefix("sha256:")
        .unwrap_or(value.trim())
        .trim();
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!(
            "{}",
            crate::i18n::ui(
                "SHA-256 sağlama toplamı 64 onaltılık karakter olmalıdır",
                "The SHA-256 checksum must be 64 hexadecimal characters"
            )
        );
    }
    Ok(())
}

pub(crate) fn safe_filename(suggested: &str, id: &str) -> String {
    let leaf = suggested.rsplit(['/', '\\']).next().unwrap_or("");
    let mut value = leaf
        .chars()
        .map(|character| {
            if character < ' '
                || matches!(
                    character,
                    '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
                )
            {
                '_'
            } else {
                character
            }
        })
        .collect::<String>();
    value = value.trim_matches([' ', '.']).to_owned();
    const MAX_NAME_CHARS: usize = 180;
    if value.chars().count() > MAX_NAME_CHARS {
        // Shorten the stem, never the extension: a clipped `.mp4` would leave a file
        // Windows cannot open by type. An implausibly long "extension" is ordinary text.
        let (stem, extension) = match value.rsplit_once('.') {
            Some((stem, extension))
                if !stem.is_empty()
                    && (1..=16).contains(&extension.chars().count())
                    && extension.chars().all(char::is_alphanumeric) =>
            {
                (stem.to_owned(), Some(extension.to_owned()))
            }
            _ => (value.clone(), None),
        };
        let keep = MAX_NAME_CHARS - extension.as_ref().map_or(0, |ext| ext.chars().count() + 1);
        let stem = stem
            .chars()
            .take(keep)
            .collect::<String>()
            .trim_end_matches([' ', '.'])
            .to_owned();
        value = match extension {
            Some(extension) => format!("{stem}.{extension}"),
            None => stem,
        };
    }
    let base = value
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end_matches(' ')
        .to_uppercase();
    let reserved = matches!(
        base.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || ((base.starts_with("COM") || base.starts_with("LPT"))
        && base.chars().count() == 4
        && base
            .chars()
            .nth(3)
            .is_some_and(|digit| digit.is_ascii_digit() || matches!(digit, '¹' | '²' | '³')));
    if value.is_empty() || value == "." || value == ".." {
        format!("download-{}", &id[..id.len().min(8)])
    } else if reserved {
        format!("_{value}")
    } else {
        value
    }
}

fn validate_browser_request_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 256
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        bail!(
            "{}",
            crate::i18n::ui(
                "Tarayıcı ağ istek kimliği geçersiz",
                "Invalid browser network request id"
            )
        );
    }
    Ok(())
}

fn job_host(value: &str) -> String {
    Url::parse(value)
        .ok()
        .and_then(|url| crate::network::canonical_host(&url).ok())
        .unwrap_or_default()
}

fn processing_phase(phase: &str) -> bool {
    let phase = phase.to_lowercase();
    phase.contains("işlen")
        || phase.contains("birleştir")
        || phase.contains("processing")
        || phase.contains("mux")
}

fn safe_error(request: &AddRequest, message: &str) -> String {
    let mut value = message.replace(&request.url, "[kaynak adresi]");
    for secret in request
        .headers
        .values()
        .chain(request.session_cookies.iter().map(|cookie| &cookie.value))
    {
        if !secret.is_empty() {
            value = value.replace(secret, "[gizli]");
        }
    }
    if let Some(value_to_hide) = request.referer.as_deref() {
        value = value.replace(value_to_hide, "[yönlendiren]");
    }
    // The head carries the yt-dlp argument list (which selection was requested) and the tail the
    // traceback, so both get room: an earlier 300/650 split dropped exactly the part that says
    // why a download could not be satisfied.
    if value.chars().nth(3400).is_some() {
        let head_end = value
            .char_indices()
            .nth(1800)
            .map_or(value.len(), |(index, _)| index);
        let tail_start = value
            .char_indices()
            .rev()
            .nth(1199)
            .map_or(0, |(index, _)| index);
        value = format!(
            "{}\n[... hata ayrıntısı kısaltıldı ...]\n{}",
            &value[..head_end],
            &value[tail_start..]
        );
    }
    value
}

/// Least free space a job with an unknown size needs to start.
const MIN_FREE_SPACE: u64 = 256 * 1024 * 1024;

/// Free bytes on the volume holding `path` (its folder), when Windows can tell.
fn free_space(path: &Path) -> Option<u64> {
    let folder = path.parent()?;
    let wide = crate::winpath::wide_long(folder);
    let mut available = 0u64;
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(available)
}

/// Windows' connectivity hint: a fixed or variable cost means a metered connection.
fn connection_is_metered() -> bool {
    use windows_sys::Win32::NetworkManagement::IpHelper::GetNetworkConnectivityHint;
    use windows_sys::Win32::Networking::WinSock::{
        NetworkConnectivityCostHintFixed, NetworkConnectivityCostHintVariable,
        NL_NETWORK_CONNECTIVITY_HINT,
    };
    let mut hint: NL_NETWORK_CONNECTIVITY_HINT = unsafe { std::mem::zeroed() };
    if unsafe { GetNetworkConnectivityHint(&mut hint) } != 0 {
        return false;
    }
    hint.ConnectivityCost == NetworkConnectivityCostHintFixed
        || hint.ConnectivityCost == NetworkConnectivityCostHintVariable
}

#[cfg(test)]
mod tests;

#[derive(Debug)]
struct TransferFailure {
    message: String,
    retryable: bool,
    retry_after: Option<Duration>,
    source_refresh: bool,
}

fn transfer_failure(request: &AddRequest, error: anyhow::Error) -> TransferFailure {
    let detail = format!("{error:#}");
    TransferFailure {
        retryable: retryable_error(&error),
        retry_after: transfer::retry_after(&error),
        source_refresh: source_refresh_needed(request, &error),
        message: safe_error(request, &detail),
    }
}

fn retryable_error(error: &anyhow::Error) -> bool {
    if transfer::http_status(error)
        .is_some_and(|status| matches!(status, 408 | 429 | 500 | 502 | 503 | 504))
    {
        return true;
    }
    for cause in error.chain() {
        if let Some(error) = cause.downcast_ref::<std::io::Error>() {
            if matches!(
                error.kind(),
                std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::Interrupted
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::ConnectionRefused
            ) {
                return true;
            }
        }
        if let Some(error) = cause.downcast_ref::<curl::Error>() {
            return matches!(error.code(), 5 | 6 | 7 | 18 | 28 | 52 | 55 | 56 | 92);
        }
    }
    let message = format!("{error:#}").to_lowercase();
    [
        "http 408",
        "http 429",
        "http 500",
        "http 502",
        "http 503",
        "http 504",
        // yt-dlp fragment and webpage failures surface as "HTTP Error <code>"
        // after exhausting its own retries; give them a job-level retry.
        "http error 408",
        "http error 429",
        "http error 500",
        "http error 502",
        "http error 503",
        "http error 504",
        // Local proxy saturation and tunnel failures surface verbatim from
        // the child's urllib stack; they are transient by construction.
        "tunnel connection failed",
        "unable to connect to proxy",
        "503 service unavailable",
        "timed out",
        "temporary failure",
        "connection reset",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}
