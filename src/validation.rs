use crate::model::{
    AddRequest, DownloadKind, FolderRule, QueuePolicy, Settings, SiteCrawlerPolicy, SyncPolicy,
    DEFAULT_QUEUE_ID,
};
use anyhow::{bail, Context, Result};
use chrono::Timelike;
pub(crate) const MAX_CONNECTIONS: u8 = 16;
pub(crate) const MAX_ACTIVE: u8 = 16;
pub(crate) const MAX_RETRIES: u8 = 20;
pub(crate) fn validate_url(value: &str, media_only: bool) -> Result<()> {
    let url = url::Url::parse(value.trim()).context(crate::i18n::ui(
        "Geçerli bir URL girin",
        "Enter a valid URL",
    ))?;
    let allowed = if media_only {
        matches!(url.scheme(), "http" | "https")
    } else {
        matches!(url.scheme(), "http" | "https" | "ftp")
    };
    if !allowed || url.host_str().is_none() {
        bail!(
            "{}",
            crate::i18n::ui(
                "Bu bağlantı protokolü desteklenmiyor. HTTP/HTTPS bağlantısı kullanın.",
                "This URL protocol is not supported. Use an HTTP/HTTPS URL."
            )
        );
    }
    if value.len() > 32 * 1024 || value.contains(['\r', '\n', '\0']) {
        bail!(
            "{}",
            crate::i18n::ui(
                "Geçersiz veya aşırı uzun bağlantı",
                "Invalid or excessively long URL"
            )
        );
    }
    Ok(())
}
pub(crate) fn validate_headers(headers: &std::collections::BTreeMap<String, String>) -> Result<()> {
    if headers.len() > 64 {
        bail!(
            "{}",
            crate::i18n::ui("Çok fazla HTTP başlığı", "Too many HTTP headers")
        );
    }
    for (key, value) in headers {
        if key.is_empty()
            || key.len() > 256
            || !key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
            || value.len() > 16384
            || value.contains(['\r', '\n', '\0'])
        {
            bail!(
                "{}",
                crate::i18n::ui("Geçersiz HTTP başlığı", "Invalid HTTP header")
            );
        }
    }
    Ok(())
}
pub(crate) fn validate_new_job(settings: &Settings, request: &AddRequest) -> Result<()> {
    settings
        .usage_modes
        .validate()
        .map_err(anyhow::Error::msg)?;
    if !settings.usage_modes.allows(request.kind) {
        let kind = match request.kind {
            DownloadKind::Auto => crate::i18n::ui("otomatik dosya/video", "automatic file/video"),
            DownloadKind::File => crate::i18n::ui("dosya", "file"),
            DownloadKind::Video => "video",
            DownloadKind::Audio => crate::i18n::ui("ses", "audio"),
        };
        bail!("{}", crate::i18n::ui_owned!(format!("{kind} indirmeleri kullanım amacınızda kapalı. Ayarlar > Kullanım amacı bölümünden etkinleştirin."), format!("{kind} downloads are disabled for your usage purpose. Enable them in Settings > Usage purpose.")));
    }
    if request.audio_format_id.is_some() && !request.audio_tracks.is_empty() {
        bail!(
            "{}",
            crate::i18n::ui(
                "Tek ses seçimi ile çoklu ses seçimi birlikte kullanılamaz.",
                "Single and multiple audio selections cannot be combined."
            )
        );
    }
    if (!request.subtitle_languages.is_empty() || request.subtitle_mode.is_some())
        && !request.subtitle_tracks.is_empty()
    {
        bail!(
            "{}",
            crate::i18n::ui(
                "Eski altyazı seçimi ile açık altyazı parçası seçimi birlikte kullanılamaz.",
                "Legacy subtitle selection and explicit subtitle tracks cannot be combined."
            )
        );
    }
    if request.format_id.is_some() && request.video_format_id.is_some() {
        bail!(
            "{}",
            crate::i18n::ui(
                "Birden fazla video format kimliği aynı anda kullanılamaz.",
                "Multiple video format IDs cannot be used together."
            )
        );
    }
    if let Some(queue_id) = request.queue_id.as_deref() {
        if !valid_id(queue_id) || !settings.queues.iter().any(|queue| queue.id == queue_id) {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Seçilen kuyruk bulunamadı.",
                    "The selected queue was not found."
                )
            );
        }
    }
    Ok(())
}

