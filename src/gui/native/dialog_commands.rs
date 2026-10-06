//! Command handling of the management dialogs, the dialog command router and settings collection.

use super::*;

pub(super) unsafe fn queue_command(
    hwnd: HWND,
    app: &App,
    f: &mut QueueFields,
    id: i32,
    code: u16,
) -> anyhow::Result<()> {
    let snapshot = app.snapshot();
    let selected = selected_management_id(f.list, &f.ids).map(str::to_owned);
    if id == Q_LIST {
        if code as u32 == LBN_SELCHANGE {
            if let Some(queue) = snapshot
                .settings
                .queues
                .iter()
                .find(|q| Some(&q.id) == selected.as_ref())
            {
                populate_queue_fields(f, queue)?;
            }
        }
        return Ok(());
    }
    let action = match id {
        Q_CREATE => Action::CreateQueue {
            name: text(f.name).trim().into(),
        },
        Q_RENAME => Action::RenameQueue {
            id: selected.clone().ok_or_else(|| {
                anyhow::anyhow!(crate::i18n::ui("Bir kuyruk seçin", "Select a queue"))
            })?,
            name: text(f.name).trim().into(),
        },
        Q_DELETE => Action::DeleteQueue {
            id: selected.clone().ok_or_else(|| {
                anyhow::anyhow!(crate::i18n::ui("Bir kuyruk seçin", "Select a queue"))
            })?,
        },
        Q_MOVE_JOB => Action::MoveToQueue {
            ids: vec![f.job_id.clone().ok_or_else(|| {
                anyhow::anyhow!(crate::i18n::ui(
                    "Önce ana listede taşınacak işi seçin",
                    "Select the job to move in the main list first",
                ))
            })?],
            queue_id: selected.clone().ok_or_else(|| {
                anyhow::anyhow!(crate::i18n::ui(
                    "Hedef kuyruğu seçin",
                    "Select the target queue"
                ))
            })?,
        },
        Q_CANCEL_COMPLETION => Action::CancelCompletionAction {
            queue_id: snapshot
                .completion_countdown
                .as_ref()
                .map(|c| c.queue_id.clone())
                .ok_or_else(|| {
                    anyhow::anyhow!(crate::i18n::ui(
                        "Etkin geri sayım yok",
                        "No countdown is active"
                    ))
                })?,
        },
        Q_UPDATE => {
            let mut queue = snapshot
                .settings
                .queues
                .iter()
                .find(|q| Some(&q.id) == selected.as_ref())
                .cloned()
                .ok_or_else(|| {
                    anyhow::anyhow!(crate::i18n::ui(
                        "Güncellenecek kuyruğu seçin",
                        "Select the queue to update",
                    ))
                })?;
            queue.name = text(f.name).trim().into();
            queue.enabled = check(f.enabled);
            queue.concurrency = text(f.concurrency).trim().parse()?;
            queue.windows.clear();
            for line in text(f.windows)
                .lines()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                let fields: Vec<_> = line.split_whitespace().collect();
                if fields.len() != 2 {
                    anyhow::bail!(crate::i18n::ui(
                        "Pencere biçimi: 0,1,2,3,4 22:00-06:00 veya * 09:00-17:00",
                        "Window format: 0,1,2,3,4 22:00-06:00 or * 09:00-17:00",
                    ));
                }
                let (start, end) = fields[1].split_once('-').ok_or_else(|| {
                    anyhow::anyhow!(crate::i18n::ui(
                        "Pencerede başlangıç-bitiş gerekli",
                        "The window needs a start and an end",
                    ))
                })?;
                let weekdays = if fields[0] == "*" {
                    Vec::new()
                } else {
                    fields[0]
                        .split(',')
                        .map(str::parse)
                        .collect::<std::result::Result<Vec<u8>, _>>()?
                };
                queue.windows.push(crate::model::QueueWindow {
                    weekdays,
                    start: start.into(),
                    end: end.into(),
                });
            }
            let limit: u64 = text(f.quota).trim().parse()?;
            queue.quota = if limit == 0 {
                None
            } else {
                let mut quota = queue.quota.take().unwrap_or_default();
                quota.limit_bytes = limit;
                Some(quota)
            };
            let countdown_seconds = text(f.delay).trim().parse::<u32>()?;
            queue.completion = match combo_index(f.completion) {
                0 => CompletionAction::None,
                1 => CompletionAction::Notify,
                2 => CompletionAction::ShutdownComputer { countdown_seconds },
                3 => CompletionAction::RunProgram {
                    program: PathBuf::from(text(f.program).trim()),
                    arguments: serde_json::from_str(&text(f.arguments))?,
                    countdown_seconds,
                },
                _ => anyhow::bail!(crate::i18n::ui(
                    "Bitiş eylemi seçin",
                    "Select a completion action",
                )),
            };
            crate::validation::validate_queue(&queue)?;
            Action::UpdateQueuePolicy { queue }
        }
        _ => return Ok(()),
    };
    app.dispatch(action)?;
    refresh_queue_controls(hwnd, f, &app.snapshot())?;
    Ok(())
}
pub(super) unsafe fn rule_command(
    app: &App,
    f: &mut RuleFields,
    id: i32,
    code: u16,
) -> anyhow::Result<()> {
    let mut settings = app.snapshot().settings.clone();
    let selected = selected_management_id(f.list, &f.ids).map(str::to_owned);
    if id == R_LIST && code as u32 == LBN_SELCHANGE {
        if let Some(rule) = settings
            .folder_rules
            .iter()
            .find(|r| Some(&r.id) == selected.as_ref())
        {
            set_text(f.host, &rule.host);
            set_text(f.dir, &rule.destination.to_string_lossy());
            set_text(f.priority, &rule.priority.to_string());
            set_check(f.subdomains, rule.include_subdomains);
            set_check(f.file, rule.kinds.contains(&DownloadKind::File));
            set_check(f.video, rule.kinds.contains(&DownloadKind::Video));
            set_check(f.audio, rule.kinds.contains(&DownloadKind::Audio));
        }
        return Ok(());
    }
    match id {
        R_NEW => {
            SendMessageW(f.list, LB_SETCURSEL, usize::MAX, 0);
            set_text(f.host, "");
            set_text(f.dir, "");
            set_text(f.priority, "0");
            set_check(f.subdomains, false);
            set_check(f.file, false);
            set_check(f.video, false);
            set_check(f.audio, false);
            return Ok(());
        }
        R_SAVE => {
            let mut kinds = Vec::new();
            for (field, kind) in [
                (f.file, DownloadKind::File),
                (f.video, DownloadKind::Video),
                (f.audio, DownloadKind::Audio),
            ] {
                if check(field) {
                    kinds.push(kind);
                }
            }
            let rule = FolderRule {
                id: selected
                    .clone()
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                host: text(f.host).trim().into(),
                include_subdomains: check(f.subdomains),
                kinds,
                destination: PathBuf::from(text(f.dir).trim()),
                priority: text(f.priority).trim().parse()?,
                enabled: true,
            };
            if let Some(old) = settings.folder_rules.iter_mut().find(|r| r.id == rule.id) {
                *old = rule;
            } else {
                settings.folder_rules.push(rule);
            }
        }
        R_REMOVE => {
            let selected = selected.ok_or_else(|| {
                anyhow::anyhow!(crate::i18n::ui(
                    "Silinecek kuralı seçin",
                    "Select the rule to delete",
                ))
            })?;
            settings.folder_rules.retain(|r| r.id != selected);
        }
        _ => return Ok(()),
    }
    app.dispatch(Action::SetSettings { settings })?;
    refresh_rule_list(f, &app.snapshot().settings.folder_rules);
    Ok(())
}
pub(super) unsafe fn sync_command(
    app: &App,
    f: &mut SyncFields,
    id: i32,
    code: u16,
) -> anyhow::Result<()> {
    let mut settings = app.snapshot().settings.clone();
    let selected = selected_management_id(f.list, &f.ids).map(str::to_owned);
    if id == Y_LIST && code as u32 == LBN_SELCHANGE {
        if let Some(policy) = settings
            .synchronization_policies
            .iter()
            .find(|p| Some(&p.id) == selected.as_ref())
        {
            set_text(f.url, &policy.url);
            set_text(f.dir, &policy.destination.to_string_lossy());
            set_text(f.interval, &policy.interval_minutes.to_string());
            set_check(f.enabled, policy.enabled);
            set_check(f.overwrite, policy.overwrite);
        }
        return Ok(());
    }
    match id {
        Y_NEW => {
            SendMessageW(f.list, LB_SETCURSEL, usize::MAX, 0);
            set_text(f.url, "");
            set_text(f.dir, "");
            set_text(f.interval, "60");
            set_check(f.enabled, true);
            set_check(f.overwrite, false);
            return Ok(());
        }
        Y_RUN => {
            app.dispatch(Action::RunSynchronization {
                id: selected.ok_or_else(|| {
                    anyhow::anyhow!(crate::i18n::ui(
                        "Kontrol edilecek kaydı seçin",
                        "Select the record to check",
                    ))
                })?,
            })?;
            return Ok(());
        }
        Y_REMOVE => {
            let selected = selected.ok_or_else(|| {
                anyhow::anyhow!(crate::i18n::ui(
                    "Kaldırılacak kaydı seçin",
                    "Select the record to remove",
                ))
            })?;
            settings
                .synchronization_policies
                .retain(|p| p.id != selected);
        }
        Y_SAVE => {
            let mut policy = settings
                .synchronization_policies
                .iter()
                .find(|p| Some(&p.id) == selected.as_ref())
                .cloned()
                .unwrap_or_else(|| SyncPolicy {
                    id: uuid::Uuid::new_v4().to_string(),
                    ..Default::default()
                });
            let url = text(f.url).trim().to_string();
            let destination = PathBuf::from(text(f.dir).trim());
            if policy.url != url || policy.destination != destination {
                policy.etag = None;
                policy.last_modified = None;
                policy.content_sha256 = None;
                policy.last_checked_at = None;
                policy.last_error = None;
            }
            policy.url = url;
            policy.destination = destination;
            policy.interval_minutes = text(f.interval).trim().parse()?;
            policy.enabled = check(f.enabled);
            policy.overwrite = check(f.overwrite);
            if let Some(old) = settings
                .synchronization_policies
                .iter_mut()
                .find(|p| p.id == policy.id)
            {
                *old = policy;
            } else {
                settings.synchronization_policies.push(policy);
            }
        }
        _ => return Ok(()),
    }
    app.dispatch(Action::SetSettings { settings })?;
    refresh_sync_list(f, &app.snapshot().settings.synchronization_policies);
    Ok(())
}
pub(super) unsafe fn crawler_command(
    hwnd: HWND,
    app: &App,
    f: &mut CrawlerFields,
    id: i32,
) -> anyhow::Result<()> {
    match id {
        C_SCAN => {
            if f.pending.is_some() {
                return Ok(());
            }
            let mut policy = app.snapshot().settings.crawler_policy.clone();
            policy.max_depth = text(f.depth).trim().parse()?;
            policy.max_pages = text(f.pages).trim().parse()?;
            policy.max_candidates = text(f.candidates).trim().parse()?;
            crate::validation::validate_crawler_policy(&policy)?;
            let request = SiteCrawlRequest {
                url: text(f.url).trim().into(),
                request_id: Some(uuid::Uuid::new_v4().to_string()),
                policy: Some(policy),
            };
            let (sender, receiver) = std::sync::mpsc::sync_channel(1);
            let app = app.clone();
            std::thread::Builder::new()
                .name("ssdownload-crawl-ui".into())
                .spawn(move || {
                    let result = app
                        .dispatch_result(Action::CrawlSite { request })
                        .and_then(|r| {
                            r.crawl.ok_or_else(|| {
                                anyhow::anyhow!(crate::i18n::ui(
                                    "Tarama sonucu eksik",
                                    "The scan result is missing",
                                ))
                            })
                        })
                        .map_err(|e| format!("{e:#}"));
                    let _ = sender.send(result);
                })?;
            f.pending = Some(receiver);
            f.result = None;
            SendMessageW(f.list, LB_RESETCONTENT, 0, 0);
            EnableWindow(GetDlgItem(hwnd, C_SCAN), 0);
            EnableWindow(GetDlgItem(hwnd, C_ADD), 0);
            SetTimer(hwnd, TIMER_DIALOG, 300, None);
        }
        C_ADD => {
            let result = f.result.as_ref().ok_or_else(|| {
                anyhow::anyhow!(crate::i18n::ui(
                    "Önce taramayı tamamlayın",
                    "Finish the scan first",
                ))
            })?;
            let candidate_ids = selected_list_indexes(f.list)
                .into_iter()
                .filter_map(|index| result.candidates.get(index).map(|c| c.id.clone()))
                .collect::<Vec<_>>();
            if candidate_ids.is_empty() {
                anyhow::bail!(crate::i18n::ui(
                    "Eklenecek adayları açıkça seçin",
                    "Explicitly select the candidates to add",
                ));
            }
            app.dispatch(Action::AddCrawlCandidates {
                request_id: result.request_id.clone(),
                candidate_ids,
                queue_id: None,
                directory: None,
            })?;
        }
        _ => {}
    }
    Ok(())
}

