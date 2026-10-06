use crate::validation::{validate_headers, validate_settings, validate_url};
use crate::{engine::Engine, install, media, model::*, paths::AppPaths, tools};
use anyhow::{bail, Context, Result};
use sha2::Digest;
use std::{
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

#[derive(Clone)]
pub struct App {
    inner: Arc<Inner>,
}
struct Inner {
    paths: AppPaths,
    engine: Engine,
    ui: Mutex<UiState>,
    shutdown: AtomicBool,
}
struct UiState {
    media: Option<MediaInfo>,
    inspecting: bool,
    inspect_id: Option<String>,
    /// Monotonic identity of the newest analysis. A caller may reuse its request
    /// id (the browser handoff does), so the id alone cannot tell an old worker
    /// from the current one; only a worker holding the current token publishes.
    inspect_token: u64,
    inspect_error: Option<String>,
    inspect_error_code: Option<String>,
    inspect_control: Option<TransferControl>,
    tools: Vec<ToolInfo>,
    installing_tools: bool,
    tool_progress: Option<ToolProgress>,
    tools_id: Option<String>,
    tools_error: Option<String>,
    messages: Vec<UserMessage>,
    next_message: u64,
    show_window_seq: u64,
    quit_requested: bool,
    /// Current browser handoff. The native picker opens from this and ends with it.
    media_launch: Option<MediaLaunch>,
    /// Monotonic handoff sequence. It never restarts when a launch ends, so a
    /// later handoff can never reuse the sequence the picker already opened
    /// from.
    launch_seq: u64,
    crawl_session: Option<SiteCrawlResult>,
    /// Session preference shared with the native media dialog: the user chose to
    /// embed no subtitle track, so later analyses must not preselect one.
    subtitle_none: bool,
    /// How often each stable code failed in this session, for the repeat hint.
    error_counts: std::collections::HashMap<&'static str, u32>,
}
/// Developer mode requested for this process. `--debug` exports
/// `SSDOWNLOAD_DEBUG=1` before the application opens, and automated runs may
/// set the variable directly; an explicit `0`/`false` turns it off again.
pub fn debug_mode_override() -> Option<bool> {
    let value = std::env::var("SSDOWNLOAD_DEBUG").ok()?;
    let value = value.trim();
    Some(
        !(value.is_empty()
            || value == "0"
            || value.eq_ignore_ascii_case("false")
            || value.eq_ignore_ascii_case("no")),
    )
}

impl App {
    pub fn open(paths: AppPaths) -> Result<Self> {
        let installed = tools::status(&paths);
        let engine = Engine::open(paths.clone())?;
        let mut settings = engine.snapshot().settings;
        // The stored interface language applies before the first frame.
        crate::i18n::set_language(&settings.ui_language);
        let actual_autostart = install::autostart_enabled(&paths)?;
        let mut startup_warning = None;
        let log_config = crate::logging::Config {
            level: crate::logging::Level::parse(&settings.logging_level)
                .unwrap_or(crate::logging::Level::Detailed),
            hosts: settings.logging_hosts,
        };
        let session = crate::logging::Session {
            id: uuid::Uuid::new_v4().to_string()[..8].to_string(),
            channel: "app",
            version: env!("CARGO_PKG_VERSION"),
        };
        let log_error = crate::logging::start(&paths, session, log_config).err();
        let mut settings_dirty = false;
        if settings.start_with_windows != actual_autostart {
            settings.start_with_windows = actual_autostart;
            settings_dirty = true;
        }
        // Developer mode is requested before startup (`--debug` exports
        // `SSDOWNLOAD_DEBUG=1`) and is persisted, so the choice survives the
        // next ordinary launch. `SSDOWNLOAD_DEBUG=0` clears it again.
        if let Some(enabled) = debug_mode_override() {
            if settings.debug_mode != enabled {
                settings.debug_mode = enabled;
                settings_dirty = true;
            }
        }
        if settings_dirty {
            if let Err(error) = engine.update_settings(settings) {
                startup_warning = Some(crate::i18n::ui_owned!(
                    format!("Başlangıç ayarı eşitlenemedi: {error}"),
                    format!("Startup setting could not be applied: {error}")
                ));
            }
        }
        let app = Self {
            inner: Arc::new(Inner {
                paths,
                engine,
                shutdown: AtomicBool::new(false),
                ui: Mutex::new(UiState {
                    media: None,
                    inspecting: false,
                    inspect_id: None,
                    inspect_token: 0,
                    inspect_error: None,
                    inspect_error_code: None,
                    inspect_control: None,
                    tools: installed,
                    installing_tools: false,
                    tool_progress: None,
                    tools_id: None,
                    tools_error: None,
                    messages: Vec::new(),
                    next_message: 1,
                    show_window_seq: 0,
                    quit_requested: false,
                    media_launch: None,
                    launch_seq: 0,
                    crawl_session: None,
                    subtitle_none: false,
                    error_counts: std::collections::HashMap::new(),
                }),
            }),
        };
        if let Some(warning) = startup_warning {
            app.message(warning, true);
        }
        // The installer helper must keep the staged Setup until the silent
        // install finishes, so leftovers from a completed or aborted update
        // move into the quarantine folder here, with the path reported.
        match crate::update::reconcile_update_staging(app.paths()) {
            Ok(Some(folder)) => app.message(
                crate::i18n::ui_owned!(
                    format!(
                        "Önceki güncelleme dosyası karantinaya taşındı:\n{}",
                        folder.display()
                    ),
                    format!(
                        "The previous update file was moved to quarantine:\n{}",
                        folder.display()
                    )
                ),
                false,
            ),
            Ok(None) => {}
            Err(error) => app.message(
                crate::i18n::ui_owned!(
                    format!("Güncelleme klasörü uzlaştırılamadı: {error:#}"),
                    format!("The update folder could not be reconciled: {error:#}")
                ),
                true,
            ),
        }
        if let Some(error) = log_error {
            app.message(
                crate::i18n::ui_owned!(
                    format!("Olay günlüğü başlatılamadı: {error:#}"),
                    format!("The event log could not be started: {error:#}")
                ),
                true,
            );
        }
        Ok(app)
    }
    pub fn paths(&self) -> &AppPaths {
        &self.inner.paths
    }
    pub(crate) fn quit_requested(&self) -> bool {
        self.inner
            .ui
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .quit_requested
    }
    /// Session-level "embed no subtitles" preference, shared with the popup's
    /// equivalent setting.
    pub(crate) fn subtitle_none(&self) -> bool {
        self.inner
            .ui
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .subtitle_none
    }
    pub(crate) fn set_subtitle_none(&self, value: bool) {
        self.inner
            .ui
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .subtitle_none = value;
    }
    pub fn snapshot(&self) -> AppSnapshot {
        let engine = self.inner.engine.snapshot();
        let ui = self.inner.ui.lock().unwrap_or_else(|e| e.into_inner());
        AppSnapshot {
            total_jobs: engine.jobs.len(),
            jobs: engine.jobs,
            revision: engine.revision,
            warning: engine.warning,
            next_offset: None,
            settings: engine.settings,
            completion_countdown: engine.completion_countdown,
            completion_events: engine.completion_events,
            media: ui.media.clone(),
            media_launch: ui.media_launch.clone(),
            inspecting: ui.inspecting,
            inspect_id: ui.inspect_id.clone(),
            inspect_generation: ui.inspect_token,
            inspect_error: ui.inspect_error.clone(),
            inspect_error_code: ui.inspect_error_code.clone(),
            tools: ui.tools.clone(),
            installing_tools: ui.installing_tools,
            tool_progress: ui.tool_progress.clone(),
            tools_id: ui.tools_id.clone(),
            tools_error: ui.tools_error.clone(),
            messages: ui.messages.clone(),
            show_window_seq: ui.show_window_seq,
            quit_requested: ui.quit_requested,
        }
    }
    pub(crate) fn snapshot_page(&self, offset: usize, limit: usize) -> AppSnapshot {
        let mut snapshot = self.snapshot();
        let total = snapshot.jobs.len();
        let end = offset.saturating_add(limit.min(500)).min(total);
        snapshot.jobs = Arc::new(snapshot.jobs.get(offset..end).unwrap_or_default().to_vec());
        snapshot.next_offset = (end < total).then_some(end);
        snapshot.total_jobs = total;
        snapshot
    }
    pub fn dispatch(&self, action: Action) -> Result<()> {
        self.dispatch_result(action).map(|_| ())
    }
    /// Ends a browser handoff once its picker closes, so the next click starts fresh.
    pub(crate) fn clear_media_launch(&self, launch_seq: u64) {
        let mut ui = self.inner.ui.lock().unwrap_or_else(|e| e.into_inner());
        if ui
            .media_launch
            .as_ref()
            .is_some_and(|launch| launch.seq == launch_seq)
        {
            ui.media_launch = None;
        }
    }
    /// Starts one media analysis. A browser handoff also records the launch its
    /// native picker opens from, and repeating that launch only raises the
    /// picker: no second analysis and no second window.
    fn begin_inspection(
        &self,
        mut request: InspectRequest,
        launch: Option<(String, Box<AddRequest>, bool)>,
    ) -> Result<()> {
        if !self
            .inner
            .engine
            .snapshot()
            .settings
            .usage_modes
            .allows_inspection()
        {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Video ve ses inceleme kullanım amacınızda kapalı.",
                    "Video and audio inspection is disabled for your usage purpose."
                )
            );
        }
        validate_url(&request.url, true)?;
        validate_headers(&request.headers)?;
        let request_id = request
            .request_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        if request_id.is_empty() || request_id.len() > 128 {
            bail!(
                "{}",
                crate::i18n::ui("Geçersiz istek kimliği", "Invalid request id")
            );
        }
        request.request_id = Some(request_id.clone());
        let control = TransferControl::default();
        // Identity of this analysis, readable after the guard is dropped.
        let inspect_token: u64;
        {
            let mut ui = self.inner.ui.lock().unwrap_or_else(|e| e.into_inner());
            match &launch {
                // The launch is still current: raise its picker, keep its analysis.
                Some((launch_id, ..)) => {
                    if let Some(current) = ui.media_launch.as_mut() {
                        if current.id == *launch_id {
                            current.raise_seq = current.raise_seq.wrapping_add(1);
                            return Ok(());
                        }
                    }
                }
                None if ui.inspect_id.as_deref() == Some(request_id.as_str()) => return Ok(()),
                None => {}
            }
            // A background refresh of an installed toolchain keeps the current tools
            // usable; only a missing toolchain has to finish installing first.
            if ui.installing_tools && !ui.tools.iter().all(|tool| tool.installed) {
                bail!(
                    "{}",
                    crate::i18n::ui(
                        "Önce medya araçlarının kurulmasını bekleyin.",
                        "Wait for the media tools installation to finish first."
                    )
                );
            }
            if let Some(previous) = ui.inspect_control.take() {
                previous.cancel();
            }
            ui.inspecting = true;
            ui.inspect_id = Some(request_id.clone());
            ui.inspect_token = ui.inspect_token.wrapping_add(1);
            inspect_token = ui.inspect_token;
            ui.inspect_error = None;
            ui.inspect_error_code = None;
            ui.inspect_control = Some(control.clone());
            ui.media = None;
            match launch {
                Some((launch_id, launch_request, session_consent)) => {
                    ui.launch_seq = ui.launch_seq.wrapping_add(1);
                    let seq = ui.launch_seq;
                    ui.media_launch = Some(MediaLaunch {
                        id: launch_id,
                        request: *launch_request,
                        session_consent,
                        seq,
                        raise_seq: 0,
                        // The browser that handed this off owns the foreground
                        // now; the picker belongs on that monitor.
                        origin: crate::gui::foreground_monitor(),
                    });
                }
                // A plain inspection replaces whatever handoff was current; its
                // picker closes when it notices the changed selection.
                None => ui.media_launch = None,
            }
        }
        let governor = self.inner.engine.network_governor();
        let app = self.clone();
        std::thread::Builder::new()
            .name("media-inspect".into())
            .spawn(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    media::inspect_controlled(app.paths(), &request, &control, &governor)
                }));
                let result = match outcome {
                    Ok(result) => result,
                    Err(_) => Err(anyhow::anyhow!(
                        "Medya analizi beklenmeyen bir hatayla durdu."
                    )),
                };
                let text = {
                    let mut ui = app.inner.ui.lock().unwrap_or_else(|e| e.into_inner());
                    if ui.inspect_token != inspect_token {
                        // A newer analysis owns the result state; this worker is
                        // stale even when its request id was reused.
                        return;
                    }
                    if ui.inspect_id.as_deref() != Some(request_id.as_str()) {
                        return;
                    }
                    ui.inspecting = false;
                    ui.inspect_control = None;
                    match result {
                        Ok(info) => {
                            let text = format!("Medya bulundu: {}", info.title);
                            ui.media = Some(info);
                            (text, false)
                        }
                        Err(error) => {
                            let code =
                                crate::error_codes::code_of(&error).map(|code| code.to_string());
                            let text = crate::logging::sanitize(&format!("{error:#}"));
                            ui.inspect_error = Some(text.clone());
                            ui.inspect_error_code = code;
                            (text, true)
                        }
                    }
                };
                app.message(text.0, text.1);
            })
            .inspect_err(|_error| {
                let mut ui = self.inner.ui.lock().unwrap_or_else(|e| e.into_inner());
                if ui.inspect_token == inspect_token {
                    ui.inspecting = false;
                }
            })?;
        Ok(())
    }
    pub(crate) fn network_governor(&self) -> crate::network::NetworkGovernor {
        self.inner.engine.network_governor()
    }
    /// Persists settings without a user-visible reply path. Used by the
    /// background updater for its check-stamp and skip-version bookkeeping.
    pub(crate) fn update_settings_quiet(&self, settings: Settings) -> Result<()> {
        self.inner.engine.update_settings(settings)
    }
    pub(crate) fn dispatch_result(
        &self,
        mut action: Action,
    ) -> Result<crate::model::CommandResult> {
        let result = (|| {
            if self.inner.shutdown.load(Ordering::Acquire) {
                bail!(
                    "{}",
                    crate::i18n::ui("Uygulama kapanıyor", "The application is shutting down")
                );
            }
            let mut result = crate::model::CommandResult::default();
            match &mut action {
                Action::Add { request } => {
                    validate_url(&request.url, false)?;
                    result.request_id = request.request_id.clone();
                    result.job_ids = self.inner.engine.add(request.clone())?;
                    let count = result.job_ids.len();
                    self.message(
                        crate::i18n::ui_owned!(
                            format!("{count} indirme eklendi"),
                            format!("{count} download(s) added")
                        ),
                        false,
                    );
                    return Ok(result);
                }
                Action::Inspect { request } => {
                    let id = request
                        .request_id
                        .get_or_insert_with(|| uuid::Uuid::new_v4().to_string());
                    result.request_id = Some(id.clone());
                }
                Action::BrowserMedia { launch_id, .. } => {
                    result.request_id = Some(launch_id.clone());
                }
                Action::InstallTools { request_id, .. } => {
                    let id = request_id.get_or_insert_with(|| uuid::Uuid::new_v4().to_string());
                    result.request_id = Some(id.clone());
                }
                Action::Log { request } => {
                    crate::validation::validate_relay_events(&request.events)?;
                    let (accepted, rejected) = self.record_relay_events(&request.events);
                    result.log = Some(LogReceipt { accepted, rejected });
                    return Ok(result);
                }
                Action::Diagnose { hours } => {
                    let hours = hours.unwrap_or(24).clamp(1, 24 * 30) as i64;
                    result.diagnose = Some(crate::diagnose::summary(self.paths(), hours)?);
                    return Ok(result);
                }
                Action::Capabilities => {
                    let snapshot = self.inner.engine.snapshot();
                    result.capabilities = Some(Capabilities {
                        protocol: 2,
                        capability_version: CAPABILITY_VERSION,
                        usage_modes: snapshot.settings.usage_modes,
                        onboarding_version: snapshot.settings.onboarding_version,
                        settings_revision: snapshot.settings.settings_revision,
                        source_refresh_jobs: snapshot
                            .jobs
                            .iter()
                            .filter(|job| job.state == JobState::AwaitingSource)
                            .filter_map(|job| {
                                let page = job
                                    .request
                                    .source_identity
                                    .as_ref()
                                    .map(|identity| identity.page_url.as_str())
                                    .or(job.request.page_url.as_deref())?;
                                Some(SourceRefreshSummary {
                                    id: job.id.clone(),
                                    page_url: public_page_url(page)?,
                                    title: job.name.clone(),
                                })
                            })
                            .collect(),
                        site_entries: snapshot.settings.site_entries,
                        browser_takeover: snapshot.settings.browser_takeover,
                        completed_jobs: snapshot
                            .jobs
                            .iter()
                            .filter(|job| job.state == JobState::Completed)
                            .count() as u64,
                    });
                    return Ok(result);
                }
                Action::BeginSourceRefresh { id } => {
                    result.source_refresh = Some(self.inner.engine.begin_source_refresh(id)?);
                    return Ok(result);
                }
                Action::CompleteSourceRefresh {
                    id,
                    token,
                    request,
                    restart,
                } => {
                    self.inner.engine.complete_source_refresh(
                        id,
                        token,
                        (**request).clone(),
                        *restart,
                    )?;
                    return Ok(result);
                }
                Action::CreateQueue { name } => {
                    result
                        .queue_ids
                        .push(self.inner.engine.create_queue(name.clone())?);
                    return Ok(result);
                }
                Action::RenameQueue { id, name } => {
                    self.inner.engine.rename_queue(id.clone(), name.clone())?;
                    return Ok(result);
                }
                Action::DeleteQueue { id } => {
                    self.inner.engine.delete_queue(id.clone())?;
                    return Ok(result);
                }
                Action::MoveToQueue { ids, queue_id } => {
                    self.inner
                        .engine
                        .move_to_queue(ids.clone(), queue_id.clone())?;
                    return Ok(result);
                }
                Action::UpdateQueuePolicy { queue } => {
                    self.inner.engine.update_queue_policy(queue.clone())?;
                    return Ok(result);
                }
                Action::CancelCompletionAction { queue_id } => {
                    self.inner
                        .engine
                        .cancel_completion_action(queue_id.clone())?;
                    return Ok(result);
                }
                Action::RunSynchronization { id } => {
                    self.inner.engine.run_synchronization(id.clone())?;
                    return Ok(result);
                }
                Action::CrawlSite { request } => {
                    let governor = self.inner.engine.network_governor();
                    let crawl = crawl_site(
                        request.clone(),
                        &self.inner.engine.snapshot().settings,
                        &governor,
                    )?;
                    result.request_id = Some(crawl.request_id.clone());
                    self.inner
                        .ui
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .crawl_session = Some(crawl.clone());
                    result.crawl = Some(crawl);
                    return Ok(result);
                }
                Action::AddCrawlCandidates {
                    request_id,
                    candidate_ids,
                    queue_id,
                    directory,
                } => {
                    result.job_ids = self.add_crawl_candidates(
                        request_id,
                        candidate_ids,
                        queue_id.clone(),
                        directory.clone(),
                    )?;
                    return Ok(result);
                }
                Action::BrowserTransfer { command } => {
                    result.browser_transfer = Some(crate::browser_transfer::dispatch(
                        self.paths(),
                        &self.inner.engine,
                        command.clone(),
                    )?);
                    return Ok(result);
                }
                _ => {}
            }
            self.dispatch_inner(action)?;
            Ok(result)
        })();
        if let Err(error) = &result {
            self.message(format!("{error:#}"), true);
        }
        result
    }
    fn dispatch_inner(&self, action: Action) -> Result<()> {
        if self.inner.shutdown.load(Ordering::Acquire) {
            bail!(
                "{}",
                crate::i18n::ui("Uygulama kapanıyor", "The application is shutting down")
            );
        }
        match action {
            // Result-producing actions are handled by `dispatch_result`.
            Action::Log { .. } | Action::Diagnose { .. } => {}
            Action::Capabilities
            | Action::BeginSourceRefresh { .. }
            | Action::CompleteSourceRefresh { .. }
            | Action::CreateQueue { .. }
            | Action::RenameQueue { .. }
            | Action::DeleteQueue { .. }
            | Action::MoveToQueue { .. }
            | Action::UpdateQueuePolicy { .. }
            | Action::CancelCompletionAction { .. }
            | Action::RunSynchronization { .. }
            | Action::CrawlSite { .. }
            | Action::AddCrawlCandidates { .. }
            | Action::BrowserTransfer { .. } => {
                bail!(
                    "{}",
                    crate::i18n::ui(
                        "Bu eylem sonuçlu komut yolundan çağrılmalı.",
                        "This action must be called through the result-producing command path."
                    )
                )
            }
            Action::Status => {}
            Action::StatusPage { limit, .. } => {
                if limit == 0 || limit > 500 {
                    bail!(
                        "{}",
                        crate::i18n::ui(
                            "Sayfa büyüklüğü 1–500 olmalı",
                            "Page size must be between 1 and 500"
                        )
                    );
                }
            }
            Action::ToolsStatus { request_id } => {
                let ui = self.inner.ui.lock().unwrap_or_else(|e| e.into_inner());
                if ui.tools_id.as_deref() != Some(request_id.as_str()) {
                    bail!(
                        "{}",
                        crate::i18n::ui("Araç işlemi değişti", "The tools operation changed")
                    )
                }
            }
            Action::Add { request } => {
                validate_url(&request.url, false)?;
                let count = self.inner.engine.add(request)?.len();
                self.message(
                    crate::i18n::ui_owned!(
                        format!("{count} indirme kuyruğa eklendi."),
                        format!("{count} download(s) added to the queue.")
                    ),
                    false,
                );
            }
            Action::Pause { id } => self.inner.engine.pause(&id)?,
            Action::Resume { id } => self.inner.engine.resume(&id)?,
            Action::Remove { id, delete_file } => self.inner.engine.remove(&id, delete_file)?,
            Action::PauseAll => self.inner.engine.pause_all()?,
            Action::SetSpeedLimit { ids, kib } => self.inner.engine.set_speed_limit(ids, kib)?,
            Action::Reorder { ids, top } => self.inner.engine.reorder(ids, top)?,
            Action::MoveBefore { ids, target } => self.inner.engine.move_before(ids, target)?,
            Action::StartNow { id } => self.inner.engine.start_now(&id)?,
            Action::SetOpenWhenDone { id, value } => {
                self.inner.engine.set_open_when_done(&id, value)?
            }
            Action::Rename { id, name } => self.inner.engine.rename(&id, &name)?,
            Action::RetryFailed => self.inner.engine.retry_failed()?,
            Action::ResumeAll => self.inner.engine.resume_all()?,
            Action::ClearCompleted => self.inner.engine.clear_completed()?,
            Action::SetSettings { settings } => {
                validate_settings(&settings)?;
                let log_level = crate::logging::Level::parse(&settings.logging_level)
                    .unwrap_or(crate::logging::Level::Detailed);
                let logging_hosts = settings.logging_hosts;
                std::fs::create_dir_all(&settings.download_dir).context(crate::i18n::ui(
                    "İndirme klasörü oluşturulamadı",
                    "The download directory could not be created",
                ))?;
                let previous = self.inner.engine.snapshot().settings;
                let changed = previous.start_with_windows != settings.start_with_windows;
                if changed {
                    install::set_autostart(self.paths(), settings.start_with_windows)?;
                }
                let ui_language = settings.ui_language.clone();
                if let Err(error) = self.inner.engine.update_settings(settings) {
                    if changed {
                        if let Err(rollback) =
                            install::set_autostart(self.paths(), previous.start_with_windows)
                        {
                            crate::logging::record(
                                crate::logging::Event::warn("app.autostart_rollback").detail(
                                    format!("Otomatik başlatma ayarı geri alınamadı: {rollback:#}"),
                                ),
                            );
                        }
                    }
                    return Err(error);
                }
                // The language switch applies to the running session at once;
                // the next start reads it back from the store.
                crate::i18n::set_language(&ui_language);
                crate::logging::reconfigure(log_level, logging_hosts);
                self.message(
                    crate::i18n::ui("Ayarlar kaydedildi.", "Settings saved.").into(),
                    false,
                );
            }
            Action::InspectStatus { request_id } => {
                let ui = self.inner.ui.lock().unwrap_or_else(|e| e.into_inner());
                if ui.inspect_id.as_deref() != Some(request_id.as_str()) {
                    bail!(
                        "{}",
                        crate::i18n::ui(
                            "Kaynak seçimi değişti. Kaynağı yeniden seçin.",
                            "The source selection changed. Select the source again."
                        )
                    );
                }
            }
            Action::Inspect { request } => {
                self.begin_inspection(request, None)?;
            }
            Action::BrowserMedia {
                launch_id,
                request,
                session_consent,
            } => {
                if launch_id.is_empty() || launch_id.len() > 128 {
                    bail!(
                        "{}",
                        crate::i18n::ui("Geçersiz istek kimliği", "Invalid request id")
                    );
                }
                let inspection = InspectRequest {
                    url: request.url.clone(),
                    request_id: None,
                    headers: request.headers.clone(),
                    session_cookies: request.session_cookies.clone(),
                    referer: request.referer.clone(),
                    page_url: request.page_url.clone(),
                    playlist: request.playlist,
                };
                self.begin_inspection(inspection, Some((launch_id, request, session_consent)))?;
            }
            Action::InstallTools {
                force_update,
                request_id,
            } => {
                let request_id = request_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                if request_id.is_empty() || request_id.len() > 128 {
                    bail!(
                        "{}",
                        crate::i18n::ui(
                            "Geçersiz araç işlem kimliği",
                            "Invalid tools operation id"
                        )
                    );
                }
                if self
                    .inner
                    .engine
                    .snapshot()
                    .jobs
                    .iter()
                    .any(|job| job.state.is_active() && job.request.kind != DownloadKind::File)
                {
                    bail!(
                        "{}",
                        crate::i18n::ui(
                            "Video/ses desteğini değiştirmeden önce etkin indirmeleri duraklatın.",
                            "Pause active downloads before changing video/audio support."
                        )
                    );
                }
                {
                    let mut ui = self.inner.ui.lock().unwrap_or_else(|e| e.into_inner());
                    if ui.installing_tools {
                        bail!(
                            "{}",
                            crate::i18n::ui(
                                "Video/ses desteği zaten hazırlanıyor.",
                                "Video/audio support is already being prepared."
                            )
                        );
                    }
                    if ui.inspecting {
                        bail!(
                            "{}",
                            crate::i18n::ui(
                                "Önce medya analizinin bitmesini bekleyin.",
                                "Wait for the media analysis to finish first."
                            )
                        );
                    }
                    ui.tools_id = Some(request_id);
                    ui.tools_error = None;
                    ui.installing_tools = true;
                    ui.tool_progress = Some(ToolProgress {
                        message: crate::i18n::ui(
                            "Yayın bilgileri kontrol ediliyor…",
                            "Checking release information…",
                        )
                        .into(),
                        ..Default::default()
                    });
                }
                let governor = self.inner.engine.network_governor();
                let app = self.clone();
                std::thread::Builder::new()
                    .name("media-tools".into())
                    .spawn(move || {
                        let outcome =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                tools::ensure(
                                    app.paths(),
                                    force_update,
                                    &governor,
                                    &mut |progress| {
                                        app.inner
                                            .ui
                                            .lock()
                                            .unwrap_or_else(|e| e.into_inner())
                                            .tool_progress = Some(progress);
                                    },
                                )
                            }));
                        let result = match outcome {
                            Ok(result) => result,
                            Err(_) => Err(anyhow::anyhow!(
                                "{}",
                                crate::i18n::ui(
                                    "Video/ses desteği hazırlığı beklenmeyen bir hatayla durdu.",
                                    "Video/audio setup stopped because of an unexpected error."
                                )
                            )),
                        };
                        let text = {
                            let mut ui = app.inner.ui.lock().unwrap_or_else(|e| e.into_inner());
                            ui.installing_tools = false;
                            ui.tools = tools::status(app.paths());
                            match result {
                                Ok(installed) => {
                                    ui.tools = installed;
                                    ui.tool_progress = Some(ToolProgress {
                                        message: crate::i18n::ui(
                                            "Video ve ses desteği hazır.",
                                            "Video and audio support is ready.",
                                        )
                                        .into(),
                                        ..Default::default()
                                    });
                                    (
                                        crate::i18n::ui(
                                            "Video ve ses desteği hazır.",
                                            "Video and audio support is ready.",
                                        )
                                        .into(),
                                        false,
                                    )
                                }
                                Err(error) => {
                                    let text = crate::i18n::ui_owned!(
                                        format!("Video ve ses desteği: {error:#}"),
                                        format!("Video and audio support: {error:#}")
                                    );
                                    ui.tools_error = Some(text.clone());
                                    (text, true)
                                }
                            }
                        };
                        app.message(text.0, text.1);
                    })
                    .inspect_err(|_error| {
                        self.inner
                            .ui
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .installing_tools = false;
                    })?;
            }
            Action::OpenFolder { id } => {
                let snapshot = self.inner.engine.snapshot();
                let folder = if let Some(id) = id {
                    let job = snapshot
                        .jobs
                        .iter()
                        .find(|job| job.id == id)
                        .context(crate::i18n::ui("İndirme bulunamadı", "Download not found"))?;
                    if job.path.is_dir() {
                        job.path.clone()
                    } else {
                        job.path
                            .parent()
                            .context(crate::i18n::ui(
                                "İndirme klasörü bulunamadı",
                                "Download directory not found",
                            ))?
                            .to_path_buf()
                    }
                } else {
                    snapshot.settings.download_dir
                };
                std::fs::create_dir_all(&folder)?;
                open_path(&folder)?;
            }
            Action::OpenFile { id } => {
                let snapshot = self.inner.engine.snapshot();
                let job = snapshot
                    .jobs
                    .iter()
                    .find(|job| job.id == id)
                    .context(crate::i18n::ui("İndirme bulunamadı", "Download not found"))?;
                if job.state != JobState::Completed || !job.path.is_file() {
                    bail!(
                        "{}",
                        crate::i18n::ui(
                            "Yalnızca tamamlanmış ve diskte bulunan bir dosya açılabilir.",
                            "Only a completed file that exists on disk can be opened."
                        )
                    );
                }
                open_path(&job.path)?;
            }
            Action::ExportDiagnostics => {
                let jobs = self
                    .snapshot()
                    .jobs
                    .iter()
                    .map(diagnostic_job)
                    .collect::<Vec<_>>();
                let package = crate::diagnose::package(self.paths(), &jobs, 24)?;
                self.message(
                    crate::i18n::ui_owned!(
                        format!(
                            "Teşhis paketi hazır. Klasör: {} · ZIP: {}",
                            package.folder.display(),
                            package.zip.display()
                        ),
                        format!(
                            "Diagnostic package ready. Folder: {} · ZIP: {}",
                            package.folder.display(),
                            package.zip.display()
                        )
                    ),
                    false,
                );
                open_path(&package.folder)?;
            }
            Action::Report { job_id } => {
                let snapshot = self.snapshot();
                let (message, code, job_id, job_name, request) =
                    match job_id {
                        Some(id) => {
                            let job = snapshot.jobs.iter().find(|job| job.id == id).with_context(
                                || {
                                    crate::i18n::ui_owned!(
                                        format!("İndirme bulunamadı: {id}"),
                                        format!("Download not found: {id}")
                                    )
                                },
                            )?;
                            (
                                job.error.clone().unwrap_or_default(),
                                snapshot.inspect_error_code.clone(),
                                Some(job.id.clone()),
                                Some(job.name.clone()),
                                job.request.clone(),
                            )
                        }
                        None => {
                            let ui = self.inner.ui.lock().unwrap_or_else(|e| e.into_inner());
                            let launch = ui.media_launch.clone();
                            let message = ui.inspect_error.clone().unwrap_or_default();
                            let code = ui.inspect_error_code.clone();
                            drop(ui);
                            let Some(launch) = launch else {
                                bail!(
                                    "{}",
                                    crate::i18n::ui(
                                        "Bildirilecek bir hata bulunamadı",
                                        "No failure was found to report"
                                    )
                                );
                            };
                            (
                                message,
                                code,
                                None,
                                launch.request.filename.clone().or_else(|| {
                                    Some(crate::i18n::ui("Medya analizi", "Media analysis").into())
                                }),
                                launch.request,
                            )
                        }
                    };
                if message.trim().is_empty() {
                    bail!(
                        "{}",
                        crate::i18n::ui(
                            "Bildirilecek bir hata bulunamadı",
                            "No failure was found to report"
                        )
                    );
                }
                // The page a failure came from: a picker-queued media job keeps it
                // in its source identity, a browser handoff in the request. Either
                // way the report stores the redacted address, and only that one is
                // ever read back.
                let site_url = request
                    .source_identity
                    .as_ref()
                    .map(|identity| identity.page_url.as_str())
                    .filter(|value| !value.is_empty())
                    .or_else(|| {
                        request
                            .page_url
                            .as_deref()
                            .filter(|value| !value.is_empty())
                    })
                    .map(|value| {
                        public_page_url(value).unwrap_or_else(|| crate::logging::sanitize(value))
                    });
                let jobs = snapshot.jobs.iter().map(diagnostic_job).collect::<Vec<_>>();
                // The report may have to read the page itself, so it runs off
                // the window thread: a click must never freeze the interface,
                // and the folder opens when the files are written.
                let app = self.clone();
                let paths = self.paths().clone();
                let handoff_html = request.page_html.clone();
                std::thread::Builder::new()
                    .name("failure-report".into())
                    .spawn(move || {
                        let (page_html, page_html_source, page_html_status) = match handoff_html {
                            Some(html) => (Some(html), Some("handoff"), None),
                            None => match site_url.as_deref().and_then(|url| {
                                crate::diagnose::fetch_page_markup(
                                    &app.inner.engine.network_governor(),
                                    url,
                                )
                            }) {
                                Some(page) => (Some(page.html), Some("desktop"), Some(page.status)),
                                None => (None, None, None),
                            },
                        };
                        let context = crate::diagnose::ReportContext {
                            code,
                            message,
                            job_id,
                            job_name,
                            site_url,
                            page_title: None,
                            page_html,
                            page_html_source,
                            page_html_status,
                        };
                        match crate::diagnose::report(&paths, &jobs, 24, &context) {
                            Ok(package) => {
                                app.message(
                                    crate::i18n::ui_owned!(
                                        format!(
                                            "Hata raporu hazır. Klasör: {} · ZIP: {}",
                                            package.folder.display(),
                                            package.zip.display()
                                        ),
                                        format!(
                                            "Failure report ready. Folder: {} · ZIP: {}",
                                            package.folder.display(),
                                            package.zip.display()
                                        )
                                    ),
                                    false,
                                );
                                if let Err(error) = open_path(&package.folder) {
                                    app.message(
                                        crate::i18n::ui_owned!(
                                            format!("Rapor klasörü açılamadı: {error:#}"),
                                            format!(
                                                "The report folder could not be opened: {error:#}"
                                            )
                                        ),
                                        true,
                                    );
                                }
                            }
                            Err(error) => app.message(
                                crate::i18n::ui_owned!(
                                    format!("Hata raporu oluşturulamadı: {error:#}"),
                                    format!("The failure report could not be created: {error:#}")
                                ),
                                true,
                            ),
                        }
                    })
                    .context(crate::i18n::ui(
                        "Hata raporu başlatılamadı",
                        "The failure report could not be started",
                    ))?;
            }
            Action::BrowserSetup => install::open_browser_setup(self.paths())?,
            Action::ShowWindow => {
                let mut ui = self.inner.ui.lock().unwrap_or_else(|e| e.into_inner());
                ui.show_window_seq = ui.show_window_seq.wrapping_add(1);
            }
            Action::Quit => {
                self.inner
                    .ui
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .quit_requested = true;
            }
        }
        Ok(())
    }
    fn add_crawl_candidates(
        &self,
        request_id: &str,
        candidate_ids: &[String],
        queue_id: Option<String>,
        directory: Option<std::path::PathBuf>,
    ) -> Result<Vec<String>> {
        let session = self
            .inner
            .ui
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .crawl_session
            .clone()
            .context(crate::i18n::ui(
                "Site tarama oturumu bulunamadı",
                "Site-crawl session not found",
            ))?;
        if session.request_id != request_id || session.expires_at < chrono::Utc::now().timestamp() {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Site tarama oturumu değişti veya süresi doldu.",
                    "The site-crawl session changed or expired."
                )
            );
        }
        let selected = candidate_ids
            .iter()
            .collect::<std::collections::BTreeSet<_>>();
        if selected.is_empty() || selected.len() != candidate_ids.len() {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Aday seçimi boş veya yinelenmiş.",
                    "The candidate selection is empty or contains duplicates."
                )
            );
        }
        let existing = self.inner.engine.snapshot();
        let mut jobs = Vec::new();
        for id in selected {
            let candidate = session
                .candidates
                .iter()
                .find(|candidate| &candidate.id == id)
                .context(crate::i18n::ui(
                    "Seçilen site adayı bu taramaya ait değil",
                    "The selected candidate does not belong to this crawl",
                ))?;
            if existing
                .jobs
                .iter()
                .any(|job| job.request.url == candidate.url)
            {
                continue;
            }
            let request = AddRequest {
                url: candidate.url.clone(),
                kind: candidate.kind,
                queue_id: queue_id.clone(),
                directory: directory.clone(),
                request_id: Some(format!("crawl:{}:{}", request_id, candidate.id)),
                page_url: Some(candidate.source_page.clone()),
                ..AddRequest::default()
            };
            jobs.extend(self.inner.engine.add(request)?);
        }
        Ok(jobs)
    }

    pub fn shutdown(&self) {
        if self.inner.shutdown.swap(true, Ordering::AcqRel) {
            return;
        }
        {
            let mut ui = self.inner.ui.lock().unwrap_or_else(|e| e.into_inner());
            ui.quit_requested = true;
            if let Some(control) = ui.inspect_control.take() {
                control.cancel();
            }
        }
        self.inner.engine.shutdown();
    }
    /// Records extension events in the session log. A malformed field rejects only its own
    /// event; the batch still lands so one bad entry cannot silence a whole report.
    fn record_relay_events(&self, events: &[crate::model::RelayEvent]) -> (usize, usize) {
        let mut accepted = 0;
        let mut rejected = 0;
        for relayed in events {
            let code = relayed
                .code
                .as_deref()
                .and_then(crate::error_codes::from_text);
            if !crate::logging::valid_event_name(&relayed.event) {
                rejected += 1;
                continue;
            }
            let level = crate::logging::parse_level(relayed.level.as_deref().unwrap_or("info"));
            let mut event = crate::logging::Event::external(relayed.event.clone(), level);
            if let Some(code) = code {
                event = event.code(code);
            }
            if let Some(outcome) = relayed
                .outcome
                .as_deref()
                .and_then(crate::logging::parse_outcome)
            {
                event = event.outcome(outcome);
            }
            if let Some(host) = &relayed.host {
                event = event.host(host.clone());
            }
            if let Some(detail) = &relayed.detail {
                event = event.detail(detail.clone());
            }
            match relayed.job.as_deref() {
                Some(job) if !job.is_empty() => crate::logging::record_job(job, event),
                _ => crate::logging::record(event),
            }
            accepted += 1;
        }
        (accepted, rejected)
    }
    pub(crate) fn message(&self, text: String, error: bool) {
        let text = crate::logging::sanitize(&text);
        let code = crate::error_codes::from_text(&text);
        {
            let mut ui = self.inner.ui.lock().unwrap_or_else(|e| e.into_inner());
            let id = ui.next_message;
            ui.next_message = ui.next_message.wrapping_add(1);
            ui.messages.push(UserMessage {
                id,
                text: text.clone(),
                error,
            });
            if ui.messages.len() > 80 {
                ui.messages.remove(0);
            }
        }
        match (error, code) {
            (true, Some(code)) => crate::logging::record(crate::logging::Event::failure(
                "app.message",
                code,
                text.clone(),
            )),
            (true, None) => crate::logging::record(
                crate::logging::Event::warn("app.message").detail(text.clone()),
            ),
            (false, Some(code)) => crate::logging::record(
                crate::logging::Event::info("app.message")
                    .code(code)
                    .outcome(crate::logging::Outcome::Ok)
                    .detail(text.clone()),
            ),
            (false, None) => crate::logging::record(
                crate::logging::Event::info("app.message").detail(text.clone()),
            ),
        }
        self.hint_when_repeated(code);
    }
    /// The same code failing three times in one session earns its registry advice, so the
    /// user is not left guessing after the third identical failure.
    fn hint_when_repeated(&self, code: Option<crate::error_codes::Code>) {
        let Some(code) = code else {
            return;
        };
        use crate::error_codes::Action;
        let advice = match crate::error_codes::definition(code).map(|entry| entry.action) {
            Some(Action::Retry) => crate::i18n::ui(
                "Yeniden deneyin; aynı hata sürerse kaynağı yenileyin",
                "Try again; refresh the source if the same error persists",
            ),
            Some(Action::Refresh) => crate::i18n::ui(
                "Kaynağı yenileyip yeniden deneyin",
                "Refresh the source and try again",
            ),
            Some(Action::OpenApp) => crate::i18n::ui(
                "SSDownload penceresini açıp durumu denetleyin",
                "Open SSDownload and check its status",
            ),
            Some(Action::OpenPage) => crate::i18n::ui(
                "Videoyu sayfada oynatıp yeniden deneyin",
                "Play the video on the page and try again",
            ),
            Some(Action::Settings) => {
                crate::i18n::ui("Ayarları gözden geçirin", "Review the settings")
            }
            Some(Action::Update) => crate::i18n::ui(
                "Uygulamayı ve eklentiyi güncelleyin",
                "Update the application and extension",
            ),
            Some(Action::Unsupported) => crate::i18n::ui(
                "Bu kaynak desteklenmiyor; başka bir kaynak deneyin",
                "This source is not supported; try another source",
            ),
            Some(Action::Report) => crate::i18n::ui(
                "Tanı paketini oluşturup bu hatayı bildirin",
                "Create a diagnostic package and report this error",
            ),
            Some(Action::None) | None => return,
        };
        let count = {
            let mut ui = self.inner.ui.lock().unwrap_or_else(|e| e.into_inner());
            let count = ui.error_counts.entry(code.0).or_insert(0);
            *count += 1;
            *count
        };
        if count != 3 {
            return;
        }
        let text = crate::logging::sanitize(&crate::i18n::ui_owned!(
            format!("{code} bu oturumda üç kez tekrarlandı: {advice}"),
            format!("{code} has occurred three times in this session: {advice}")
        ));
        {
            let mut ui = self.inner.ui.lock().unwrap_or_else(|e| e.into_inner());
            let id = ui.next_message;
            ui.next_message = ui.next_message.wrapping_add(1);
            ui.messages.push(UserMessage {
                id,
                text: text.clone(),
                error: false,
            });
            if ui.messages.len() > 80 {
                ui.messages.remove(0);
            }
        }
        crate::logging::record(
            crate::logging::Event::info("hint.repeat")
                .code(code)
                .detail(text),
        );
    }
}