/// Bounds every field of a relayed extension event so the log cannot be used as a channel for
/// URLs, credentials or unbounded text.
pub fn validate_relay_events(events: &[crate::model::RelayEvent]) -> Result<()> {
    if events.is_empty() || events.len() > 50 {
        bail!(
            "{}",
            crate::i18n::ui(
                "Olay günlüğü partisi 1–50 olay içermelidir",
                "An event batch must contain 1–50 events"
            )
        );
    }
    for event in events {
        // The event name is validated per event by the recorder: one malformed name is counted
        // as rejected instead of silencing a whole report. Every other field below is a bound
        // the batch must satisfy, because those fields carry the payload.
        if let Some(code) = &event.code {
            if crate::error_codes::from_text(code).is_none() {
                bail!("Bilinmeyen hata kodu");
            }
        }
        if let Some(host) = &event.host {
            let host = host.trim();
            if host.is_empty()
                || host.len() > 190
                || host.contains(['/', '\\', ':', '@', '?', '#', ' '])
                || host.contains("..")
            {
                bail!(
                    "{}",
                    crate::i18n::ui(
                        "Olay host alanı yalnız ana makine adı olabilir",
                        "The event host field must contain only a host name"
                    )
                );
            }
        }
        if let Some(job) = &event.job {
            if job.len() > 64 || job.contains(['\r', '\n']) {
                bail!(
                    "{}",
                    crate::i18n::ui("Geçersiz iş kimliği", "Invalid job ID")
                );
            }
        }
        if let Some(detail) = &event.detail {
            if detail.len() > 300 || detail.contains(['\r', '\n', '\0']) {
                bail!(
                    "{}",
                    crate::i18n::ui(
                        "Olay ayrıntısı çok uzun veya çok satırlı",
                        "Event details are too long or contain multiple lines"
                    )
                );
            }
        }
        if let Some(outcome) = &event.outcome {
            if crate::logging::parse_outcome(outcome).is_none() {
                bail!(
                    "{}",
                    crate::i18n::ui("Geçersiz olay sonucu", "Invalid event outcome")
                );
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_settings(settings: &Settings) -> Result<()> {
    if !matches!(settings.last_video_container.as_str(), "mp4" | "mkv")
        || settings
            .last_video_height
            .is_some_and(|height| height == 0 || height > 16384)
    {
        bail!(
            "{}",
            crate::i18n::ui(
                "Geçersiz video biçimi veya çözünürlük tercihi",
                "Invalid video format or resolution preference"
            )
        );
    }
    if !(1..=MAX_ACTIVE).contains(&settings.max_active)
        || !(1..=MAX_CONNECTIONS).contains(&settings.connections)
        || !(1..=MAX_CONNECTIONS).contains(&settings.media_fragment_connections)
        || !(1..=MAX_ACTIVE).contains(&settings.per_host_limit)
    {
        bail!(
            "{}",
            crate::i18n::ui(
                "İş ve bağlantı sınırları 1–16 arasında olmalı.",
                "Job and connection limits must be between 1 and 16."
            )
        );
    }
    if settings.retry_limit > MAX_RETRIES {
        bail!(
            "{}",
            crate::i18n::ui(
                "Yeniden deneme sınırı en fazla 20 olabilir.",
                "The retry limit cannot exceed 20."
            )
        );
    }
    if settings.speed_limit_kib > u64::MAX / 1024 {
        bail!(
            "{}",
            crate::i18n::ui("Hız sınırı çok büyük.", "The speed limit is too large.")
        );
    }
    if crate::logging::Level::parse(&settings.logging_level).is_none() {
        bail!(
            "{}",
            crate::i18n::ui(
                "Günlük düzeyi off, error, normal veya detailed olmalı.",
                "The logging level must be off, error, normal or detailed."
            )
        );
    }
    if settings.download_dir.as_os_str().is_empty() || !settings.download_dir.is_absolute() {
        bail!(
            "{}",
            crate::i18n::ui(
                "Mutlak bir indirme klasörü seçin.",
                "Choose an absolute download directory."
            )
        );
    }
    for time in [&settings.schedule_start, &settings.schedule_end] {
        chrono::NaiveTime::parse_from_str(time, "%H:%M").context(crate::i18n::ui(
            "Zamanlama saati SS:DD biçiminde olmalı (örn. 23:30)",
            "The schedule time must use HH:MM format (e.g. 23:30)",
        ))?;
    }
    settings
        .usage_modes
        .validate()
        .map_err(anyhow::Error::msg)?;
    if settings.queues.is_empty() || settings.queues.len() > 64 {
        bail!(
            "{}",
            crate::i18n::ui(
                "En az bir, en fazla 64 kuyruk tanımlanabilir.",
                "Define between 1 and 64 queues."
            )
        );
    }
    if settings
        .queues
        .iter()
        .filter(|queue| queue.id == DEFAULT_QUEUE_ID)
        .count()
        != 1
    {
        bail!(
            "{}",
            crate::i18n::ui(
                "Varsayılan kuyruk eksik veya yinelenmiş.",
                "The default queue is missing or duplicated."
            )
        );
    }
    let mut queue_ids = std::collections::BTreeSet::new();
    for queue in &settings.queues {
        validate_queue(queue)?;
        if !queue_ids.insert(&queue.id) {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Kuyruk kimlikleri yinelenemez.",
                    "Queue IDs must be unique."
                )
            );
        }
    }
    validate_transfer_options(settings)?;
    if settings.folder_rules.len() > 256 {
        bail!(
            "{}",
            crate::i18n::ui(
                "En fazla 256 klasör kuralı tanımlanabilir.",
                "No more than 256 folder rules can be defined."
            )
        );
    }
    let mut rule_ids = std::collections::BTreeSet::new();
    for rule in &settings.folder_rules {
        validate_folder_rule(rule)?;
        if !rule_ids.insert(&rule.id) {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Klasör kuralı kimlikleri yinelenemez.",
                    "Folder rule IDs must be unique."
                )
            );
        }
    }
    if settings.synchronization_policies.len() > 128 {
        bail!(
            "{}",
            crate::i18n::ui(
                "En fazla 128 senkronizasyon tanımlanabilir.",
                "No more than 128 synchronizations can be defined."
            )
        );
    }
    let mut sync_ids = std::collections::BTreeSet::new();
    for policy in &settings.synchronization_policies {
        validate_sync_policy(policy)?;
        if !sync_ids.insert(&policy.id) {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Senkronizasyon kimlikleri yinelenemez.",
                    "Synchronization IDs must be unique."
                )
            );
        }
    }
    validate_crawler_policy(&settings.crawler_policy)?;
    Ok(())
}

