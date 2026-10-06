use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, AtomicU8, Ordering},
        Arc,
    },
};

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DownloadKind {
    #[default]
    Auto,
    File,
    Video,
    Audio,
}

pub const CAPABILITY_VERSION: u32 = 3;
pub const ONBOARDING_VERSION: u32 = 1;
pub const DEFAULT_QUEUE_ID: &str = "default";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct UsageModes {
    pub video: bool,
    pub file: bool,
    pub audio: bool,
}
impl Default for UsageModes {
    fn default() -> Self {
        Self {
            video: true,
            file: true,
            audio: true,
        }
    }
}
impl UsageModes {
    pub fn validate(self) -> std::result::Result<(), &'static str> {
        if self.video || self.file || self.audio {
            Ok(())
        } else {
            Err(crate::i18n::ui(
                "En az bir kullanım amacı seçin.",
                "Select at least one usage purpose.",
            ))
        }
    }

    /// Checks public, newly-created work only. Video's internal audio and existing
    /// accepted jobs intentionally do not pass through this gate.
    pub fn allows(self, kind: DownloadKind) -> bool {
        match kind {
            DownloadKind::Auto => self.video || self.file,
            DownloadKind::File => self.file,
            DownloadKind::Video => self.video,
            DownloadKind::Audio => self.audio,
        }
    }

    pub fn allows_inspection(self) -> bool {
        self.video || self.audio
    }
}

/// Desktop surface depth, chosen on the first run and switchable from Settings.
/// `Simple` keeps the download flow only; `Advanced` shows the full window and
/// menu set. Stored as a tolerant string so an unreadable value can never
/// quarantine the whole settings record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiMode {
    Simple,
    Advanced,
}

impl UiMode {
    /// Anything that is not exactly `simple` reads as `advanced`, matching the
    /// pre-existing surface a stored profile already shows.
    pub fn parse(value: &str) -> Self {
        if value.trim().eq_ignore_ascii_case("simple") {
            Self::Simple
        } else {
            Self::Advanced
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Simple => "simple",
            Self::Advanced => "advanced",
        }
    }