fn public_page_url(value: &str) -> Option<String> {
    let mut url = url::Url::parse(value).ok()?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return None;
    }
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);
    Some(url.to_string())
}

fn crawl_site(
    request: SiteCrawlRequest,
    settings: &Settings,
    governor: &crate::network::NetworkGovernor,
) -> Result<SiteCrawlResult> {
    validate_url(&request.url, true)?;
    let policy = request
        .policy
        .unwrap_or_else(|| settings.crawler_policy.clone());
    crate::validation::validate_crawler_policy(&policy)?;
    for kind in &policy.allowed_kinds {
        if !settings.usage_modes.allows(*kind) {
            bail!(
                "{}",
                crate::i18n::ui(
                    "Site tarayıcı kapalı bir kullanım türünü isteyemez.",
                    "The site browser cannot request a disabled usage purpose."
                )
            );
        }
    }
    let root = url::Url::parse(&request.url)?;
    if !root.username().is_empty() || root.password().is_some() {
        bail!(
            "{}",
            crate::i18n::ui(
                "Site tarayıcı kullanıcı bilgisi içeren adresleri açmaz.",
                "The site browser does not open addresses carrying credentials."
            )
        );
    }
    let root_host = root
        .host_str()
        .context(crate::i18n::ui(
            "Site host adı eksik",
            "The site host name is missing",
        ))?
        .to_ascii_lowercase();
    let request_id = request
        .request_id
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    if request_id.is_empty() || request_id.len() > 128 {
        bail!(
            "{}",
            crate::i18n::ui("Geçersiz site tarama kimliği.", "Invalid site-crawl id.")
        );
    }
    let mut pending = std::collections::VecDeque::from([(root.clone(), 0u8)]);
    let mut visited = std::collections::BTreeSet::new();
    let mut candidate_urls = std::collections::BTreeSet::new();
    let mut candidates = Vec::new();
    let mut pages = 0u32;
    let mut total_bytes = 0u64;
    let mut failed_pages = 0u32;
    let mut last_failure: Option<String> = None;
    let mut truncated = false;
    let control = TransferControl::default();
    while let Some((mut page, depth)) = pending.pop_front() {
        page.set_fragment(None);
        if !visited.insert(page.to_string()) {
            continue;
        }
        if pages >= policy.max_pages || total_bytes >= policy.max_total_bytes {
            truncated = true;
            break;
        }
        let mut body = Vec::new();
        let cap = policy
            .max_response_bytes
            .min(policy.max_total_bytes.saturating_sub(total_bytes)) as usize;
        let mut easy = governor.easy_for_url(&page, &control)?;
        // The governor only reserves capacity; the transfer still needs its URL.
        easy.url(page.as_str())?;
        easy.follow_location(false)?;
        easy.fail_on_error(true)?;
        easy.useragent("SSDownload-SiteCrawler/1")?;
        easy.timeout(std::time::Duration::from_secs(20))?;
        let failure = {
            let mut transfer = easy.transfer();
            transfer.write_function(|chunk| {
                let remaining = cap.saturating_sub(body.len());
                let count = remaining.min(chunk.len());
                body.extend_from_slice(&chunk[..count]);
                Ok(count)
            })?;
            transfer.perform().err()
        };
        pages += 1;
        total_bytes = total_bytes.saturating_add(body.len() as u64);
        if let Some(error) = failure {
            failed_pages += 1;
            last_failure = Some(format!("{error:#}"));
            if body.len() < cap {
                continue;
            }
        }
        if body.len() >= cap {
            truncated = true;
        }
        let text = String::from_utf8_lossy(&body);
        for link in html_links(&text) {
            let Ok(mut target) = page.join(&link) else {
                continue;
            };
            target.set_fragment(None);
            if !matches!(target.scheme(), "http" | "https")
                || !target.username().is_empty()
                || target.password().is_some()
            {
                continue;
            }
            let same_host = target
                .host_str()
                .is_some_and(|host| host.eq_ignore_ascii_case(&root_host));
            if policy.same_host_only && !same_host {
                continue;
            }
            if let Some(kind) = candidate_kind(&target, &policy.allowed_kinds) {
                if candidates.len() >= policy.max_candidates as usize {
                    truncated = true;
                    continue;
                }
                if candidate_urls.insert(target.to_string()) {
                    let id = hex::encode(sha2::Sha256::digest(target.as_str().as_bytes()));
                    candidates.push(CrawlCandidate {
                        id,
                        url: target.to_string(),
                        kind,
                        source_page: public_page_url(page.as_str())
                            .unwrap_or_else(|| page.to_string()),
                    });
                }
            } else if same_host && depth < policy.max_depth {
                pending.push_back((target, depth + 1));
            }
        }
    }
    if pages > 0 && pages == failed_pages {
        bail!(
            "{}",
            crate::i18n::ui_owned!(
                format!(
                    "Site taranamadı: {}",
                    last_failure.as_deref().unwrap_or("sayfalar alınamadı")
                ),
                format!(
                    "Site could not be scanned: {}",
                    last_failure
                        .as_deref()
                        .unwrap_or("pages could not be fetched")
                )
            )
        );
    }
    Ok(SiteCrawlResult {
        request_id,
        root_url: public_page_url(root.as_str()).unwrap_or_else(|| root.to_string()),
        candidates,
        pages_scanned: pages,
        bytes_scanned: total_bytes,
        failed_pages,
        last_failure,
        truncated,
        expires_at: chrono::Utc::now().timestamp() + 10 * 60,
    })
}