pub(crate) fn validate_queue(queue: &QueuePolicy) -> Result<()> {
    if !valid_id(&queue.id) || queue.name.trim().is_empty() || queue.name.chars().count() > 80 {
        bail!(
            "{}",
            crate::i18n::ui(
                "Kuyruk kimliği veya adı geçersiz.",
                "Invalid queue ID or name."
            )
        );
    }
    if !(1..=MAX_ACTIVE).contains(&queue.concurrency) {
        bail!(
            "{}",
            crate::i18n::ui(
                "Kuyruk eşzamanlılığı 1–16 arasında olmalı.",
                "Queue concurrency must be between 1 and 16."
            )
        );
    }
    if queue.windows.len() > 16 {
        bail!(
            "{}",
            crate::i18n::ui(
                "Bir kuyrukta en fazla 16 zaman aralığı olabilir.",
                "A queue can have no more than 16 time windows."
            )
        );
    }
    let mut occupied_week_minutes = vec![false; 7 * 24 * 60];
    for window in &queue.windows {
        let start =
            chrono::NaiveTime::parse_from_str(&window.start, "%H:%M").context(crate::i18n::ui(
                "Kuyruk başlangıç saati SS:DD biçiminde olmalı",
                "The queue start time must use HH:MM format",
            ))?;
        let end =
            chrono::NaiveTime::parse_from_str(&window.end, "%H:%M").context(crate::i18n::ui(
                "Kuyruk bitiş saati SS:DD biçiminde olmalı",
                "The queue end time must use HH:MM format",
            ))?;
        if window.weekdays.iter().any(|day| *day > 6) {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Haftanın günü 0–6 arasında olmalı.",
                    "The weekday must be between 0 and 6."
                )
            );
        }
        let unique = window
            .weekdays
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        if unique.len() != window.weekdays.len() {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Kuyruk günleri yinelenemez.",
                    "Queue weekdays must be unique."
                )
            );
        }
        let start_minute = start.num_seconds_from_midnight() as usize / 60;
        let end_minute = end.num_seconds_from_midnight() as usize / 60;
        let duration = if start_minute == end_minute {
            24 * 60
        } else {
            (end_minute + 24 * 60 - start_minute) % (24 * 60)
        };
        let days = if window.weekdays.is_empty() {
            (0u8..7).collect::<Vec<_>>()
        } else {
            window.weekdays.clone()
        };
        for day in days {
            let absolute_start = day as usize * 24 * 60 + start_minute;
            for offset in 0..duration {
                let minute = (absolute_start + offset) % occupied_week_minutes.len();
                if std::mem::replace(&mut occupied_week_minutes[minute], true) {
                    bail!(
                        "{}",
                        crate::i18n::ui(
                            "Kuyruk zaman aralıkları birbiriyle çakışamaz.",
                            "Queue time windows must not overlap."
                        )
                    );
                }
            }
        }
    }
    if let Some(quota) = &queue.quota {
        if quota.limit_bytes == 0 {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Kuyruk kotası sıfır olamaz.",
                    "The queue quota cannot be zero."
                )
            );
        }
        if quota.period_key.len() > 64 || quota.period_key.contains(['\r', '\n', '\0']) {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Kuyruk kota dönemi geçersiz.",
                    "Invalid queue quota period."
                )
            );
        }
    }
    if let crate::model::CompletionAction::RunProgram {
        program,
        arguments,
        countdown_seconds,
    } = &queue.completion
    {
        if !program.is_absolute() || program.as_os_str().is_empty() || arguments.len() > 64 {
            bail!("{}", crate::i18n::ui("Bitiş programı mutlak bir yol olmalı ve en fazla 64 argüman alabilir.", "The completion program must use an absolute path and accept no more than 64 arguments."));
        }
        if arguments
            .iter()
            .any(|arg| arg.len() > 4096 || arg.contains('\0'))
        {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Bitiş programı argümanı geçersiz.",
                    "Invalid completion program argument."
                )
            );
        }
        if *countdown_seconds > 3600 {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Bitiş eylemi geri sayımı en fazla bir saat olabilir.",
                    "The completion action countdown cannot exceed one hour."
                )
            );
        }
    }
    if let crate::model::CompletionAction::ShutdownComputer { countdown_seconds } =
        &queue.completion
    {
        if *countdown_seconds > 3600 {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Kapatma geri sayımı en fazla bir saat olabilir.",
                    "The shutdown countdown cannot exceed one hour."
                )
            );
        }
    }
    Ok(())
}