    pub fn is_simple(self) -> bool {
        self == Self::Simple
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ScopedCookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    pub secure: bool,
    pub http_only: bool,
    pub host_only: bool,
    pub expires: Option<f64>,
    pub store_id: Option<String>,
    pub partition_key: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct SourceIdentity {
    pub video_id: String,
    pub frame_id: i64,
    pub document_id: Option<String>,
    pub page_url: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct AudioSelection {
    pub id: String,
    pub language: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct SubtitleSelection {
    pub language: String,
    pub automatic: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ExternalSubtitle {
    pub url: String,
    pub language: String,
    pub label: String,
    pub kind: String,
    pub is_default: bool,
    pub headers: BTreeMap<String, String>,
    pub referer: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AddRequest {
    pub url: String,
    pub kind: DownloadKind,
    pub filename: Option<String>,
    pub directory: Option<PathBuf>,
    pub queue_id: Option<String>,
    pub headers: BTreeMap<String, String>,
    pub session_cookies: Vec<ScopedCookie>,
    pub referer: Option<String>,
    /// The page this download was handed off from: cookie/source context only, never an
    /// HTTP Referer. `referer` keeps the network value exactly as the browser observed it.
    pub page_url: Option<String>,
    /// Page markup the browser captured at handoff time, already bounded by the
    /// extension. It can carry session-bound markup, so it stays inside the
    /// profile: the job record seals it like the rest of the job, no
    /// browser-facing response carries it (`public_snapshot`) and neither the
    /// log nor the events dialog shows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_html: Option<String>,
    pub source_identity: Option<SourceIdentity>,
    pub format_id: Option<String>,
    pub video_format_id: Option<String>,
    pub container: Option<String>,
    pub max_height: Option<u32>,
    pub exact_height: Option<u32>,
    pub audio_format_id: Option<String>,
    pub audio_language: Option<String>,
    pub audio_tracks: Vec<AudioSelection>,
    pub subtitle_mode: Option<String>,
    pub external_subtitles: Vec<ExternalSubtitle>,
    pub request_id: Option<String>,
    pub audio_format: Option<String>,
    /// An earlier inspection observed audio; later source changes must not silently remove it.
    pub expected_audio: Option<bool>,
    pub subtitle_languages: Vec<String>,
    pub subtitle_tracks: Vec<SubtitleSelection>,
    pub playlist: bool,
    pub connections: Option<u8>,
    pub checksum: Option<String>,
    pub start_at: Option<i64>,
    /// yt-dlp `--playlist-items` selection (`1-10,15`), for playlist requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playlist_items: Option<String>,
    /// Per-job speed limit in KiB/s; `None` or `0` follows the global limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed_limit_kib: Option<u64>,
    pub full_verification: bool,
}
impl AddRequest {
    /// Drops the session material a finished job no longer needs: request headers, session
    /// cookies, subtitle headers and captured page markup. Only a completed job forgets
    /// them; paused, failed and waiting jobs keep them for resume, renewal and reports.
    pub(crate) fn forget_session(&mut self) {
        self.headers.clear();
        self.session_cookies.clear();
        self.page_html = None;
        for subtitle in &mut self.external_subtitles {
            subtitle.headers.clear();
        }
    }

    /// Whether any session material that `forget_session` removes is present.
    pub(crate) fn holds_session(&self) -> bool {
        !self.headers.is_empty()
            || !self.session_cookies.is_empty()
            || self.page_html.is_some()
            || self
                .external_subtitles
                .iter()
                .any(|subtitle| !subtitle.headers.is_empty())
    }

    /// The request as the duplicate-request check compares it: identical options, with the
    /// session material that `forget_session` removes left out on both sides.
    pub(crate) fn idempotency_view(&self) -> Self {
        let mut view = self.clone();
        view.forget_session();
        // A per-job speed limit is adjusted after submission and is not an option.
        view.speed_limit_kib = None;
        view
    }
}

impl Default for AddRequest {
    fn default() -> Self {
        Self {
            url: String::new(),
            kind: DownloadKind::Auto,
            filename: None,
            directory: None,
            queue_id: None,
            headers: BTreeMap::new(),
            session_cookies: Vec::new(),
            referer: None,
            page_url: None,
            page_html: None,
            source_identity: None,
            format_id: None,
            video_format_id: None,
            container: None,
            max_height: None,
            exact_height: None,
            audio_format_id: None,
            audio_language: None,
            audio_tracks: Vec::new(),
            subtitle_mode: None,
            external_subtitles: Vec::new(),
            request_id: None,
            audio_format: None,
            expected_audio: None,
            subtitle_languages: Vec::new(),
            subtitle_tracks: Vec::new(),
            playlist: false,
            connections: None,
            checksum: None,
            start_at: None,
            speed_limit_kib: None,
            playlist_items: None,
            full_verification: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct InspectRequest {
    pub url: String,
    pub request_id: Option<String>,
    pub headers: BTreeMap<String, String>,
    pub session_cookies: Vec<ScopedCookie>,
    pub referer: Option<String>,
    /// The page this analysis was handed off from: cookie/source context only. It authorizes
    /// partition cookies and is never sent as an HTTP Referer; `referer` stays the observed
    /// network value, and neither substitutes for the other.
    pub page_url: Option<String>,
    pub playlist: bool,
}

/// One browser-originated media handoff. The extension cannot open the selection UI
/// itself any more, so it hands the desktop the whole request context and the desktop
/// opens its own picker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaLaunch {
    /// Launch identity: dedupes repeated clicks and supersedes an older launch.
    pub id: String,
    /// The picker starts from this exact request; dropped fields cannot be recovered later.
    pub request: AddRequest,
    /// The extension included this site's session cookies under an explicit user grant.
    #[serde(default)]
    pub session_consent: bool,
    /// Bumped for every accepted launch; the window opens on a change.
    #[serde(default)]
    pub seq: u64,
    /// Bumped when the same launch repeats while it is current; an open picker raises itself.
    #[serde(default)]
    pub raise_seq: u64,
    /// Desktop-internal: the originating monitor, captured when the handoff
    /// arrived so the picker opens where the browser is. Never on the wire.
    #[serde(skip)]
    pub origin: Option<[i32; 4]>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Scheduled,
    Connecting,
    Downloading,
    Processing,
    Paused,
    AwaitingSource,
    Completed,
    Failed,
    Cancelled,
}
impl JobState {
    pub fn is_active(self) -> bool {
        matches!(
            self,
            Self::Connecting | Self::Downloading | Self::Processing
        )
    }
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Queued => crate::i18n::ui("Sırada", "Queued"),
            Self::Scheduled => crate::i18n::ui("Zamanlandı", "Scheduled"),
            Self::Connecting => crate::i18n::ui("Bağlanıyor", "Connecting"),
            Self::Downloading => crate::i18n::ui("İndiriliyor", "Downloading"),
            Self::Processing => crate::i18n::ui("İşleniyor", "Processing"),
            Self::Paused => crate::i18n::ui("Duraklatıldı", "Paused"),
            Self::AwaitingSource => {
                crate::i18n::ui("Kaynak yenileme bekleniyor", "Awaiting source")
            }
            Self::Completed => crate::i18n::ui("Tamamlandı", "Completed"),
            Self::Failed => crate::i18n::ui("Başarısız", "Failed"),
            Self::Cancelled => crate::i18n::ui("İptal edildi", "Cancelled"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub request: AddRequest,
    pub name: String,
    pub state: JobState,
    pub path: PathBuf,
    pub downloaded: u64,
    pub total: Option<u64>,
    pub speed: u64,
    pub eta: Option<u64>,
    pub error: Option<String>,
    pub phase: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub attempts: u32,
    #[serde(default)]
    pub work_dir: Option<PathBuf>,
    #[serde(default)]
    pub remove_requested: Option<bool>,
    #[serde(default)]
    pub claim: Option<PathBuf>,
    #[serde(default)]
    pub legacy_completed: bool,
    #[serde(default)]
    pub browser_transfer_authorized: bool,
    /// Queue order: a higher value starts first; equal values keep creation order.
    #[serde(default)]
    pub priority: i64,
    /// Started at the next scheduling pass regardless of the concurrency limits.
    #[serde(default)]
    pub force_start: bool,
    /// The desktop opens the file once this job completes.
    #[serde(default)]
    pub open_when_done: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompletionAction {
    #[default]
    None,
    Notify,
    ShutdownComputer {
        countdown_seconds: u32,
    },
    RunProgram {
        program: PathBuf,
        #[serde(default)]
        arguments: Vec<String>,
        countdown_seconds: u32,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct QueueWindow {
    /// Monday = 0 through Sunday = 6. Empty means every day.
    pub weekdays: Vec<u8>,
    pub start: String,
    pub end: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct QueueQuota {
    pub limit_bytes: u64,
    pub consumed_bytes: u64,
    /// Identifies the concrete local-time schedule occurrence which owns consumed_bytes.
    pub period_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct QueuePolicy {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub concurrency: u8,
    pub windows: Vec<QueueWindow>,
    pub quota: Option<QueueQuota>,
    pub completion: CompletionAction,
}
impl Default for QueuePolicy {
    fn default() -> Self {
        Self {
            id: DEFAULT_QUEUE_ID.into(),
            name: "Varsayılan".into(),
            enabled: true,
            concurrency: 3,
            windows: Vec::new(),
            quota: None,
            completion: CompletionAction::None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct FolderRule {
    pub id: String,
    pub host: String,
    pub include_subdomains: bool,
    /// Empty means every kind.
    pub kinds: Vec<DownloadKind>,
    pub destination: PathBuf,
    pub priority: i32,
    pub enabled: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct SyncPolicy {
    pub id: String,
    pub url: String,
    pub destination: PathBuf,
    pub enabled: bool,
    pub interval_minutes: u32,
    pub overwrite: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub content_sha256: Option<String>,
    pub last_checked_at: Option<i64>,
    pub version: u64,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct SiteCrawlerPolicy {
    pub max_depth: u8,
    pub same_host_only: bool,
    pub allowed_kinds: Vec<DownloadKind>,
    pub max_pages: u32,
    pub max_candidates: u32,
    pub max_response_bytes: u64,
    pub max_total_bytes: u64,
}
impl Default for SiteCrawlerPolicy {
    fn default() -> Self {
        Self {
            max_depth: 2,
            same_host_only: true,
            allowed_kinds: vec![DownloadKind::File],
            max_pages: 100,
            max_candidates: 500,
            max_response_bytes: 2 * 1024 * 1024,
            max_total_bytes: 32 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub download_dir: PathBuf,
    pub max_active: u8,
    pub connections: u8,
    pub media_fragment_connections: u8,
    pub per_host_limit: u8,
    pub speed_limit_kib: u64,
    pub retry_limit: u8,
    pub clipboard_watch: bool,
    pub close_to_tray: bool,
    pub start_with_windows: bool,
    pub notify_completion: bool,
    pub schedule_enabled: bool,
    pub schedule_start: String,
    pub schedule_end: String,
    pub dark_mode: bool,
    pub last_video_container: String,
    pub last_video_height: Option<u32>,
    pub usage_modes: UsageModes,
    pub onboarding_version: u32,
    pub settings_revision: u64,
    pub queues: Vec<QueuePolicy>,
    pub folder_rules: Vec<FolderRule>,
    pub synchronization_policies: Vec<SyncPolicy>,
    pub crawler_policy: SiteCrawlerPolicy,
    pub experimental_browser_transfer: bool,
    pub update_check_enabled: bool,
    pub last_update_check: i64,
    pub skipped_update_version: Option<String>,
    pub extension_offer_seen: bool,
    /// Event log detail: `off` | `error` | `normal` | `detailed`.
    pub logging_level: String,
    /// Records site host names in the event log.
    pub logging_hosts: bool,
    /// Desktop surface depth: `simple` | `advanced`.
    pub ui_mode: String,
    /// Interface language: `tr` (default) | `en`.
    pub ui_language: String,
    /// Developer mode: suppresses the first-run wizard and keeps the
    /// diagnostics, media-tools and raw tool-name surfaces visible.
    pub debug_mode: bool,
    /// The browser extension offers site-level download entries.
    pub site_entries: bool,
    /// A second speed limit that replaces `speed_limit_kib` inside a daily window.
    pub speed_schedule_enabled: bool,
    pub speed_schedule_start: String,
    pub speed_schedule_end: String,
    pub speed_schedule_kib: u64,
    /// Outbound proxy for every transfer and the media tools' loopback proxy.
    pub proxy: ProxySettings,
    /// Per-host HTTP Basic credentials for file transfers.
    pub site_logins: Vec<SiteLogin>,
    /// Keeps Windows from sleeping while a download is active.
    pub keep_awake: bool,
    /// Plays the system completion sound when a download finishes.
    pub completion_sound: bool,
    /// The extension hands ordinary browser downloads over to SSDownload.
    pub browser_takeover: bool,
    /// Completed jobs older than this many days leave the list (files stay); 0 = never.
    pub history_days: u32,
    /// Double-click on a completed row: `open` the file or show it in its `folder`.
    pub double_click: String,
    /// Shows a small card with open actions when a download completes.
    pub completion_card: bool,
    /// Ctrl+Shift+D adds the clipboard's links from anywhere.
    pub global_hotkey: bool,
    /// No new work and running downloads wait while Windows reports a metered network.
    pub pause_on_metered: bool,
    /// The short first-run tour was shown.
    pub tour_seen: bool,
    /// Remembered media choices per site.
    pub site_media_defaults: Vec<SiteMediaDefault>,
    /// Main window placement: left, top, right, bottom, maximized (0/1).
    pub window_placement: Option<[i32; 5]>,
    /// Last top-left position of the media picker.
    pub picker_position: Option<[i32; 2]>,
}

/// Remembered media selection for one host. With `auto`, a handoff from this host
/// queues the remembered choice as soon as the analysis is ready.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct SiteMediaDefault {
    pub host: String,
    pub height: Option<u32>,
    pub container: String,
    pub audio_only: bool,
    pub auto: bool,
}

/// Proxy selection: `system` (Windows static proxy), `none`, or `manual` with an
/// `http://` / `socks5://` address. The password is stored DPAPI-sealed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ProxySettings {
    pub mode: String,
    pub url: String,
    pub username: String,
    pub password: String,
}
impl Default for ProxySettings {
    fn default() -> Self {
        Self {
            mode: "system".into(),
            url: String::new(),
            username: String::new(),
            password: String::new(),
        }
    }
}

/// One host's HTTP Basic credentials. `host` matches the request host exactly
/// (case-insensitive); plain HTTP is used only when `allow_http` is set. The password
/// is stored DPAPI-sealed.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct SiteLogin {
    pub host: String,
    pub username: String,
    pub password: String,
    pub allow_http: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            download_dir: PathBuf::new(),
            max_active: 3,
            connections: 4,
            media_fragment_connections: 4,
            per_host_limit: 2,
            speed_limit_kib: 0,
            retry_limit: 3,
            clipboard_watch: false,
            close_to_tray: true,
            start_with_windows: false,
            notify_completion: true,
            schedule_enabled: false,
            schedule_start: "00:00".into(),
            schedule_end: "23:59".into(),
            dark_mode: false,
            last_video_container: "mp4".into(),
            last_video_height: None,
            usage_modes: UsageModes::default(),
            onboarding_version: 0,
            settings_revision: 0,
            queues: vec![QueuePolicy::default()],
            folder_rules: Vec::new(),
            synchronization_policies: Vec::new(),
            crawler_policy: SiteCrawlerPolicy::default(),
            experimental_browser_transfer: false,
            update_check_enabled: true,
            last_update_check: 0,
            skipped_update_version: None,
            extension_offer_seen: false,
            logging_level: "detailed".into(),
            logging_hosts: true,
            ui_mode: "advanced".into(),
            ui_language: "tr".into(),
            debug_mode: false,
            site_entries: false,
            speed_schedule_enabled: false,
            speed_schedule_start: "09:00".into(),
            speed_schedule_end: "18:00".into(),
            speed_schedule_kib: 0,
            proxy: ProxySettings::default(),
            site_logins: Vec::new(),
            keep_awake: true,
            completion_sound: true,
            browser_takeover: false,
            history_days: 0,
            double_click: "open".into(),
            completion_card: true,
            global_hotkey: true,
            pause_on_metered: false,
            tour_seen: false,
            site_media_defaults: Vec::new(),
            window_placement: None,
            picker_position: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineSnapshot {
    pub revision: u64,
    pub warning: Option<String>,
    pub jobs: Arc<Vec<Job>>,
    pub settings: Settings,
    #[serde(default)]
    pub completion_countdown: Option<CompletionCountdown>,
    #[serde(default)]
    pub completion_events: Vec<CompletionEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceRefreshTicket {
    pub job_id: String,
    pub token: String,
    pub expires_at: i64,
    pub page_url: String,
    pub request: Box<AddRequest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capabilities {
    pub protocol: u32,
    pub capability_version: u32,
    pub usage_modes: UsageModes,
    pub onboarding_version: u32,
    pub settings_revision: u64,
    pub source_refresh_jobs: Vec<SourceRefreshSummary>,
    /// Whether the extension may offer site-level download entries. Owned by
    /// Settings; defaults to off so a page never grows an entry the user did
    /// not ask for.
    #[serde(default)]
    pub site_entries: bool,
    /// Whether the extension hands ordinary browser downloads over to the desktop.
    #[serde(default)]
    pub browser_takeover: bool,
    /// Number of completed jobs in the queue; the extension badges an increase.
    #[serde(default)]
    pub completed_jobs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceRefreshSummary {
    pub id: String,
    pub page_url: String,
    pub title: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionCountdown {
    pub queue_id: String,
    pub deadline: i64,
    pub action: CompletionAction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionEvent {
    pub queue_id: String,
    pub occurred_at: i64,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SiteCrawlRequest {
    pub url: String,
    pub request_id: Option<String>,
    pub policy: Option<SiteCrawlerPolicy>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrawlCandidate {
    pub id: String,
    pub url: String,
    pub kind: DownloadKind,
    pub source_page: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SiteCrawlResult {
    pub request_id: String,
    pub root_url: String,
    pub candidates: Vec<CrawlCandidate>,
    pub pages_scanned: u32,
    pub bytes_scanned: u64,
    /// Pages whose response could not be read; the crawl still returns the
    /// candidates it did find.
    #[serde(default)]
    pub failed_pages: u32,
    #[serde(default)]
    pub last_failure: Option<String>,
    pub truncated: bool,
    pub expires_at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MediaFormat {
    pub id: String,
    pub label: String,
    pub extension: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<f64>,
    pub filesize: Option<u64>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub language: Option<String>,
    pub audio_channels: Option<u32>,
    pub dynamic_range: Option<String>,
    pub has_drm: bool,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SubtitleTrack {
    pub language: String,
    pub automatic: bool,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MediaInfo {
    pub title: String,
    pub webpage_url: String,
    pub duration: Option<f64>,
    pub duration_tolerance: Option<f64>,
    pub thumbnail: Option<String>,
    pub formats: Vec<MediaFormat>,
    pub subtitles: Vec<String>,
    pub subtitle_tracks: Vec<SubtitleTrack>,
    pub playlist_count: Option<usize>,
    pub is_live: bool,
    pub extractor: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolInfo {
    pub name: String,
    pub installed: bool,
    pub version: String,
    pub path: PathBuf,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolProgress {
    pub name: String,
    pub message: String,
    pub downloaded: u64,
    pub total: Option<u64>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TransferProgress {
    pub downloaded: u64,
    pub total: Option<u64>,
    pub speed: u64,
    pub eta: Option<u64>,
    pub phase: String,
}
#[derive(Debug, Clone)]
pub struct TransferResult {
    pub path: PathBuf,
    pub bytes: u64,
}

#[derive(Debug, Clone, Default)]
pub struct TransferControl {
    state: Arc<AtomicU8>,
    speed_limit: Arc<AtomicU64>,
}
impl TransferControl {
    pub fn pause(&self) {
        // Cancellation is terminal. A late pause notification must not turn a
        // remove/shutdown cancellation back into a resumable pause.
        let _ = self
            .state
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire);
    }
    pub fn cancel(&self) {
        self.state.store(2, Ordering::Release);
    }
    pub fn is_paused(&self) -> bool {
        self.state.load(Ordering::Acquire) == 1
    }
    pub fn is_cancelled(&self) -> bool {
        self.state.load(Ordering::Acquire) == 2
    }
    pub fn stop_requested(&self) -> bool {
        self.state.load(Ordering::Acquire) != 0
    }
    pub fn set_speed_limit(&self, bytes_per_sec: u64) {
        self.speed_limit.store(bytes_per_sec, Ordering::Release);
    }
    pub fn speed_limit(&self) -> u64 {
        self.speed_limit.load(Ordering::Acquire)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserMessage {
    pub id: u64,
    pub text: String,
    pub error: bool,
}

#[cfg(test)]
mod tests {
    use super::TransferControl;
    use super::UiMode;
    use std::thread;

    #[test]
    fn an_unreadable_surface_value_reads_as_advanced() {
        // The stored value is a tolerant string: a profile written by any other
        // build must never quarantine the settings record over this field.
        assert!(UiMode::parse("simple").is_simple());
        assert!(UiMode::parse("  Simple ").is_simple());
        for value in ["advanced", "", "gelişmiş", "SIMPLE2"] {
            assert_eq!(
                UiMode::parse(value),
                UiMode::Advanced,
                "{value} must read as advanced"
            );
        }
    }

    #[test]
    fn cancellation_wins_over_late_pause_requests() {
        let control = TransferControl::default();
        control.cancel();
        control.pause();
        assert!(control.is_cancelled());
        assert!(!control.is_paused());
    }

    #[test]
    fn concurrent_pause_and_cancel_never_restore_a_cancelled_job() {
        let control = TransferControl::default();
        thread::scope(|scope| {
            for _ in 0..8 {
                let control = control.clone();
                scope.spawn(move || {
                    for _ in 0..1000 {
                        control.pause();
                    }
                });
            }
            let control = control.clone();
            scope.spawn(move || control.cancel());
        });
        // One final cancellation models remove/shutdown arriving after any
        // in-flight pause messages and must always be preserved.
        control.cancel();
        assert!(control.is_cancelled());
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSnapshot {
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub warning: Option<String>,
    #[serde(default)]
    pub next_offset: Option<usize>,
    #[serde(default)]
    pub total_jobs: usize,
    #[serde(default)]
    pub tools_id: Option<String>,
    #[serde(default)]
    pub tools_error: Option<String>,
    pub jobs: Arc<Vec<Job>>,
    pub settings: Settings,
    #[serde(default)]
    pub completion_countdown: Option<CompletionCountdown>,
    #[serde(default)]
    pub completion_events: Vec<CompletionEvent>,
    pub media: Option<MediaInfo>,
    /// Current browser handoff: present while its picker session lives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_launch: Option<MediaLaunch>,
    pub inspecting: bool,
    #[serde(default)]
    pub inspect_id: Option<String>,
    /// Monotonic identity of the newest analysis. The request id can be reused
    /// (a browser handoff keeps one id per player), so a later round compares
    /// this generation to tell its own result from a replaced one.
    #[serde(default)]
    pub inspect_generation: u64,
    #[serde(default)]
    pub inspect_error: Option<String>,
    /// Stable code for the failed inspection (`SSD-MED-002`), when the failure carried one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inspect_error_code: Option<String>,
    pub tools: Vec<ToolInfo>,
    pub installing_tools: bool,
    pub tool_progress: Option<ToolProgress>,
    pub messages: Vec<UserMessage>,
    pub show_window_seq: u64,
    pub quit_requested: bool,
}

/// One event relayed by the browser extension. Every field is bounded and validated by the
/// bridge before the application records it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayEvent {
    pub event: String,
    #[serde(default)]
    pub level: Option<String>,
    #[serde(default)]
    pub outcome: Option<String>,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub job: Option<String>,
    #[serde(default)]
    pub detail: Option<String>,
}

/// One batch of relayed extension events: `{"type":"log","request":{"events":[…]}}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogBatch {
    pub events: Vec<RelayEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
// Public wire/API value; avoid changing all callers for a one-shot stack allocation.
#[allow(clippy::large_enum_variant)]
pub enum Action {
    Add {
        request: AddRequest,
    },
    Pause {
        id: String,
    },
    Resume {
        id: String,
    },
    Remove {
        id: String,
        delete_file: bool,
    },
    PauseAll,
    ResumeAll,
    /// Moves the listed jobs to the top (`top`) or the bottom of the queue order.
    Reorder {
        ids: Vec<String>,
        top: bool,
    },
    /// Places the listed jobs just before `target` in the queue order (drag and drop).
    MoveBefore {
        ids: Vec<String>,
        target: String,
    },
    /// Starts a queued job now, regardless of the concurrency limits.
    StartNow {
        id: String,
    },
    /// Opens the file when the job completes.
    SetOpenWhenDone {
        id: String,
        value: bool,
    },
    /// Renames a completed download's file in its folder.
    Rename {
        id: String,
        name: String,
    },
    /// Retries every failed job.
    RetryFailed,
    /// Sets the per-job speed limit (KiB/s, 0 = global) of the listed jobs.
    SetSpeedLimit {
        ids: Vec<String>,
        kib: u64,
    },
    ClearCompleted,
    SetSettings {
        settings: Settings,
    },
    Capabilities,
    BeginSourceRefresh {
        id: String,
    },
    CompleteSourceRefresh {
        id: String,
        token: String,
        request: Box<AddRequest>,
        restart: bool,
    },
    CreateQueue {
        name: String,
    },
    RenameQueue {
        id: String,
        name: String,
    },
    DeleteQueue {
        id: String,
    },
    MoveToQueue {
        ids: Vec<String>,
        queue_id: String,
    },
    UpdateQueuePolicy {
        queue: QueuePolicy,
    },
    CancelCompletionAction {
        queue_id: String,
    },
    RunSynchronization {
        id: String,
    },
    CrawlSite {
        request: SiteCrawlRequest,
    },
    AddCrawlCandidates {
        request_id: String,
        candidate_ids: Vec<String>,
        queue_id: Option<String>,
        directory: Option<PathBuf>,
    },
    BrowserTransfer {
        command: crate::browser_transfer::Command,
    },
    Inspect {
        request: InspectRequest,
    },
    /// Browser handoff: the desktop opens its own media picker for this request.
    BrowserMedia {
        launch_id: String,
        request: Box<AddRequest>,
        #[serde(default)]
        session_consent: bool,
    },
    InspectStatus {
        request_id: String,
    },
    InstallTools {
        force_update: bool,
        #[serde(default)]
        request_id: Option<String>,
    },
    ToolsStatus {
        request_id: String,
    },
    StatusPage {
        offset: usize,
        limit: usize,
    },
    OpenFolder {
        id: Option<String>,
    },
    OpenFile {
        id: String,
    },
    BrowserSetup,
    /// Events forwarded by the browser extension for the session log.
    Log {
        request: LogBatch,
    },
    /// Summary of the recorded events, for the CLI.
    Diagnose {
        #[serde(default)]
        hours: Option<u32>,
    },
    ExportDiagnostics,
    /// Writes a local failure report for the most recent failure in this window
    /// (a media analysis or one job) and opens its folder. Nothing is uploaded;
    /// the file exists so the user can attach it by hand.
    Report {
        #[serde(default)]
        job_id: Option<String>,
    },
    Status,
    ShowWindow,
    Quit,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<CommandResult>,
    pub ok: bool,
    pub message: String,
    pub snapshot: Option<AppSnapshot>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CommandResult {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub job_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queue_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_refresh: Option<SourceRefreshTicket>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Capabilities>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crawl: Option<SiteCrawlResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub browser_transfer: Option<crate::browser_transfer::Response>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log: Option<LogReceipt>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnose: Option<crate::diagnose::Summary>,
}

/// Outcome of accepting relayed extension events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogReceipt {
    pub accepted: usize,
    pub rejected: usize,
}

impl CommandResult {
    pub fn log(accepted: usize, rejected: usize) -> Self {
        Self {
            log: Some(LogReceipt { accepted, rejected }),
            ..Self::default()
        }
    }
}