fn html_links(text: &str) -> Vec<String> {
    let lower = text.to_ascii_lowercase();
    let mut links = Vec::new();
    let mut offset = 0usize;
    while let Some(found) = lower[offset..].find("href") {
        let mut index = offset + found + 4;
        while lower
            .as_bytes()
            .get(index)
            .is_some_and(u8::is_ascii_whitespace)
        {
            index += 1;
        }
        if lower.as_bytes().get(index) != Some(&b'=') {
            offset = index;
            continue;
        }
        index += 1;
        while lower
            .as_bytes()
            .get(index)
            .is_some_and(u8::is_ascii_whitespace)
        {
            index += 1;
        }
        let quote = *text.as_bytes().get(index).unwrap_or(&0);
        if quote != b'\'' && quote != b'"' {
            offset = index;
            continue;
        }
        let start = index + 1;
        let Some(end) = text.as_bytes()[start..]
            .iter()
            .position(|byte| *byte == quote)
        else {
            break;
        };
        let value = &text[start..start + end];
        if value.len() <= 32 * 1024 {
            links.push(value.to_owned());
        }
        offset = start + end + 1;
    }
    links
}

fn candidate_kind(url: &url::Url, allowed: &[DownloadKind]) -> Option<DownloadKind> {
    let extension = url.path().rsplit('.').next()?.to_ascii_lowercase();
    let kind = if matches!(extension.as_str(), "mp4" | "mkv" | "webm" | "m3u8" | "mpd") {
        DownloadKind::Video
    } else if matches!(
        extension.as_str(),
        "mp3" | "m4a" | "aac" | "flac" | "ogg" | "wav"
    ) {
        DownloadKind::Audio
    } else if matches!(
        extension.as_str(),
        "html" | "htm" | "php" | "asp" | "aspx" | "jsp"
    ) {
        return None;
    } else {
        DownloadKind::File
    };
    allowed.contains(&kind).then_some(kind)
}