fn validate_folder_rule(rule: &FolderRule) -> Result<()> {
    if !valid_id(&rule.id) || rule.host.trim().is_empty() || rule.host.len() > 253 {
        bail!(
            "{}",
            crate::i18n::ui(
                "Klasör kuralı kimliği veya host adı geçersiz.",
                "Invalid folder rule ID or host name."
            )
        );
    }
    let host = rule.host.trim().trim_end_matches('.');
    let parsed = url::Url::parse(&format!("https://{host}")).context(crate::i18n::ui(
        "Klasör kuralı host adı geçersiz",
        "Invalid folder rule host name",
    ))?;
    if parsed.host_str().is_none()
        || !parsed
            .host_str()
            .is_some_and(|parsed| parsed.eq_ignore_ascii_case(host))
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.port().is_some()
        || parsed.path() != "/"
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        bail!(
            "{}",
            crate::i18n::ui(
                "Klasör kuralı yalnız tam bir host adı içermeli.",
                "A folder rule must contain only a complete host name."
            )
        );
    }
    if rule
        .kinds
        .iter()
        .enumerate()
        .any(|(index, kind)| rule.kinds[..index].contains(kind))
    {
        bail!(
            "{}",
            crate::i18n::ui(
                "Klasör kuralı türleri yinelenemez.",
                "Folder rule types must be unique."
            )
        );
    }
    if !rule.destination.is_absolute()
        || rule
            .destination
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        bail!(
            "{}",
            crate::i18n::ui(
                "Klasör kuralı hedefi güvenli, mutlak bir yol olmalı.",
                "The folder rule destination must be a safe absolute path."
            )
        );
    }
    if rule.kinds.contains(&DownloadKind::Auto) {
        bail!(
            "{}",
            crate::i18n::ui(
                "Klasör kuralında otomatik tür yerine dosya, video veya ses seçin.",
                "Choose file, video or audio rather than automatic for a folder rule."
            )
        );
    }
    Ok(())
}