/// A picker answers only for the analysis generation it was populated from.
/// A newer handoff or analysis has replaced what is on screen, so queueing or
/// rebinding from it would bind the wrong source; the timer destroys the
/// dialog for the same reason, and this covers the messages that arrive first.
pub(super) unsafe fn media_generation_current(dlg: &DialogUi, f: &MediaFields) -> bool {
    if f.inspect_generation == dlg.app.snapshot().inspect_generation {
        return true;
    }
    set_text(
        dlg.error,
        crate::i18n::ui(
            "Bu seçim yeni bir analizle değiştirildi; pencere kapanıyor.",
            "This selection was replaced by a newer analysis; the window is closing.",
        ),
    );
    false
}

pub(super) unsafe fn dialog_command(dlg: &mut DialogUi, id: i32, _code: u16) {
    if id == IDCANCEL || id == D_CANCEL {
        DestroyWindow(dlg.hwnd);
        return;
    }
    let dlg_ptr = dlg as *mut DialogUi;
    // Set by the picker when its selection really reached the queue: the window
    // is turned into the job view of those exact jobs after the state that
    // submitted them has been handed over.
    let mut queued: Option<Vec<String>> = None;
    match &mut (*dlg_ptr).kind {
        DialogKind::Wizard(f) => match id {
            ID_WIZARD_SIMPLE | ID_WIZARD_ADVANCED => {
                // The two cards behave as one radio group: the clicked card
                // stays checked and the other is cleared.
                let simple = id == ID_WIZARD_SIMPLE;
                set_check(f.simple_card, simple);
                set_check(f.advanced_card, !simple);
                set_text(dlg.error, "");
                EnableWindow(f.start, wizard_start_enabled(f) as BOOL);
                // The purpose row belongs to the advanced answer.
                layout_dialog(dlg);
            }
            ID_WIZARD_VIDEO | ID_WIZARD_FILE | ID_WIZARD_AUDIO => {
                let any = check(f.video) || check(f.file) || check(f.audio);
                EnableWindow(f.start, wizard_start_enabled(f) as BOOL);
                set_text(
                    dlg.error,
                    if any {
                        ""
                    } else {
                        crate::i18n::ui(
                            "En az bir kullanım amacı seçin.",
                            "Select at least one usage mode.",
                        )
                    },
                );
            }
            ID_WIZARD_START => {
                let simple = check(f.simple_card);
                // A simple installation keeps every purpose enabled; the
                // detailed answer only exists in the advanced choice.
                let modes = if simple {
                    UsageModes {
                        video: true,
                        file: true,
                        audio: true,
                    }
                } else {
                    UsageModes {
                        video: check(f.video),
                        file: check(f.file),
                        audio: check(f.audio),
                    }
                };
                if let Err(error) = modes.validate() {
                    set_dialog_error(dlg.hwnd, dlg.error, error, f.video);
                    return;
                }
                let mut settings = dlg.app.snapshot().settings.clone();
                settings.usage_modes = modes;
                settings.ui_mode = if simple {
                    UiMode::Simple
                } else {
                    UiMode::Advanced
                }
                .as_str()
                .into();
                settings.onboarding_version = ONBOARDING_VERSION;
                match dlg.app.dispatch(Action::SetSettings { settings }) {
                    Ok(()) => {
                        DestroyWindow(dlg.hwnd);
                    }
                    Err(error) => set_dialog_error(
                        dlg.hwnd,
                        dlg.error,
                        &crate::i18n::ui_owned!(
                            format!("Kurulum seçimi kaydedilemedi: {error:#}"),
                            format!("The setup choice could not be saved: {error:#}")
                        ),
                        f.start,
                    ),
                };
            }
            D_CANCEL => {
                DestroyWindow(dlg.hwnd);
            }
            _ => {}
        },
        DialogKind::Add(f) => match id {
            D_BROWSE => {
                if let Some(p) = browse_folder(dlg.hwnd, &text(f.dir)) {
                    set_text(f.dir, &p.to_string_lossy());
                }
            }
            D_ADVANCED => {
                f.advanced = !f.advanced;
                resize_dialog(
                    dlg.hwnd,
                    dlg.dpi,
                    if f.advanced { 420 } else { ADD_DIALOG_HEIGHT },
                );
                layout_dialog(dlg);
            }
            D_ADD_OK => match collect_add(f, false) {
                Ok(mut requests) => {
                    let modes = dlg.app.snapshot().settings.usage_modes;
                    if let Some(request) = requests
                        .iter()
                        .find(|request| !mode_allows_new_job(modes, request.kind))
                    {
                        set_dialog_error(dlg.hwnd, dlg.error, mode_error(request.kind), f.kind);
                        return;
                    }
                    if requests.len() == 1 && !direct_file_request(&requests[0]) {
                        let request = requests.remove(0);
                        let owner = GetWindow(dlg.hwnd, GW_OWNER);
                        let app = dlg.app.clone();
                        DestroyWindow(dlg.hwnd);
                        show_media_dialog(owner, app, request);
                        return;
                    }
                    // An address already in the queue asks once for the whole batch:
                    // keep the existing jobs (skip these addresses) or add new copies.
                    let queued = dlg
                        .app
                        .snapshot()
                        .jobs
                        .iter()
                        .map(|job| normalized_job_url(&job.request.url))
                        .collect::<std::collections::HashSet<_>>();
                    let duplicates = requests
                        .iter()
                        .filter(|request| queued.contains(&normalized_job_url(&request.url)))
                        .count();
                    if duplicates > 0 {
                        let body = crate::i18n::ui_owned!(
                            format!(
                                "{duplicates} adres zaten indirme listesinde.

Mevcut işler kullanılsın mı, yoksa yeni kopyalar mı indirilsin?"
                            ),
                            format!(
                                "{duplicates} address(es) are already in the download list.

Use the existing jobs, or download new copies?"
                            )
                        );
                        let buttons = [
                            (IDYES, crate::i18n::ui("Mevcut işi kullan", "Use existing")),
                            (IDNO, crate::i18n::ui("Yeni kopya indir", "Download a copy")),
                            (IDCANCEL, crate::i18n::ui("İptal", "Cancel")),
                        ];
                        match run_alert_buttons(
                            dlg.hwnd,
                            &body,
                            "SSDownload",
                            MB_ICONQUESTION,
                            Some(&buttons),
                        )
                        .unwrap_or(IDCANCEL)
                        {
                            IDYES => requests.retain(|request| {
                                !queued.contains(&normalized_job_url(&request.url))
                            }),
                            IDNO => {}
                            _ => return,
                        }
                    }
                    let mut remaining = requests.into_iter();
                    while let Some(request) = remaining.next() {
                        let url = request.url.clone();
                        if let Err(e) = dlg.app.dispatch(Action::Add { request }) {
                            let pending = std::iter::once(url)
                                .chain(remaining.map(|request| request.url))
                                .collect::<Vec<_>>()
                                .join("\r\n");
                            set_text(f.urls, &pending);
                            set_dialog_error(
                                dlg.hwnd,
                                dlg.error,
                                &crate::i18n::ui_owned!(
                                    format!("İndirme eklenemedi: {e:#}"),
                                    format!("The download could not be added: {e:#}")
                                ),
                                null_mut(),
                            );
                            return;
                        }
                    }
                    DestroyWindow(dlg.hwnd);
                }
                Err((e, h)) => set_dialog_error(dlg.hwnd, dlg.error, &e, h),
            },
            D_ADD_ANALYZE => match collect_add(f, true) {
                Ok(mut requests) => {
                    let modes = dlg.app.snapshot().settings.usage_modes;
                    if !modes.allows_inspection() {
                        set_dialog_error(
                            dlg.hwnd,
                            dlg.error,
                            crate::i18n::ui(
                                "Video veya ses indirmesi Ayarlar'da etkin değil.",
                                "Video or audio downloading is disabled in Settings.",
                            ),
                            f.kind,
                        );
                        return;
                    }
                    if let Some(request) = requests
                        .iter()
                        .find(|request| !mode_allows_new_job(modes, request.kind))
                    {
                        set_dialog_error(dlg.hwnd, dlg.error, mode_error(request.kind), f.kind);
                        return;
                    }
                    let owner = GetWindow(dlg.hwnd, GW_OWNER);
                    let app = dlg.app.clone();
                    let request = requests.remove(0);
                    DestroyWindow(dlg.hwnd);
                    show_media_dialog(owner, app, request);
                }
                Err((e, h)) => set_dialog_error(dlg.hwnd, dlg.error, &e, h),
            },
            D_CANCEL => {
                DestroyWindow(dlg.hwnd);
            }
            _ => {}
        },
        DialogKind::Settings(f) => match id {
            S_BROWSE => {
                if let Some(p) = browse_folder(dlg.hwnd, &text(f.dir)) {
                    set_text(f.dir, &p.to_string_lossy());
                }
            }
            ID_SETTINGS_REOPEN_WIZARD => {
                show_usage_wizard(dlg.hwnd, dlg.app.clone(), f.value.clone());
                let settings = dlg.app.snapshot().settings.clone();
                f.value = settings.clone();
                set_check(f.mode_video, settings.usage_modes.video);
                set_check(f.mode_file, settings.usage_modes.file);
                set_check(f.mode_audio, settings.usage_modes.audio);
                // The wizard answers the same question as the mode selector, so
                // the dialog follows whatever it decided.
                combo_select(
                    f.ui_mode,
                    if UiMode::parse(&settings.ui_mode).is_simple() {
                        0
                    } else {
                        1
                    },
                );
                apply_settings_mode(dlg, f);
            }
            S_UI_MODE if _code == CBN_SELCHANGE as u16 => {
                f.value.ui_mode = if combo_index(f.ui_mode) == 0 {
                    UiMode::Simple
                } else {
                    UiMode::Advanced
                }
                .as_str()
                .to_string();
                apply_settings_mode(dlg, f);
            }
            S_UI_LANGUAGE if _code == CBN_SELCHANGE as u16 => {
                // The dialog value mirrors the combo; collect_settings writes
                // it back when the settings are saved.
                f.value.ui_language = if combo_index(f.ui_language) == 0 {
                    "tr"
                } else {
                    "en"
                }
                .to_string();
            }
            ID_SETTINGS_CHECK_UPDATES => {
                crate::update::check_now(dlg.app.clone());
            }
            ID_SETTINGS_INSTALL_EXTENSION => {
                let app = dlg.app.clone();
                let spawned = std::thread::Builder::new()
                    .name("extension-install".into())
                    .spawn(move || {
                        let mut next = app.snapshot().settings;
                        next.extension_offer_seen = true;
                        if let Err(error) = app.update_settings_quiet(next) {
                            crate::logging::record(crate::logging::Event::warn("gui.extension_install_save").detail(format!("{error:#}")));
                        }
                        let existing = crate::extension::policies_installed().unwrap_or(false);
                        match crate::extension::install_policies() {
                            Ok(report) if report.needs_elevation => {
                                if confirm_elevated(
                                    crate::i18n::ui(
                                        "Chrome politika anahtarı yönetici izni gerektiriyor.\n\nYönetici onayıyla yeniden denensin mi?",
                                        "The Chrome policy key requires administrator rights.\n\nRetry with administrator approval?",
                                    ),
                                    crate::i18n::ui(
                                        "SSDownload tarayıcı eklentisi",
                                        "SSDownload browser extension",
                                    ),
                                ) {
                                    let _ = crate::extension::run_elevated("--install-extension");
                                }
                            }
                            Ok(report) => {
                                // Chrome blocks self-hosted policies on unmanaged
                                // devices, and a leftover entry evicts a manually
                                // loaded copy: offer the cleanup.
                                if existing
                                    && crate::extension::policies_removal_needs_elevation()
                                    && confirm_elevated(
                                        crate::i18n::ui(
                                            "Daha önce yazılmış Chrome politika kaydı eklentiyi engelliyor.\n\nYönetici onayıyla kaldırılsın mı?",
                                            "A previously written Chrome policy entry blocks the extension.\n\nRemove it with administrator approval?",
                                        ),
                                        crate::i18n::ui(
                                            "SSDownload tarayıcı eklentisi",
                                            "SSDownload browser extension",
                                        ),
                                    )
                                {
                                    let _ =
                                        crate::extension::run_elevated("--remove-extension-policies");
                                }
                                crate::gui::report_update_text(&report.summary());
                            }
                            Err(error) => crate::gui::report_update_error(&crate::i18n::ui_owned!(format!("Eklenti politikaları yazılamadı: {error:#}"), format!("The extension policies could not be written: {error:#}"))),
                        }
                    });
                if let Err(error) = spawned {
                    crate::logging::record(
                        crate::logging::Event::warn("gui.extension_install_spawn").detail(format!(
                            "Eklenti kurulum iş parçacığı başlatılamadı: {error}"
                        )),
                    );
                }
            }
            S_OK => match collect_settings(f) {
                Ok(value) => match dlg.app.dispatch(Action::SetSettings { settings: value }) {
                    Ok(()) => {
                        DestroyWindow(dlg.hwnd);
                    }
                    Err(e) => set_dialog_error(
                        dlg.hwnd,
                        dlg.error,
                        &crate::i18n::ui_owned!(
                            format!("Ayarlar kaydedilemedi: {e:#}"),
                            format!("Settings could not be saved: {e:#}")
                        ),
                        null_mut(),
                    ),
                },
                Err((e, h)) => set_dialog_error(dlg.hwnd, dlg.error, &e, h),
            },
            D_CANCEL => {
                DestroyWindow(dlg.hwnd);
            }
            _ => {}
        },
        DialogKind::Tools(_) => match id {
            T_INSTALL => {
                if let Err(e) = dlg.app.dispatch(Action::InstallTools {
                    request_id: None,
                    force_update: false,
                }) {
                    set_dialog_error(
                        dlg.hwnd,
                        dlg.error,
                        &crate::i18n::ui_owned!(
                            format!("Kurulum başlatılamadı: {e:#}"),
                            format!("The installation could not be started: {e:#}")
                        ),
                        null_mut(),
                    );
                }
            }
            T_UPDATE => {
                if let Err(e) = dlg.app.dispatch(Action::InstallTools {
                    force_update: true,
                    request_id: None,
                }) {
                    set_dialog_error(
                        dlg.hwnd,
                        dlg.error,
                        &crate::i18n::ui_owned!(
                            format!("Güncelleme başlatılamadı: {e:#}"),
                            format!("The update could not be started: {e:#}")
                        ),
                        null_mut(),
                    );
                }
            }
            T_BROWSER => {
                if let Err(e) = dlg.app.dispatch(Action::BrowserSetup) {
                    set_dialog_error(
                        dlg.hwnd,
                        dlg.error,
                        &crate::i18n::ui_owned!(
                            format!("Tarayıcı kurulumu açılamadı: {e:#}"),
                            format!("Browser setup could not be opened: {e:#}")
                        ),
                        null_mut(),
                    );
                }
            }
            D_CANCEL => {
                DestroyWindow(dlg.hwnd);
            }
            _ => {}
        },
        DialogKind::Queues(f) => {
            match queue_command(dlg.hwnd, &dlg.app, f, id, _code) {
                Ok(()) => set_text(dlg.error, ""),
                Err(e) => set_dialog_error(dlg.hwnd, dlg.error, &format!("{e:#}"), null_mut()),
            };
        }
        DialogKind::Rules(f) => {
            match rule_command(&dlg.app, f, id, _code) {
                Ok(()) => set_text(dlg.error, ""),
                Err(e) => set_dialog_error(dlg.hwnd, dlg.error, &format!("{e:#}"), null_mut()),
            };
        }
        DialogKind::Crawler(f) => {
            match crawler_command(dlg.hwnd, &dlg.app, f, id) {
                Ok(()) => set_text(dlg.error, ""),
                Err(e) => set_dialog_error(dlg.hwnd, dlg.error, &format!("{e:#}"), null_mut()),
            };
        }
        DialogKind::Synchronization(f) => {
            match sync_command(&dlg.app, f, id, _code) {
                Ok(()) => set_text(dlg.error, ""),
                Err(e) => set_dialog_error(dlg.hwnd, dlg.error, &format!("{e:#}"), null_mut()),
            };
        }
        DialogKind::Events(f) => match id {
            E_OPEN => {
                let logs = dlg.app.paths().base_dir.join("logs");
                if open_folder(&logs) {
                    set_text(dlg.error, "");
                } else {
                    set_dialog_error(
                        dlg.hwnd,
                        dlg.error,
                        crate::i18n::ui(
                            "Günlük klasörü açılamadı.",
                            "The log folder could not be opened.",
                        ),
                        f.list,
                    );
                }
            }
            E_PACKAGE => {
                if let Err(e) = dlg.app.dispatch(Action::ExportDiagnostics) {
                    set_dialog_error(
                        dlg.hwnd,
                        dlg.error,
                        &crate::i18n::ui_owned!(
                            format!("Teşhis paketi oluşturulamadı: {e:#}"),
                            format!("The diagnostic package could not be created: {e:#}")
                        ),
                        f.list,
                    );
                }
            }
            E_CLOSE => {
                DestroyWindow(dlg.hwnd);
            }
            _ => {}
        },
        DialogKind::Speed(f) => {
            if id == SP_OK {
                match text(f.edit).trim().parse::<u64>() {
                    Ok(kib) if f.ids.is_empty() => {
                        // No selection: the global limit (tray "Hız sınırı → Özel...").
                        let mut settings = dlg.app.snapshot().settings;
                        settings.speed_limit_kib = kib;
                        match dlg.app.update_settings_quiet(settings) {
                            Ok(()) => {
                                DestroyWindow(dlg.hwnd);
                            }
                            Err(error) => {
                                set_dialog_error(dlg.hwnd, dlg.error, &format!("{error:#}"), f.edit)
                            }
                        }
                    }
                    Ok(kib) => match dlg.app.dispatch(Action::SetSpeedLimit {
                        ids: f.ids.clone(),
                        kib,
                    }) {
                        Ok(()) => {
                            DestroyWindow(dlg.hwnd);
                        }
                        Err(error) => {
                            set_dialog_error(dlg.hwnd, dlg.error, &format!("{error:#}"), f.edit)
                        }
                    },
                    Err(_) => set_dialog_error(
                        dlg.hwnd,
                        dlg.error,
                        crate::i18n::ui(
                            "Hız sınırı 0 veya pozitif bir tam sayı olmalıdır.",
                            "The speed limit must be 0 or a positive integer.",
                        ),
                        f.edit,
                    ),
                }
            }
        }
        DialogKind::Rename(f) => {
            if id == RN_OK {
                let name = text(f.edit).trim().to_string();
                if name.is_empty() {
                    set_dialog_error(
                        dlg.hwnd,
                        dlg.error,
                        crate::i18n::ui("Dosya adı boş olamaz.", "The file name cannot be empty."),
                        f.edit,
                    );
                    return;
                }
                match dlg.app.dispatch(Action::Rename {
                    id: f.id.clone(),
                    name,
                }) {
                    Ok(()) => {
                        DestroyWindow(dlg.hwnd);
                    }
                    Err(error) => {
                        set_dialog_error(dlg.hwnd, dlg.error, &format!("{error:#}"), f.edit)
                    }
                }
            }
        }
        DialogKind::Logins(f) => match id {
            L_LIST if _code == LBN_SELCHANGE as u16 => {
                let index = SendMessageW(f.list, LB_GETCURSEL, 0, 0);
                if let Some(login) = (index >= 0).then(|| f.logins.get(index as usize)).flatten() {
                    set_text(f.host, &login.host);
                    set_text(f.user, &login.username);
                    set_text(f.pass, "");
                    set_check(f.allow_http, login.allow_http);
                }
            }
            L_ADD => {
                let host = text(f.host).trim().to_ascii_lowercase();
                let username = text(f.user).trim().to_string();
                let password = text(f.pass);
                let existing = f
                    .logins
                    .iter()
                    .position(|login| login.host.eq_ignore_ascii_case(&host));
                let sealed = if password.is_empty() {
                    existing
                        .map(|index| f.logins[index].password.clone())
                        .unwrap_or_default()
                } else {
                    match crate::secure::seal(password) {
                        Ok(value) => value,
                        Err(error) => {
                            set_dialog_error(dlg.hwnd, dlg.error, &format!("{error:#}"), f.pass);
                            return;
                        }
                    }
                };
                set_text(f.pass, "");
                let login = crate::model::SiteLogin {
                    host,
                    username,
                    password: sealed,
                    allow_http: check(f.allow_http),
                };
                let mut candidate = dlg.app.snapshot().settings;
                let mut logins = f.logins.clone();
                match existing {
                    Some(index) => logins[index] = login,
                    None => logins.push(login),
                }
                candidate.site_logins = logins.clone();
                if let Err(error) = crate::validation::validate_settings(&candidate) {
                    set_dialog_error(dlg.hwnd, dlg.error, &error.to_string(), f.host);
                    return;
                }
                f.logins = logins;
                set_text(dlg.error, "");
                refresh_login_list(f);
            }
            L_REMOVE => {
                let index = SendMessageW(f.list, LB_GETCURSEL, 0, 0);
                if index >= 0 && (index as usize) < f.logins.len() {
                    f.logins.remove(index as usize);
                    refresh_login_list(f);
                }
            }
            L_SAVE => {
                let mut settings = dlg.app.snapshot().settings;
                settings.site_logins = f.logins.clone();
                match dlg.app.dispatch(Action::SetSettings { settings }) {
                    Ok(()) => {
                        DestroyWindow(dlg.hwnd);
                    }
                    Err(error) => {
                        set_dialog_error(dlg.hwnd, dlg.error, &format!("{error:#}"), f.list)
                    }
                }
            }
            _ => {}
        },
        DialogKind::JobLog(f) => match id {
            J_COPY => {
                let text = f.rendered.join("\r\n");
                if set_clipboard_text(&text) {
                    set_text(dlg.error, "");
                } else {
                    set_dialog_error(
                        dlg.hwnd,
                        dlg.error,
                        crate::i18n::ui(
                            "Kayıtlar panoya kopyalanamadı.",
                            "The entries could not be copied to the clipboard.",
                        ),
                        f.list,
                    );
                }
            }
            J_OPEN => {
                let jobs = dlg.app.paths().base_dir.join("logs").join("jobs");
                if open_folder(&jobs) {
                    set_text(dlg.error, "");
                } else {
                    set_dialog_error(
                        dlg.hwnd,
                        dlg.error,
                        crate::i18n::ui(
                            "Günlük klasörü açılamadı.",
                            "The log folder could not be opened.",
                        ),
                        f.list,
                    );
                }
            }
            J_CLOSE => {
                DestroyWindow(dlg.hwnd);
            }
            _ => {}
        },
        DialogKind::Media(f) => match id {
            D_ADVANCED => {
                f.advanced = !f.advanced;
                resize_dialog(dlg.hwnd, dlg.dpi, if f.advanced { 730 } else { 420 });
                layout_dialog(dlg);
            }
            M_REPORT => {
                // No job is behind a failed analysis: the report is written for
                // the source that failed, and the application opens the folder
                // it wrote it into.
                if let Err(e) = dlg.app.dispatch(Action::Report { job_id: None }) {
                    let error = crate::i18n::ui_owned!(
                        format!("Hata bildirilemedi: {e:#}"),
                        format!("The problem could not be reported: {e:#}")
                    );
                    f.report_error = Some(error.clone());
                    set_dialog_error(dlg.hwnd, dlg.error, &error, null_mut());
                }
            }
            M_KIND => {
                layout_dialog(dlg);
                update_media_queue_enabled(dlg.hwnd, f, &dlg.app.snapshot());
            }
            ID_MEDIA_EXTERNAL_ADD => {
                let url = text(f.external_url).trim().to_string();
                let language = text(f.external_language).trim().to_string();
                let label = text(f.external_label).trim().to_string();
                if !url::Url::parse(&url)
                    .is_ok_and(|value| matches!(value.scheme(), "http" | "https"))
                {
                    set_dialog_error(
                        dlg.hwnd,
                        dlg.error,
                        crate::i18n::ui(
                            "Harici altyazı için geçerli bir HTTP/HTTPS URL girin.",
                            "Enter a valid HTTP/HTTPS URL for the external subtitle.",
                        ),
                        f.external_url,
                    );
                    return;
                }
                if language.is_empty() {
                    set_dialog_error(
                        dlg.hwnd,
                        dlg.error,
                        crate::i18n::ui(
                            "Harici altyazı dil kodu boş bırakılamaz.",
                            "The external subtitle language code cannot be empty.",
                        ),
                        f.external_language,
                    );
                    return;
                }
                let kind = if combo_index(f.external_kind) == 1 {
                    "srt"
                } else {
                    "vtt"
                };
                let is_default = check(f.external_default);
                if is_default {
                    for existing in &mut f.external_values {
                        existing.is_default = false;
                    }
                }
                f.external_values.push(ExternalSubtitle {
                    url,
                    language,
                    label,
                    kind: kind.into(),
                    is_default,
                    headers: f.base.headers.clone(),
                    referer: f.base.referer.clone(),
                });
                set_text(f.external_url, "");
                set_text(f.external_label, "");
                set_check(f.external_default, false);
                refresh_external_subtitle_list(f);
                set_text(dlg.error, "");
                SetFocus(f.external_url);
            }
            ID_MEDIA_EXTERNAL_REMOVE => {
                let index = SendMessageW(f.external_list, LB_GETCURSEL, 0, 0) as i32;
                if index >= 0 && (index as usize) < f.external_values.len() {
                    f.external_values.remove(index as usize);
                    refresh_external_subtitle_list(f);
                }
            }
            M_MP3 => {
                // Audio-only output as MP3, then the ordinary queue path.
                if let Some(index) = f
                    .kind_values
                    .iter()
                    .position(|(kind, _)| *kind == DownloadKind::Audio)
                {
                    combo_select(f.kind, index as i32);
                    combo_select(f.audio, 1);
                    layout_dialog(&*dlg_ptr);
                    PostMessageW(dlg.hwnd, WM_COMMAND, M_QUEUE as usize, 0);
                }
            }
            M_QUEUE => {
                if media_generation_current(dlg, f) {
                    if f.populated {
                        queued = queue_media(dlg.hwnd, dlg.error, dlg.app.clone(), f);
                    } else {
                        // A posted command can outrun the populate, so the queue
                        // path checks the same readiness the button shows.
                        set_dialog_error(
                            dlg.hwnd,
                            dlg.error,
                            crate::i18n::ui(
                                "Analiz bu pencereye yüklenmeden indirme başlatılamaz.",
                                "A download cannot start before the analysis loads into this window.",
                            ),
                            f.kind,
                        );
                    }
                }
            }
            M_RENEW => {
                if media_generation_current(dlg, f) {
                    renew_source(dlg.hwnd, dlg.error, dlg.app.clone(), f);
                }
            }
            D_CANCEL => {
                DestroyWindow(dlg.hwnd);
            }
            _ => {}
        },
        DialogKind::Progress(f) if id == P_MINI => {
            let ids = f.ids.clone();
            let owner = GetWindow(dlg.hwnd, GW_OWNER);
            open_card(owner, dlg.app.clone(), CardKind::Jobs(ids));
            DestroyWindow(dlg.hwnd);
        }
        DialogKind::Progress(_) if id == P_PIN => {
            let pin = GetDlgItem(dlg.hwnd, P_PIN);
            SetWindowPos(
                dlg.hwnd,
                if check(pin) {
                    HWND_TOPMOST
                } else {
                    HWND_NOTOPMOST
                },
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
        DialogKind::Progress(f) => match id {
            P_ACTION => {
                let snapshot = dlg.app.snapshot();
                match job_view_job(&snapshot, f) {
                    Some(job) if can_pause_or_resume(job) => {
                        // The same pair the main window offers; a failed or
                        // cancelled job is retried through Resume, an active
                        // one is held with Pause.
                        let action = match job.state {
                            JobState::Paused | JobState::Failed | JobState::Cancelled => {
                                Action::Resume { id: job.id.clone() }
                            }
                            _ => Action::Pause { id: job.id.clone() },
                        };
                        match dlg.app.dispatch(action) {
                            Ok(()) => set_text(dlg.error, ""),
                            Err(e) => set_dialog_error(
                                dlg.hwnd,
                                dlg.error,
                                &crate::i18n::ui_owned!(
                                    format!("İşlem uygulanamadı: {e:#}"),
                                    format!("The action could not be applied: {e:#}")
                                ),
                                f.action,
                            ),
                        }
                    }
                    _ => {}
                }
            }
            P_CANCEL => {
                let snapshot = dlg.app.snapshot();
                let target = job_view_job(&snapshot, f)
                    .filter(|job| {
                        job.remove_requested.is_none()
                            && !matches!(job.state, JobState::Completed | JobState::Cancelled)
                    })
                    .map(|job| (job.id.clone(), job.name.clone()));
                if let Some((id, name)) = target {
                    let answer = message(
                        dlg.hwnd,
                        &crate::i18n::ui_owned!(format!(
                            "\"{name}\" indirmesi iptal edilsin mi?\n\nParça dosyaları silinir; tamamlanmış çıktı korunur."
                        ), format!(
                            "Cancel the download \"{name}\"?\n\nFragment files are deleted; finished output is kept."
                        )),
                        crate::i18n::ui("İndirmeyi iptal et", "Cancel the download"),
                        MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2,
                    );
                    // The alert pumps its own loop, and the window can be closed
                    // from under it (a quit, a forced teardown), so the box is
                    // only used again while its window still exists.
                    if answer == IDYES && IsWindow(dlg.hwnd) != 0 {
                        match dlg.app.dispatch(Action::Remove {
                            id,
                            delete_file: false,
                        }) {
                            // The engine acknowledges the cancellation itself;
                            // until it does, this says a request was made and
                            // not that the download is already cancelled.
                            Ok(()) => set_text(
                                dlg.error,
                                crate::i18n::ui("İptal istendi.", "Cancellation requested."),
                            ),
                            Err(e) => set_dialog_error(
                                dlg.hwnd,
                                dlg.error,
                                &crate::i18n::ui_owned!(
                                    format!("İptal edilemedi: {e:#}"),
                                    format!("Could not cancel: {e:#}")
                                ),
                                f.cancel,
                            ),
                        }
                    }
                }
            }
            P_OPEN_FILE => {
                let snapshot = dlg.app.snapshot();
                if let Some(job) = job_view_job(&snapshot, f) {
                    // Only a finished job has an output; the engine checks the
                    // same state again before it opens anything.
                    if job.state == JobState::Completed && job.remove_requested.is_none() {
                        match dlg.app.dispatch(Action::OpenFile { id: job.id.clone() }) {
                            Ok(()) => set_text(dlg.error, ""),
                            Err(e) => set_dialog_error(
                                dlg.hwnd,
                                dlg.error,
                                &crate::i18n::ui_owned!(
                                    format!("Dosya açılamadı: {e:#}"),
                                    format!("The file could not be opened: {e:#}")
                                ),
                                f.open_file,
                            ),
                        }
                    }
                }
            }
            P_OPEN_FOLDER => {
                let snapshot = dlg.app.snapshot();
                if let Some(job) = job_view_job(&snapshot, f) {
                    if job.state == JobState::Completed && job.remove_requested.is_none() {
                        match dlg.app.dispatch(Action::OpenFolder {
                            id: Some(job.id.clone()),
                        }) {
                            Ok(()) => set_text(dlg.error, ""),
                            Err(e) => set_dialog_error(
                                dlg.hwnd,
                                dlg.error,
                                &crate::i18n::ui_owned!(
                                    format!("Klasör açılamadı: {e:#}"),
                                    format!("The folder could not be opened: {e:#}")
                                ),
                                f.open_folder,
                            ),
                        }
                    }
                }
            }
            P_CLOSE => {
                // Closing the window is not cancelling the download: the job
                // keeps running and stays in the main window, and only the
                // explicit cancel above stops it.
                DestroyWindow(dlg.hwnd);
            }
            P_REPORT => {
                let snapshot = dlg.app.snapshot();
                // Only the failure the window is showing can be reported; the
                // application writes the files and opens the folder it wrote
                // them into.
                let target = job_view_job(&snapshot, f)
                    .filter(|job| job.state == JobState::Failed || job.error.is_some())
                    .map(|job| job.id.clone());
                if let Some(id) = target {
                    match dlg.app.dispatch(Action::Report { job_id: Some(id) }) {
                        Ok(()) => set_text(dlg.error, ""),
                        Err(e) => set_dialog_error(
                            dlg.hwnd,
                            dlg.error,
                            &crate::i18n::ui_owned!(
                                format!("Hata bildirilemedi: {e:#}"),
                                format!("The problem could not be reported: {e:#}")
                            ),
                            f.report,
                        ),
                    }
                }
            }
            P_LIST if _code == LBN_SELCHANGE as u16 => {
                let index = SendMessageW(f.list, LB_GETCURSEL, 0, 0) as i32;
                if index >= 0 {
                    f.selected = index as usize;
                }
                let snapshot = dlg.app.snapshot();
                refresh_job_view(dlg.hwnd, dlg.error, f, &snapshot);
            }
            _ => {}
        },
    }
    if let Some(job_ids) = queued {
        if !job_ids.is_empty() {
            begin_job_view(dlg, job_ids);
        }
    }
}

pub(super) unsafe fn collect_settings(
    f: &SettingsFields,
) -> std::result::Result<Settings, (String, HWND)> {
    let mut value = f.value.clone();
    let dir = text(f.dir).trim().to_string();
    if dir.is_empty() {
        return Err((
            crate::i18n::ui(
                "İndirme klasörü boş bırakılamaz.",
                "The download folder cannot be empty.",
            )
            .to_string(),
            f.dir,
        ));
    }
    value.download_dir = PathBuf::from(dir);
    value.ui_mode = if combo_index(f.ui_mode) == 0 {
        UiMode::Simple
    } else {
        UiMode::Advanced
    }
    .as_str()
    .to_string();
    value.ui_language = if combo_index(f.ui_language) == 0 {
        "tr"
    } else {
        "en"
    }
    .to_string();
    value.site_entries = check(f.site_entries);
    value.close_to_tray = check(f.tray);
    value.start_with_windows = check(f.startup);
    value.notify_completion = check(f.notify);
    value.dark_mode = check(f.dark);
    value.update_check_enabled = check(f.update_check);
    if UiMode::parse(&value.ui_mode).is_simple() {
        // The rows the simple dialog does not show are not read back and not
        // validated: they keep the values the profile already carries, so a
        // value typed on the advanced page can never fail a check whose field
        // the user cannot see.
        return Ok(value);
    }
    let max_active = parse_u8_range(
        &text(f.max_active),
        1,
        16,
        crate::i18n::ui("Etkin iş sayısı", "Active job count"),
    )
    .map_err(|e| (e, f.max_active))?;
    let connections = parse_u8_range(
        &text(f.connections),
        1,
        16,
        crate::i18n::ui("Dosya aralığı bağlantısı", "Connections per file"),
    )
    .map_err(|e| (e, f.connections))?;
    let media_fragment_connections = parse_u8_range(
        &text(f.media_fragments),
        1,
        16,
        crate::i18n::ui("Medya parçası eşzamanlılığı", "Media fragment concurrency"),
    )
    .map_err(|e| (e, f.media_fragments))?;
    let per_host_limit = parse_u8_range(
        &text(f.per_host),
        1,
        16,
        crate::i18n::ui("Sunucu sınırı", "Per-server limit"),
    )
    .map_err(|e| (e, f.per_host))?;
    let retry_limit = parse_u8_range(
        &text(f.retry),
        0,
        20,
        crate::i18n::ui("Yeniden deneme", "Retry count"),
    )
    .map_err(|e| (e, f.retry))?;
    let speed_limit_kib = text(f.speed).trim().parse::<u64>().map_err(|_| {
        (
            crate::i18n::ui(
                "Hız sınırı 0 veya pozitif bir tam sayı olmalıdır.",
                "The speed limit must be 0 or a positive integer.",
            )
            .to_string(),
            f.speed,
        )
    })?;
    let start = text(f.start).trim().to_string();
    let end = text(f.end).trim().to_string();
    if !valid_clock(&start) {
        return Err((
            crate::i18n::ui(
                "Başlangıç saati SS:DD biçiminde ve geçerli olmalıdır.",
                "The start time must be valid and in HH:MM format.",
            )
            .to_string(),
            f.start,
        ));
    }
    if !valid_clock(&end) {
        return Err((
            crate::i18n::ui(
                "Bitiş saati SS:DD biçiminde ve geçerli olmalıdır.",
                "The end time must be valid and in HH:MM format.",
            )
            .to_string(),
            f.end,
        ));
    }
    let usage_modes = UsageModes {
        video: check(f.mode_video),
        file: check(f.mode_file),
        audio: check(f.mode_audio),
    };
    usage_modes
        .validate()
        .map_err(|error| (error.to_string(), f.mode_video))?;
    // Every row of the advanced page is on screen here, so all of them are read
    // back and validated; the simple dialog never reaches this point.
    value.max_active = max_active;
    value.connections = connections;
    value.media_fragment_connections = media_fragment_connections;
    value.per_host_limit = per_host_limit;
    value.speed_limit_kib = speed_limit_kib;
    value.retry_limit = retry_limit;
    value.clipboard_watch = check(f.clipboard);
    value.schedule_enabled = check(f.schedule);
    value.schedule_start = start;
    value.schedule_end = end;
    value.usage_modes = usage_modes;
    value.experimental_browser_transfer = check(f.browser_transfer);
    value.logging_level = match combo_index(f.log_level) {
        0 => Level::Off,
        1 => Level::Error,
        2 => Level::Normal,
        _ => Level::Detailed,
    }
    .as_str()
    .to_string();
    value.logging_hosts = check(f.log_hosts);
    value.proxy.mode = match combo_index(f.proxy_mode) {
        1 => "none",
        2 => "manual",
        _ => "system",
    }
    .to_string();
    value.proxy.url = text(f.proxy_url).trim().to_string();
    value.proxy.username = text(f.proxy_user).trim().to_string();
    let proxy_password = text(f.proxy_pass);
    if value.proxy.username.is_empty() {
        value.proxy.password.clear();
    } else if !proxy_password.is_empty() {
        value.proxy.password = crate::secure::seal(proxy_password)
            .map_err(|error| (format!("{error:#}"), f.proxy_pass))?;
    }
    if let Err(error) = crate::validation::validate_proxy(&value.proxy) {
        return Err((error.to_string(), f.proxy_url));
    }
    let speed_start = text(f.speed_start).trim().to_string();
    let speed_end = text(f.speed_end).trim().to_string();
    for (clock, control) in [(&speed_start, f.speed_start), (&speed_end, f.speed_end)] {
        if !valid_clock(clock) {
            return Err((
                crate::i18n::ui(
                    "Hız sınırı saati SS:DD biçiminde ve geçerli olmalıdır.",
                    "The speed limit time must be valid and in HH:MM format.",
                )
                .to_string(),
                control,
            ));
        }
    }
    value.speed_schedule_kib = text(f.speed_kib).trim().parse::<u64>().map_err(|_| {
        (
            crate::i18n::ui(
                "Saatli hız sınırı 0 veya pozitif bir tam sayı olmalıdır.",
                "The scheduled speed limit must be 0 or a positive integer.",
            )
            .to_string(),
            f.speed_kib,
        )
    })?;
    value.speed_schedule_enabled = check(f.speed_schedule);
    value.speed_schedule_start = speed_start;
    value.speed_schedule_end = speed_end;
    value.keep_awake = check(f.keep_awake);
    value.completion_sound = check(f.sound);
    value.browser_takeover = check(f.takeover);
    value.completion_card = check(f.completion_card);
    value.pause_on_metered = check(f.metered);
    value.global_hotkey = check(f.hotkey);
    value.double_click = if combo_index(f.double_click) == 1 {
        "folder"
    } else {
        "open"
    }
    .into();
    value.history_days = text(f.history).trim().parse::<u32>().map_err(|_| {
        (
            crate::i18n::ui(
                "Geçmiş süresi 0 veya pozitif bir tam sayı olmalıdır.",
                "The history period must be 0 or a positive integer.",
            )
            .to_string(),
            f.history,
        )
    })?;
    Ok(value)
}
