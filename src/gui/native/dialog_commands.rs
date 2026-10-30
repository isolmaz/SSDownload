//! Command handling of the management dialogs, the dialog command router and settings collection.

use super::*;

/// Parses a numeric field, naming the field instead of surfacing the parser's
/// untranslated "invalid digit found in string".
fn parse_number<T: std::str::FromStr>(value: &str, label: &str) -> anyhow::Result<T> {
    value.trim().parse().map_err(|_| {
        anyhow::anyhow!(crate::i18n::ui_format!(
            "{label} geçerli bir sayı olmalıdır.",
            "{label} must be a valid number.",
            label = label
        ))
    })
}

/// Asks before a list entry is deleted. "No" is the default button and Escape
/// answers it, and a dialog closed from under the box counts as "No" too.
unsafe fn confirm_removal(hwnd: HWND, body: &str) -> bool {
    let answer = message(
        hwnd,
        body,
        crate::i18n::ui("Silmeyi onayla", "Confirm deletion"),
        MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2,
    );
    answer == IDYES && IsWindow(hwnd) != 0
}

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
        Q_DELETE => {
            let id = selected.clone().ok_or_else(|| {
                anyhow::anyhow!(crate::i18n::ui("Bir kuyruk seçin", "Select a queue"))
            })?;
            let name = snapshot
                .settings
                .queues
                .iter()
                .find(|q| q.id == id)
                .map(|q| q.display_name().to_owned())
                .unwrap_or_default();
            if !confirm_removal(
                hwnd,
                &crate::i18n::ui_format!(
                    "\"{name}\" kuyruğu silinsin mi?",
                    "Delete the queue \"{name}\"?",
                    name = name
                ),
            ) {
                return Ok(());
            }
            Action::DeleteQueue { id }
        }
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
            // The built-in queue's shown name is a translation of its stored one: unchanged text keeps it.
            let typed = text(f.name);
            if typed.trim() != queue.display_name() {
                queue.name = typed.trim().into();
            }
            queue.enabled = check(f.enabled);
            queue.concurrency = parse_number(
                &text(f.concurrency),
                crate::i18n::ui("Eşzamanlılık", "Concurrency"),
            )?;
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
                        .map(|day| {
                            parse_number::<u8>(day, crate::i18n::ui("Gün numarası", "Day number"))
                        })
                        .collect::<anyhow::Result<Vec<u8>>>()?
                };
                queue.windows.push(crate::model::QueueWindow {
                    weekdays,
                    start: start.into(),
                    end: end.into(),
                });
            }
            let limit: u64 = parse_number(&text(f.quota), crate::i18n::ui("Kota", "Quota"))?;
            queue.quota = if limit == 0 {
                None
            } else {
                let mut quota = queue.quota.take().unwrap_or_default();
                quota.limit_bytes = limit;
                Some(quota)
            };
            let countdown_seconds: u32 =
                parse_number(&text(f.delay), crate::i18n::ui("Geri sayım", "Countdown"))?;
            queue.completion = match combo_index(f.completion) {
                0 => CompletionAction::None,
                1 => CompletionAction::Notify,
                2 => CompletionAction::ShutdownComputer { countdown_seconds },
                3 => CompletionAction::RunProgram {
                    program: PathBuf::from(text(f.program).trim()),
                    arguments: serde_json::from_str(&text(f.arguments)).map_err(|error| {
                        anyhow::anyhow!(crate::i18n::ui_format!("Program bağımsız değişkenleri bir JSON metin dizisi olmalı, örn. [\"-x\"]: {error}", "Program arguments must be a JSON array of strings, e.g. [\"-x\"]: {error}", error = error))
                    })?,
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
    hwnd: HWND,
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
                priority: parse_number(&text(f.priority), crate::i18n::ui("Öncelik", "Priority"))?,
                // The dialog has no switch for it: an update keeps the rule's state.
                enabled: selected
                    .as_ref()
                    .and_then(|id| settings.folder_rules.iter().find(|r| &r.id == id))
                    .is_none_or(|existing| existing.enabled),
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
            let host = settings
                .folder_rules
                .iter()
                .find(|r| r.id == selected)
                .map(|r| r.host.clone())
                .unwrap_or_default();
            if !confirm_removal(
                hwnd,
                &crate::i18n::ui_format!(
                    "\"{host}\" kuralı silinsin mi?",
                    "Delete the rule for \"{host}\"?",
                    host = host
                ),
            ) {
                return Ok(());
            }
            settings.folder_rules.retain(|r| r.id != selected);
        }
        _ => return Ok(()),
    }
    app.dispatch(Action::SetSettings { settings })?;
    refresh_rule_list(f, &app.snapshot().settings.folder_rules);
    Ok(())
}
pub(super) unsafe fn sync_command(
    hwnd: HWND,
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
            let url = settings
                .synchronization_policies
                .iter()
                .find(|p| p.id == selected)
                .map(|p| p.url.clone())
                .unwrap_or_default();
            if !confirm_removal(
                hwnd,
                &crate::i18n::ui_format!(
                    "\"{url}\" eşitleme kaydı silinsin mi?",
                    "Delete the sync record for \"{url}\"?",
                    url = url
                ),
            ) {
                return Ok(());
            }
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
            policy.interval_minutes = parse_number(
                &text(f.interval),
                crate::i18n::ui("Kontrol aralığı", "Check interval"),
            )?;
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
            policy.max_depth = parse_number(&text(f.depth), crate::i18n::ui("Derinlik", "Depth"))?;
            policy.max_pages = parse_number(
                &text(f.pages),
                crate::i18n::ui("En fazla sayfa", "Maximum pages"),
            )?;
            policy.max_candidates = parse_number(
                &text(f.candidates),
                crate::i18n::ui("En fazla aday", "Maximum candidates"),
            )?;
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
    if id == D_ERROR {
        // A click on the error line copies the whole message, which the row
        // itself may have had to cut short.
        let message = text(dlg.error);
        if _code as u32 == STN_CLICKED && !message.is_empty() {
            set_clipboard_text(&message);
        }
        return;
    }
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
                        &crate::i18n::ui_format!(
                            "Kurulum seçimi kaydedilemedi: {error:#}",
                            "The setup choice could not be saved: {error:#}",
                            error = error
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
                        let body = crate::i18n::ui_format!("{duplicates} adres zaten indirme listesinde.\n\nMevcut işler kullanılsın mı, yoksa yeni kopyalar mı indirilsin?", "{duplicates} address(es) are already in the download list.\n\nUse the existing jobs, or download new copies?", duplicates = duplicates);
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
                    let batch = requests.len();
                    let mut added = 0usize;
                    let mut remaining = requests.into_iter();
                    while let Some(request) = remaining.next() {
                        let url = request.url.clone();
                        match dlg.app.dispatch_result(Action::Add { request }) {
                            Ok(result) => added += result.job_ids.len(),
                            Err(e) => {
                                let pending = std::iter::once(url)
                                    .chain(remaining.map(|request| request.url))
                                    .collect::<Vec<_>>()
                                    .join("\r\n");
                                set_text(f.urls, &pending);
                                set_dialog_error(
                                    dlg.hwnd,
                                    dlg.error,
                                    &crate::i18n::ui_format!(
                                        "İndirme eklenemedi: {e:#}",
                                        "The download could not be added: {e:#}",
                                        e = e
                                    ),
                                    null_mut(),
                                );
                                return;
                            }
                        }
                    }
                    // Every addition posted its own "1 added" line and the last
                    // one would stay on the status bar; one line states the batch.
                    if batch > 1 {
                        dlg.app.message(
                            crate::i18n::ui_format!(
                                "{added} indirme eklendi",
                                "{added} download(s) added",
                                added = added
                            ),
                            false,
                        );
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
                f.opened = settings.clone();
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
            ID_SETTINGS_CHECK_UPDATES => {
                crate::update::check_now(dlg.app.clone());
            }
            ID_SETTINGS_INSTALL_EXTENSION => {
                let spawned = std::thread::Builder::new()
                    .name("extension-install".into())
                    .spawn(|| {
                        if let Err(error) = crate::extension::register_external() {
                            crate::gui::report_update_error(&crate::i18n::ui_format!("Eklenti Chrome/Edge için kaydedilemedi: {error:#}", "The extension could not be registered with Chrome/Edge: {error:#}", error = error));
                            return;
                        }
                        if crate::extension::remove_legacy_policies()
                            == crate::extension::LegacyCleanup::NeedsElevation
                            && confirm_elevated(
                                crate::i18n::ui(
                                    "Eski bir SSDownload sürümünün yazdığı Chrome/Edge politika kaydı eklentiyi engelliyor.\n\nYönetici onayıyla kaldırılsın mı?",
                                    "A Chrome/Edge policy entry written by an older SSDownload version blocks the extension.\n\nRemove it with administrator approval?",
                                ),
                                crate::i18n::ui(
                                    "SSDownload tarayıcı eklentisi",
                                    "SSDownload browser extension",
                                ),
                            )
                        {
                            if let Err(error) =
                                crate::extension::run_elevated("--remove-extension-policies")
                            {
                                crate::gui::report_update_error(&format!("{error:#}"));
                            }
                        }
                        let opened = crate::install::open_web_store().is_ok();
                        crate::gui::report_update_text(if opened {
                            crate::i18n::ui(
                                "Eklenti Chrome ve Edge'e eklendi: tarayıcıyı bir sonraki açışınızda çıkan uyarıda \"Eklentiyi etkinleştir\"e tıklayın.\n\nChrome Web Mağazası sayfası da açıldı; eklentiyi oradan hemen ekleyebilirsiniz.",
                                "The extension was added to Chrome and Edge: click \"Enable extension\" on the notice the next time you open the browser.\n\nThe Chrome Web Store page is open too; you can add the extension there right away.",
                            )
                        } else {
                            crate::i18n::ui(
                                "Eklenti Chrome ve Edge'e eklendi: tarayıcıyı bir sonraki açışınızda çıkan uyarıda \"Eklentiyi etkinleştir\"e tıklayın.",
                                "The extension was added to Chrome and Edge: click \"Enable extension\" on the notice the next time you open the browser.",
                            )
                        });
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
                Ok(value) => {
                    match merge_dialog_settings(&f.opened, value, dlg.app.snapshot().settings)
                        .and_then(|settings| dlg.app.dispatch(Action::SetSettings { settings }))
                    {
                        Ok(()) => {
                            DestroyWindow(dlg.hwnd);
                        }
                        Err(e) => set_dialog_error(
                            dlg.hwnd,
                            dlg.error,
                            &crate::i18n::ui_format!(
                                "Ayarlar kaydedilemedi: {e:#}",
                                "Settings could not be saved: {e:#}",
                                e = e
                            ),
                            null_mut(),
                        ),
                    }
                }
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
                        &crate::i18n::ui_format!(
                            "Kurulum başlatılamadı: {e:#}",
                            "The installation could not be started: {e:#}",
                            e = e
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
                        &crate::i18n::ui_format!(
                            "Güncelleme başlatılamadı: {e:#}",
                            "The update could not be started: {e:#}",
                            e = e
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
                        &crate::i18n::ui_format!(
                            "Tarayıcı kurulumu açılamadı: {e:#}",
                            "Browser setup could not be opened: {e:#}",
                            e = e
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
            match rule_command(dlg.hwnd, &dlg.app, f, id, _code) {
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
            match sync_command(dlg.hwnd, &dlg.app, f, id, _code) {
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
                        &crate::i18n::ui_format!(
                            "Teşhis paketi oluşturulamadı: {e:#}",
                            "The diagnostic package could not be created: {e:#}",
                            e = e
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
                    .position(|login| crate::proxy::same_host(&login.host, &host));
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
                    let host = f.logins[index as usize].host.clone();
                    if confirm_removal(
                        dlg.hwnd,
                        &crate::i18n::ui_format!(
                            "\"{host}\" girişi silinsin mi?",
                            "Delete the login for \"{host}\"?",
                            host = host
                        ),
                    ) {
                        f.logins.remove(index as usize);
                        refresh_login_list(f);
                    }
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
                    let error = crate::i18n::ui_format!(
                        "Hata bildirilemedi: {e:#}",
                        "The problem could not be reported: {e:#}",
                        e = e
                    );
                    f.report_error = Some(error.clone());
                    set_dialog_error(dlg.hwnd, dlg.error, &error, null_mut());
                }
            }
            M_KIND => {
                layout_dialog(dlg);
                update_media_queue_enabled(dlg.hwnd, f, &dlg.app.snapshot());
                refresh_media_size(f);
            }
            M_FORMAT | ID_MEDIA_AUDIO_TRACKS => refresh_media_size(f),
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
                                &crate::i18n::ui_format!(
                                    "İşlem uygulanamadı: {e:#}",
                                    "The action could not be applied: {e:#}",
                                    e = e
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
                        &crate::i18n::ui_format!("\"{name}\" indirmesi iptal edilsin mi?\n\nParça dosyaları silinir; tamamlanmış çıktı korunur.", "Cancel the download \"{name}\"?\n\nFragment files are deleted; finished output is kept.", name = name),
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
                                &crate::i18n::ui_format!(
                                    "İptal edilemedi: {e:#}",
                                    "Could not cancel: {e:#}",
                                    e = e
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
                                &crate::i18n::ui_format!(
                                    "Dosya açılamadı: {e:#}",
                                    "The file could not be opened: {e:#}",
                                    e = e
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
                                &crate::i18n::ui_format!(
                                    "Klasör açılamadı: {e:#}",
                                    "The folder could not be opened: {e:#}",
                                    e = e
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
                            &crate::i18n::ui_format!(
                                "Hata bildirilemedi: {e:#}",
                                "The problem could not be reported: {e:#}",
                                e = e
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

/// The settings to save when the dialog is confirmed: the engine's `current` settings with only
/// the fields the user changed in the dialog applied, so a change made elsewhere while the
/// dialog was open (a tray toggle, a queue edit, an update-check stamp, a language switch) is
/// kept. `opened` is the settings as the dialog opened them, `edited` the dialog's value after
/// the user's changes.
fn merge_dialog_settings(
    opened: &Settings,
    edited: Settings,
    current: Settings,
) -> anyhow::Result<Settings> {
    let opened = serde_json::to_value(opened)?;
    let edited = serde_json::to_value(edited)?;
    let mut merged = serde_json::to_value(current)?;
    if let (Some(opened), Some(edited), Some(merged)) = (
        opened.as_object(),
        edited.as_object(),
        merged.as_object_mut(),
    ) {
        for (key, value) in edited {
            if opened.get(key) != Some(value) {
                merged.insert(key.clone(), value.clone());
            }
        }
    }
    Ok(serde_json::from_value(merged)?)
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
    value.ui_language = crate::i18n::LANGUAGES
        .get(combo_index(f.ui_language) as usize)
        .map_or("tr", |info| info.code)
        .to_string();
    value.site_entries = check(f.site_entries);
    value.close_to_tray = check(f.tray);
    value.start_with_windows = check(f.startup);
    value.notify_completion = check(f.notify);
    value.dark_mode = check(f.dark);
    value.update_check_enabled = check(f.update_check);
    // Both modes show these two rows (on the general page in the simple one),
    // so both read them back.
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
    if UiMode::parse(&value.ui_mode).is_simple() {
        // The other rows the simple dialog does not show are not read back and
        // not validated: they keep the values the profile already carries, so a
        // value typed on the advanced page can never fail a check whose field
        // the user cannot see.
        value.max_active = max_active;
        value.connections = connections;
        return Ok(value);
    }
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
        crate::i18n::ui(
            "İlerlemesiz art arda deneme sayısı",
            "Attempts in a row without progress",
        ),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::CompletionAction;

    #[test]
    fn confirming_the_dialog_keeps_changes_made_elsewhere_while_it_was_open() {
        let opened = Settings {
            settings_revision: 3,
            speed_limit_kib: 100,
            history_days: 7,
            ..Default::default()
        };
        let mut edited = opened.clone();
        edited.close_to_tray = !opened.close_to_tray;
        edited.history_days = 30;
        edited.ui_language = "en".into();
        let mut current = opened.clone();
        current.settings_revision = 5;
        current.last_update_check = 1_700_000_000;
        current.speed_limit_kib = 512;
        current.queues[0].completion = CompletionAction::Notify;

        let merged = merge_dialog_settings(&opened, edited, current).unwrap();

        assert_eq!(merged.close_to_tray, !opened.close_to_tray);
        assert_eq!(merged.history_days, 30);
        assert_eq!(merged.ui_language, "en");
        assert_eq!(merged.last_update_check, 1_700_000_000);
        assert_eq!(merged.speed_limit_kib, 512);
        assert_eq!(merged.queues[0].completion, CompletionAction::Notify);
        assert_eq!(merged.settings_revision, 5);
    }

    #[test]
    fn a_language_or_mode_changed_elsewhere_survives_when_the_user_left_the_combos_alone() {
        let opened = Settings {
            ui_mode: UiMode::Advanced.as_str().into(),
            ui_language: "tr".into(),
            ..Default::default()
        };
        let edited = opened.clone();
        let mut current = opened.clone();
        current.ui_mode = UiMode::Simple.as_str().into();
        current.ui_language = "en".into();

        let merged = merge_dialog_settings(&opened, edited, current).unwrap();

        assert_eq!(merged.ui_mode, UiMode::Simple.as_str());
        assert_eq!(merged.ui_language, "en");
    }

    #[test]
    fn the_users_own_language_and_mode_choice_is_applied() {
        let opened = Settings {
            ui_mode: UiMode::Advanced.as_str().into(),
            ui_language: "tr".into(),
            ..Default::default()
        };
        let mut edited = opened.clone();
        edited.ui_mode = UiMode::Simple.as_str().into();
        edited.ui_language = "en".into();
        let mut current = opened.clone();
        current.speed_limit_kib = 512;

        let merged = merge_dialog_settings(&opened, edited, current).unwrap();

        assert_eq!(merged.ui_mode, UiMode::Simple.as_str());
        assert_eq!(merged.ui_language, "en");
        assert_eq!(merged.speed_limit_kib, 512);
    }
}