fn validate_sync_policy(policy: &SyncPolicy) -> Result<()> {
    if !valid_id(&policy.id) || !(1..=525_600).contains(&policy.interval_minutes) {
        bail!(
            "{}",
            crate::i18n::ui(
                "Senkronizasyon kimliği veya aralığı geçersiz.",
                "Invalid synchronization ID or interval."
            )
        );
    }
    validate_url(&policy.url, false)?;
    let parsed = url::Url::parse(&policy.url)?;
    if !matches!(parsed.scheme(), "http" | "https")
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        bail!(
            "{}",
            crate::i18n::ui(
                "Senkronizasyon yalnız kullanıcı bilgisi içermeyen HTTP/HTTPS adresi kullanabilir.",
                "Synchronization requires an HTTP/HTTPS URL without user credentials."
            )
        );
    }
    if !policy.destination.is_absolute()
        || policy.destination.file_name().is_none()
        || policy
            .destination
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
        || policy.destination.exists() && policy.destination.is_dir()
    {
        bail!(
            "{}",
            crate::i18n::ui(
                "Senkronizasyon hedefi güvenli, mutlak bir dosya yolu olmalı.",
                "The synchronization destination must be a safe absolute file path."
            )
        );
    }
    if policy
        .content_sha256
        .as_ref()
        .is_some_and(|hash| hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        bail!(
            "{}",
            crate::i18n::ui(
                "Senkronizasyon içerik özeti geçersiz.",
                "Invalid synchronization content digest."
            )
        );
    }
    for (label, value) in [
        ("ETag", policy.etag.as_deref()),
        ("Last-Modified", policy.last_modified.as_deref()),
    ] {
        if value.is_some_and(|value| value.len() > 8192 || value.contains(['\r', '\n', '\0'])) {
            bail!(
                "{}",
                crate::i18n::ui_owned!(
                    format!("Senkronizasyon {label} doğrulayıcısı geçersiz."),
                    format!("Invalid synchronization {label} validator.")
                )
            );
        }
    }
    if policy
        .last_error
        .as_ref()
        .is_some_and(|error| error.len() > 16 * 1024)
    {
        bail!(
            "{}",
            crate::i18n::ui(
                "Senkronizasyon hata kaydı çok uzun.",
                "The synchronization error record is too long."
            )
        );
    }
    Ok(())
}