fn open_path(path: &Path) -> Result<()> {
    use windows_sys::Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL};
    // The shell refuses verbatim paths and answers an unusable target with its own dialog,
    // which the application could neither log nor explain; report it here instead.
    let target = crate::winpath::shell_target(path).map_err(anyhow::Error::msg)?;
    crate::logging::record(
        crate::logging::Event::info("shell.open").detail(format!("path={target}")),
    );
    let value: Vec<u16> = target.encode_utf16().chain(Some(0)).collect();
    let verb: Vec<u16> = "open\0".encode_utf16().collect();
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            value.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    if result as isize <= 32 {
        let code = result as isize;
        bail!(
            "{}",
            crate::i18n::ui_owned!(
                format!("Dosya/klasör açılamadı (Windows kodu {code})."),
                format!("The file/folder could not be opened (Windows code {code}).")
            )
        );
    }
    Ok(())
}
fn diagnostic_job(job: &Job) -> serde_json::Value {
    let context = job
        .error
        .as_deref()
        .map(|error| media::classify_download_error(error, &job.request));
    let phase = context.map(|value| value.0);
    let header_present = |name: &str| {
        job.request
            .headers
            .keys()
            .any(|key| key.eq_ignore_ascii_case(name))
    };
    serde_json::json!({
        "error_code": job
            .error
            .as_deref()
            .and_then(crate::error_codes::from_text)
            .map(|code| code.0),
        "failed_stage": phase.map(diagnostic_failure_stage),
        "request_role": diagnostic_request_role(phase, job.request.kind),
        "error_kind": context.map(|value| diagnostic_error_kind(value.1)),
        "header_presence": {
            "accept": header_present("accept"),
            "accept_language": header_present("accept-language"),
            "user_agent": header_present("user-agent"),
            "origin": header_present("origin"),
            "referer": job.request.referer.is_some() || header_present("referer"),
            "authorization": header_present("authorization"),
            "cookie": header_present("cookie") || !job.request.session_cookies.is_empty(),
            "other": job.request.headers.keys().any(|name| !["accept", "accept-language", "user-agent", "origin", "referer", "authorization", "cookie"].iter().any(|known| name.eq_ignore_ascii_case(known)))
        }
    })
}