pub(crate) fn validate_crawler_policy(policy: &SiteCrawlerPolicy) -> Result<()> {
    if policy.max_depth > 8
        || !(1..=1_000).contains(&policy.max_pages)
        || !(1..=10_000).contains(&policy.max_candidates)
        || !(1..=16 * 1024 * 1024).contains(&policy.max_response_bytes)
        || policy.max_total_bytes < policy.max_response_bytes
        || policy.max_total_bytes > 256 * 1024 * 1024
    {
        bail!(
            "{}",
            crate::i18n::ui(
                "Site tarayıcı sınırları geçersiz.",
                "Invalid site crawler limits."
            )
        );
    }
    if policy.allowed_kinds.is_empty() || policy.allowed_kinds.contains(&DownloadKind::Auto) {
        bail!(
            "{}",
            crate::i18n::ui(
                "Site tarayıcı en az bir açık dosya, video veya ses türü seçmeli.",
                "The site crawler must select at least one explicit file, video or audio type."
            )
        );
    }
    Ok(())
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

/// Scheduled speed limit, proxy and site-login settings.
fn validate_transfer_options(settings: &Settings) -> Result<()> {
    for time in [&settings.speed_schedule_start, &settings.speed_schedule_end] {
        chrono::NaiveTime::parse_from_str(time, "%H:%M").context(crate::i18n::ui(
            "Hız sınırı saati SS:DD biçiminde olmalı (örn. 09:00)",
            "The speed limit time must use HH:MM format (e.g. 09:00)",
        ))?;
    }
    if settings.speed_schedule_kib > u64::MAX / 1024 {
        bail!(
            "{}",
            crate::i18n::ui("Hız sınırı çok büyük.", "The speed limit is too large.")
        );
    }
    validate_proxy(&settings.proxy)?;
    if settings.site_logins.len() > 64 {
        bail!(
            "{}",
            crate::i18n::ui(
                "En fazla 64 site girişi tanımlanabilir.",
                "No more than 64 site logins can be defined."
            )
        );
    }
    let mut hosts = std::collections::BTreeSet::new();
    for login in &settings.site_logins {
        let host = login.host.trim().to_ascii_lowercase();
        if host.is_empty()
            || host.len() > 253
            || url::Host::parse(&host).is_err()
            || host.contains(['/', ':', '@', ' '])
        {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Site girişi için geçerli bir sunucu adı girin (örn. dosya.example.com).",
                    "Enter a valid host name for the site login (e.g. files.example.com)."
                )
            );
        }
        if login.username.is_empty()
            || login.username.len() > 256
            || login.username.contains([':', '\r', '\n'])
        {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Site girişi kullanıcı adı boş olamaz ve ':' içeremez.",
                    "The site login user name cannot be empty or contain ':'."
                )
            );
        }
        validate_sealed(&login.password)?;
        if !hosts.insert(host) {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Aynı sunucu için birden fazla site girişi tanımlanamaz.",
                    "Only one site login can be defined per host."
                )
            );
        }
    }
    Ok(())
}

/// A stored secret is empty or a DPAPI envelope; plaintext never reaches the store.
fn validate_sealed(value: &str) -> Result<()> {
    if !value.is_empty() && !value.starts_with("dpapi-v1:") {
        bail!(
            "{}",
            crate::i18n::ui(
                "Parola korunmadan kaydedilemez.",
                "A password cannot be saved unprotected."
            )
        );
    }
    Ok(())
}

pub(crate) fn validate_proxy(proxy: &crate::model::ProxySettings) -> Result<()> {
    match proxy.mode.as_str() {
        "system" | "none" => {}
        "manual" => {
            let parsed = url::Url::parse(proxy.url.trim()).ok();
            let valid = parsed.as_ref().is_some_and(|url| {
                matches!(url.scheme(), "http" | "socks5" | "socks5h")
                    && url.host_str().is_some_and(|host| !host.is_empty())
                    && url.port_or_known_default().is_some()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && matches!(url.path(), "" | "/")
                    && url.query().is_none()
            });
            if !valid {
                bail!(
                    "{}",
                    crate::i18n::ui(
                        "Proxy adresi http://sunucu:port veya socks5://sunucu:port biçiminde olmalı; kullanıcı adı ve parola ayrı alanlara girilir.",
                        "The proxy address must be http://host:port or socks5://host:port; enter the user name and password in their own fields."
                    )
                );
            }
        }
        _ => bail!(
            "{}",
            crate::i18n::ui("Geçersiz proxy kipi.", "Invalid proxy mode.")
        ),
    }
    if proxy.username.len() > 256 || proxy.username.contains([':', '\r', '\n']) {
        bail!(
            "{}",
            crate::i18n::ui(
                "Proxy kullanıcı adı ':' içeremez.",
                "The proxy user name cannot contain ':'."
            )
        );
    }
    validate_sealed(&proxy.password)
}

/// Upper bound of addresses one batch pattern may produce.
pub(crate) const MAX_BATCH_URLS: usize = 1000;

/// Expands the first `[start-end]` range of a batch address, recursively, into the
/// addresses it names: numeric ranges keep the start's zero padding (`[001-120]`), letter
/// ranges run inside one case (`[a-z]`). `Ok(None)` means the text holds no range.
pub(crate) fn expand_batch_pattern(value: &str) -> Result<Option<Vec<String>>> {
    let Some((prefix, range, suffix)) = find_batch_range(value) else {
        return Ok(None);
    };
    let (start, end) = range.split_once('-').expect("checked by find_batch_range");
    let items: Vec<String> =
        if let (Ok(first), Ok(last)) = (start.parse::<u64>(), end.parse::<u64>()) {
            if first > last || last - first >= MAX_BATCH_URLS as u64 {
                bail!("{}", batch_limit_message());
            }
            let width = if start.len() > 1 && start.starts_with('0') {
                start.len()
            } else {
                0
            };
            (first..=last).map(|n| format!("{n:0width$}")).collect()
        } else {
            let (first, last) = (
                start.chars().next().unwrap_or('a'),
                end.chars().next().unwrap_or('a'),
            );
            (first..=last).map(String::from).collect()
        };
    let mut out = Vec::new();
    for item in items {
        let expanded = format!("{prefix}{item}{suffix}");
        match expand_batch_pattern(&expanded)? {
            Some(nested) => out.extend(nested),
            None => out.push(expanded),
        }
        if out.len() > MAX_BATCH_URLS {
            bail!("{}", batch_limit_message());
        }
    }
    Ok(Some(out))
}

fn batch_limit_message() -> &'static str {
    crate::i18n::ui(
        "Toplu indirme deseni en fazla 1000 adres üretebilir ve aralık başı sonundan büyük olamaz.",
        "A batch pattern can produce at most 1000 addresses and a range cannot start after its end.",
    )
}