fn diagnostic_failure_stage(phase: &str) -> &'static str {
    match phase {
        "keşif" => "discovery",
        "ana liste" | "alt liste" => "manifest",
        "seçilen ses" => "audio",
        "altyazı" => "subtitle",
        "başlatma parçası" => "initialization",
        "medya parçası" => "segment",
        "doğrulama" => "validation",
        _ => "source",
    }
}

fn diagnostic_request_role(phase: Option<&str>, kind: DownloadKind) -> &'static str {
    match phase {
        Some("keşif") => "discovery",
        Some("ana liste") => "manifest_root",
        Some("alt liste") => "manifest_child",
        Some("seçilen ses") => "audio",
        Some("altyazı") => "subtitle",
        Some("başlatma parçası") => "initialization",
        Some("medya parçası") => "media_segment",
        Some("doğrulama") => "output",
        _ if kind == DownloadKind::File => "file",
        _ => "source",
    }
}

fn diagnostic_error_kind(kind: &str) -> &'static str {
    match kind {
        "tls" => "tls",
        "dns" => "dns",
        "access_denied" => "access_denied",
        "not_found" => "not_found",
        "timeout" => "timeout",
        "verification" => "verification",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The session log writer keeps its files open for the process lifetime, so a test that
    /// deletes its data directory can race with an append. Cleanup is best effort; the directory
    /// lives in the system temp folder.
    fn cleanup(root: std::path::PathBuf) {
        for _ in 0..10 {
            if std::fs::remove_dir_all(&root).is_ok() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    fn isolated_app() -> (App, std::path::PathBuf) {
        let root =
            std::env::temp_dir().join(format!("ssdownload-app-test-{}", uuid::Uuid::new_v4()));
        let paths = AppPaths::new(root.clone()).unwrap();
        (App::open(paths).unwrap(), root)
    }

    /// Regression: the crawler reserved governor capacity but never set the request URL,
    /// so every page failed instantly and the crawl reported an empty result.
    #[test]
    fn site_crawl_fetches_pages_and_reports_candidates() {
        use std::io::{Read as _, Write as _};
        use std::time::{Duration, Instant};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let body: &[u8] = b"<html><body><a href=\"/movie.mp4\">video</a><a href=\"/page2.html\">next</a></body></html>";
        let served = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let server = {
            use std::sync::atomic::Ordering as AtomicOrdering;
            let served = served.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut handlers = Vec::new();
                while served.load(AtomicOrdering::Relaxed) < 2 && Instant::now() < deadline {
                    let Ok((mut stream, _)) = listener.accept() else {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    };
                    // One handler per connection: the transfer governor can pre-open a
                    // socket and use it a moment later, so a connection that has not sent
                    // anything yet must stay open instead of aborting its peer.
                    let served = served.clone();
                    let stop = stop.clone();
                    handlers.push(std::thread::spawn(move || {
                        let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
                        let mut request = Vec::new();
                        let mut buffer = [0u8; 512];
                        while !stop.load(AtomicOrdering::Relaxed) {
                            match stream.read(&mut buffer) {
                                Ok(0) => break,
                                Ok(count) => {
                                    request.extend_from_slice(&buffer[..count]);
                                    if !request.windows(4).any(|window| window == b"\r\n\r\n") {
                                        continue;
                                    }
                                    let head = format!(
                                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                        body.len()
                                    );
                                    if stream.write_all(head.as_bytes()).is_ok()
                                        && stream.write_all(body).is_ok()
                                        && stream.flush().is_ok()
                                    {
                                        served.fetch_add(1, AtomicOrdering::Relaxed);
                                    }
                                    break;
                                }
                                Err(_) => {}
                            }
                        }
                    }));
                }
                let served = served.load(AtomicOrdering::Relaxed);
                stop.store(true, AtomicOrdering::Relaxed);
                for handler in handlers {
                    let _ = handler.join();
                }
                served
            })
        };

        let settings = Settings::default();
        let governor = crate::network::NetworkGovernor::new(2);
        let result = crawl_site(
            SiteCrawlRequest {
                url: format!("http://{address}/index.html"),
                request_id: Some("crawl-regression".into()),
                policy: Some(SiteCrawlerPolicy {
                    max_depth: 1,
                    same_host_only: true,
                    allowed_kinds: vec![DownloadKind::File, DownloadKind::Video],
                    max_pages: 4,
                    max_candidates: 10,
                    max_response_bytes: 256 * 1024,
                    max_total_bytes: 1024 * 1024,
                }),
            },
            &settings,
            &governor,
        )
        .expect("crawl should reach the local server");

        assert_eq!(result.failed_pages, 0, "{:?}", result.last_failure);
        assert!(result.pages_scanned >= 2, "walked the linked page too");
        assert!(
            result
                .candidates
                .iter()
                .any(|candidate| candidate.url.ends_with("/movie.mp4")),
            "expected the linked media candidate: {:?}",
            result.candidates
        );
        assert!(server.join().unwrap() >= 2, "server served both pages");
    }

    #[test]
    fn add_ack_returns_a_stable_job_id_for_the_same_request_id() {
        let (app, root) = isolated_app();
        let request = AddRequest {
            url: "https://example.invalid/scheduled-file.bin".into(),
            request_id: Some("repeatable-add".into()),
            // Keep this test hermetic: the engine schedules the job and performs no network I/O.
            start_at: Some(chrono::Utc::now().timestamp() + 3_600),
            ..Default::default()
        };
        let first = app
            .dispatch_result(Action::Add {
                request: request.clone(),
            })
            .unwrap();
        let repeated = app.dispatch_result(Action::Add { request }).unwrap();
        assert_eq!(first.request_id.as_deref(), Some("repeatable-add"));
        assert_eq!(first.job_ids.len(), 1);
        assert_eq!(first.job_ids, repeated.job_ids);

        app.shutdown();
        cleanup(root);
    }

    #[test]
    fn diagnostic_export_contains_only_stable_failure_metadata() {
        let (app, root) = isolated_app();
        let added = app
            .dispatch_result(Action::Add {
                request: AddRequest {
                    url: "https://example.invalid/file.bin".into(),
                    kind: DownloadKind::File,
                    start_at: Some(chrono::Utc::now().timestamp() + 3_600),
                    ..Default::default()
                },
            })
            .unwrap();
        let mut job = app
            .snapshot()
            .jobs
            .iter()
            .find(|job| job.id == added.job_ids[0])
            .cloned()
            .unwrap();
        app.shutdown();
        cleanup(root);

        job.name = "SENSITIVE-NAME.bin".into();
        job.path = std::path::PathBuf::from(r"C:\SENSITIVE-DIRECTORY\SENSITIVE-NAME.bin");
        job.request.url = "https://example.invalid/file.bin?token=SENSITIVE-URL".into();
        job.request.filename = Some("SENSITIVE-NAME.bin".into());
        job.request.directory = Some(std::path::PathBuf::from(r"C:\SENSITIVE-DIRECTORY"));
        job.request.request_id = Some("SENSITIVE-REQUEST-ID".into());
        job.request.referer =
            Some("https://example.invalid/watch?session=SENSITIVE-REFERER".into());
        job.request.page_url = Some("https://example.invalid/page?token=SENSITIVE-PAGE".into());
        job.request.source_identity = Some(SourceIdentity {
            video_id: "SENSITIVE-VIDEO-ID".into(),
            frame_id: 7,
            document_id: Some("SENSITIVE-DOCUMENT-ID".into()),
            page_url: "https://example.invalid/source?token=SENSITIVE-SOURCE".into(),
        });
        job.request.headers = [
            ("accept".into(), "SENSITIVE-ACCEPT".into()),
            ("accept-language".into(), "SENSITIVE-LANGUAGE".into()),
            ("user-agent".into(), "SENSITIVE-AGENT".into()),
            (
                "origin".into(),
                "https://example.invalid/SENSITIVE-ORIGIN".into(),
            ),
            ("authorization".into(), "SENSITIVE-AUTHORIZATION".into()),
            ("x-private".into(), "SENSITIVE-HEADER".into()),
        ]
        .into();
        job.request.session_cookies = vec![ScopedCookie {
            name: "SENSITIVE-COOKIE-NAME".into(),
            value: "SENSITIVE-COOKIE-VALUE".into(),
            domain: "sensitive.example.invalid".into(),
            path: "/SENSITIVE-COOKIE-PATH".into(),
            secure: true,
            http_only: true,
            host_only: true,
            expires: None,
            store_id: Some("SENSITIVE-COOKIE-STORE".into()),
            partition_key: Some("https://sensitive.example.invalid".into()),
        }];
        job.request.external_subtitles = vec![ExternalSubtitle {
            url: "https://example.invalid/subtitle?token=SENSITIVE-SUBTITLE".into(),
            language: "en".into(),
            label: "SENSITIVE-SUBTITLE-LABEL".into(),
            kind: "subtitles".into(),
            is_default: true,
            headers: [(
                "x-subtitle-private".into(),
                "SENSITIVE-SUBTITLE-HEADER".into(),
            )]
            .into(),
            referer: Some(
                "https://example.invalid/subtitle-page?token=SENSITIVE-SUBTITLE-REFERER".into(),
            ),
        }];
        job.error = Some(
            "fragment 7: HTTP Error 403: https://cdn.example/part?token=SENSITIVE-ERROR".into(),
        );

        let exported = diagnostic_job(&job);
        assert_eq!(exported.as_object().unwrap().len(), 5);
        assert_eq!(exported["error_code"], serde_json::Value::Null);
        assert_eq!(exported["failed_stage"], "segment");
        assert_eq!(exported["error_kind"], "access_denied");
        assert_eq!(exported["request_role"], "media_segment");
        assert_eq!(exported["header_presence"].as_object().unwrap().len(), 8);
        assert_eq!(exported["header_presence"]["accept"], true);
        assert_eq!(exported["header_presence"]["accept_language"], true);
        assert_eq!(exported["header_presence"]["user_agent"], true);
        assert_eq!(exported["header_presence"]["origin"], true);
        assert_eq!(exported["header_presence"]["referer"], true);
        assert_eq!(exported["header_presence"]["authorization"], true);
        assert_eq!(exported["header_presence"]["cookie"], true);
        assert_eq!(exported["header_presence"]["other"], true);
        assert!(!exported.to_string().contains("SENSITIVE-"));

        job.error = Some("SSD-TRF-003 parça doğrulaması başarısız".into());
        assert_eq!(diagnostic_job(&job)["error_code"], "SSD-TRF-003");

        for (error, stage, role, kind) in [
            (
                "manifest HTTP Error 404",
                "manifest",
                "manifest_root",
                "not_found",
            ),
            ("seçilen ses timed out", "audio", "audio", "timeout"),
            (
                "initialization TLS failure",
                "initialization",
                "initialization",
                "tls",
            ),
            (
                "fragment HTTP Error 403",
                "segment",
                "media_segment",
                "access_denied",
            ),
            (
                "verification checksum mismatch",
                "validation",
                "output",
                "verification",
            ),
            (
                "medya analizi başarısız: resolve host",
                "discovery",
                "discovery",
                "dns",
            ),
        ] {
            job.request.url = if stage == "discovery" {
                "https://page.example/watch".into()
            } else {
                "https://cdn.example/master.m3u8".into()
            };
            job.error = Some(error.into());
            let exported = diagnostic_job(&job);
            assert_eq!(exported["failed_stage"], stage);
            assert_eq!(exported["request_role"], role);
            assert_eq!(exported["error_kind"], kind);
        }

        job.request.kind = DownloadKind::Video;
        job.error = Some("connection reset for https://cdn.example/403404.bin".into());
        let exported = diagnostic_job(&job);
        assert_eq!(exported["failed_stage"], "source");
        assert_eq!(exported["request_role"], "source");
        assert_eq!(exported["error_kind"], "other");
    }

    #[test]
    fn status_page_enforces_the_public_page_limit() {
        let (app, root) = isolated_app();
        assert!(app
            .dispatch_result(Action::StatusPage {
                offset: 0,
                limit: 1,
            })
            .is_ok());
        assert!(app
            .dispatch_result(Action::StatusPage {
                offset: 0,
                limit: 0,
            })
            .is_err());
        assert!(app
            .dispatch_result(Action::StatusPage {
                offset: 0,
                limit: 501,
            })
            .is_err());

        app.shutdown();
        cleanup(root);
    }

    /// Recent human lines the process-global writer rendered, regardless of which data
    /// directory owns the log files. The writer is started once per process, so this ring is
    /// the observation that survives another test winning the startup race.
    fn relay_line_seen(marker: &str) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if crate::logging::recent(200)
                .iter()
                .any(|line| line.contains(marker))
            {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }

    /// JSONL values this test's own data directory received, or `None` when another test in this
    /// binary won the process-global logger startup race: only the winning directory gets files,
    /// and a losing `App::open` can still have created its own (empty) `logs/` first. Callers
    /// assert the file-level contract only when this returns `Some`; the receipt and the recent
    /// lines are the contract that always holds.
    fn relay_lines(root: &std::path::Path, marker: &str) -> Option<Vec<serde_json::Value>> {
        let logs = root.join("logs");
        if !logs.exists() {
            return None;
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let mut values: Vec<serde_json::Value> = Vec::new();
            if let Ok(entries) = std::fs::read_dir(&logs) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name.starts_with("events-") && name.ends_with(".jsonl") {
                        if let Ok(text) = std::fs::read_to_string(entry.path()) {
                            values.extend(
                                text.lines()
                                    .filter_map(|line| serde_json::from_str(line).ok()),
                            );
                        }
                    }
                }
            }
            if values
                .iter()
                .any(|value| value["event"].as_str() == Some(marker))
            {
                return Some(values);
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }

    #[test]
    fn relay_log_receipt_and_lines_cover_accepted_events() {
        let (app, root) = isolated_app();
        let job = "relayjob0001";
        let result = app
            .dispatch_result(Action::Log {
                request: LogBatch {
                    events: vec![
                        RelayEvent {
                            event: "relay.capture.ok".into(),
                            level: Some("info".into()),
                            outcome: Some("ok".into()),
                            code: Some("SSD-EXT-010".into()),
                            host: Some("site.example".into()),
                            job: Some(job.into()),
                            detail: Some("capture relayed from the extension".into()),
                        },
                        RelayEvent {
                            event: "relay.page.scanned".into(),
                            level: Some("warn".into()),
                            outcome: Some("partial".into()),
                            code: None,
                            host: Some("cdn.example".into()),
                            job: None,
                            detail: None,
                        },
                    ],
                },
            })
            .unwrap();
        let receipt = result.log.expect("relay batches return a receipt");
        assert_eq!(receipt.accepted, 2);
        assert_eq!(receipt.rejected, 0);
        if crate::logging::session_id().is_some() {
            assert!(
                relay_line_seen("relay.capture.ok"),
                "the relayed event should reach the session writer"
            );
        }

        if let Some(lines) = relay_lines(&root, "relay.capture.ok") {
            let captured = lines
                .iter()
                .find(|value| value["event"].as_str() == Some("relay.capture.ok"))
                .expect("the relayed event line should be written");
            assert_eq!(captured["code"], "SSD-EXT-010");
            assert_eq!(captured["job"], job);
            let job_log = root
                .join("logs")
                .join("jobs")
                .join(format!("job-{job}.jsonl"));
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while !job_log.exists() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            assert!(job_log.exists(), "{}", job_log.display());
        }

        app.shutdown();
        cleanup(root);
    }

    /// One malformed event name is counted as rejected while the rest of the batch is still
    /// recorded: a single bad entry must not silence a whole report.
    #[test]
    fn relay_log_counts_a_malformed_event_name_as_rejected() {
        let (app, root) = isolated_app();
        let receipt = app
            .dispatch_result(Action::Log {
                request: LogBatch {
                    events: vec![
                        RelayEvent {
                            event: "relay.capture.partial".into(),
                            level: None,
                            outcome: Some("partial".into()),
                            code: Some("SSD-EXT-010".into()),
                            host: None,
                            job: None,
                            detail: None,
                        },
                        RelayEvent {
                            event: "Site.Event".into(),
                            level: None,
                            outcome: None,
                            code: None,
                            host: None,
                            job: None,
                            detail: None,
                        },
                    ],
                },
            })
            .expect("a malformed name must not refuse the batch");
        assert_eq!(
            receipt.log.as_ref().map(|log| (log.accepted, log.rejected)),
            Some((1, 1))
        );

        app.shutdown();
        cleanup(root);
    }
}