/// `(prefix, "a-b", suffix)` for the first bracket that holds a numeric range of
/// equal-or-shorter digits or a single-case letter range.
fn find_batch_range(value: &str) -> Option<(&str, &str, &str)> {
    let mut offset = 0;
    while let Some(open) = value[offset..].find('[') {
        let open = offset + open;
        let close = open + value[open..].find(']')?;
        let inner = &value[open + 1..close];
        if let Some((start, end)) = inner.split_once('-') {
            let numeric = !start.is_empty()
                && !end.is_empty()
                && start.len() <= 9
                && end.len() <= 9
                && start.bytes().all(|b| b.is_ascii_digit())
                && end.bytes().all(|b| b.is_ascii_digit());
            let letters = start.len() == 1
                && end.len() == 1
                && start.bytes().zip(end.bytes()).all(|(a, b)| {
                    (a.is_ascii_lowercase() && b.is_ascii_lowercase() && a <= b)
                        || (a.is_ascii_uppercase() && b.is_ascii_uppercase() && a <= b)
                });
            if numeric || letters {
                return Some((&value[..open], inner, &value[close + 1..]));
            }
        }
        offset = close + 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::RelayEvent;

    fn relay(event: &str) -> RelayEvent {
        RelayEvent {
            event: event.into(),
            level: None,
            outcome: None,
            code: None,
            host: None,
            job: None,
            detail: None,
        }
    }

    #[test]
    fn relay_event_batch_accepts_bounded_events() {
        let mut event = relay("relay.capture.observed");
        event.level = Some("warn".into());
        event.outcome = Some("partial".into());
        event.code = Some("SSD-EXT-010".into());
        event.host = Some("site.example".into());
        event.job = Some("relayjob0001".into());
        event.detail = Some("x".repeat(300));
        assert!(validate_relay_events(&[event]).is_ok());
    }

    #[test]
    fn relay_event_batch_enforces_the_one_to_fifty_bound() {
        assert!(validate_relay_events(&[]).is_err());
        let over_limit: Vec<RelayEvent> =
            (0..51).map(|_| relay("relay.capture.observed")).collect();
        assert!(validate_relay_events(&over_limit).is_err());
        let at_limit: Vec<RelayEvent> = (0..50).map(|_| relay("relay.capture.observed")).collect();
        assert!(validate_relay_events(&at_limit).is_ok());
    }

    #[test]
    fn relay_event_host_must_be_a_bare_host_name() {
        for host in ["https://site.example/x", "site.example:443"] {
            let mut event = relay("relay.capture.observed");
            event.host = Some(host.into());
            assert!(validate_relay_events(&[event]).is_err(), "{host}");
        }
        let mut bare = relay("relay.capture.observed");
        bare.host = Some("site.example".into());
        assert!(validate_relay_events(&[bare]).is_ok());
    }

    #[test]
    fn relay_event_rejects_unknown_codes_and_malformed_names() {
        let mut unknown_code = relay("relay.capture.observed");
        unknown_code.code = Some("SSD-XXX-999".into());
        assert!(validate_relay_events(&[unknown_code]).is_err());

        // A malformed name is rejected per event by the recorder, so the batch itself passes.
        for name in ["Site.Event", &"a".repeat(65)] {
            assert!(validate_relay_events(&[relay(name)]).is_ok(), "{name}");
            assert!(!crate::logging::valid_event_name(name), "{name}");
        }
    }

    #[test]
    fn relay_event_rejects_unbounded_or_multiline_details() {
        let mut long_detail = relay("relay.capture.observed");
        long_detail.detail = Some("x".repeat(301));
        assert!(validate_relay_events(&[long_detail]).is_err());

        let mut multiline = relay("relay.capture.observed");
        multiline.detail = Some("first line\nsecond line".into());
        assert!(validate_relay_events(&[multiline]).is_err());
    }

    #[test]
    fn batch_patterns_expand_padded_numbers_letters_and_nested_ranges() {
        assert_eq!(
            expand_batch_pattern("https://a.example/x.zip").unwrap(),
            None
        );
        let numbers = expand_batch_pattern("https://a.example/img[008-011].jpg")
            .unwrap()
            .unwrap();
        assert_eq!(
            numbers,
            [
                "https://a.example/img008.jpg",
                "https://a.example/img009.jpg",
                "https://a.example/img010.jpg",
                "https://a.example/img011.jpg"
            ]
        );
        let nested = expand_batch_pattern("https://a.example/[a-b]/[1-2].bin")
            .unwrap()
            .unwrap();
        assert_eq!(nested.len(), 4);
        assert_eq!(nested[3], "https://a.example/b/2.bin");
        assert!(expand_batch_pattern("https://a.example/[1-5000].bin").is_err());
        assert!(expand_batch_pattern("https://a.example/[9-1].bin").is_err());
        assert_eq!(expand_batch_pattern("https://[::1]/x").unwrap(), None);
    }
}
