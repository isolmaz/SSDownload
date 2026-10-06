//! Timer-driven refresh of open dialogs: job view, media picker population and queueing.

use super::*;

pub(super) unsafe fn dialog_timer(dlg: &mut DialogUi) {
    let snapshot = dlg.app.snapshot();
    let dlg_ptr = dlg as *mut DialogUi;
    match &mut (*dlg_ptr).kind {
        DialogKind::Queues(f) => refresh_completion_countdown(dlg.hwnd, f, &snapshot),
        DialogKind::Crawler(f) => {
            if let Some(receiver) = &f.pending {
                let received = receiver.try_recv();
                match received {
                    Ok(result) => {
                        f.pending = None;
                        EnableWindow(GetDlgItem(dlg.hwnd, C_SCAN), 1);
                        match result {
                            Ok(result) => {
                                SendMessageW(f.list, LB_RESETCONTENT, 0, 0);
                                for candidate in &result.candidates {
                                    let label =
                                        wide(&format!("{:?} · {}", candidate.kind, candidate.url));
                                    SendMessageW(f.list, LB_ADDSTRING, 0, label.as_ptr() as isize);
                                }
                                let truncated_note = if result.truncated {
                                    crate::i18n::ui(" (sınırda durdu)", " (stopped at the limit)")
                                } else {
                                    ""
                                };
                                let failed_note = if result.failed_pages > 0 {
                                    crate::i18n::ui_owned!(
                                        format!(
                                            ", {} sayfa alınamadı: {}",
                                            result.failed_pages,
                                            result.last_failure.clone().unwrap_or_default()
                                        ),
                                        format!(
                                            ", {} pages could not be fetched: {}",
                                            result.failed_pages,
                                            result.last_failure.clone().unwrap_or_default()
                                        )
                                    )
                                } else {
                                    String::new()
                                };
                                set_text(dlg.error,&crate::i18n::ui_owned!(format!("{} sayfa, {} aday, {} bayt tarandı{truncated_note}{failed_note}; yalnız seçilen adaylar eklenir.",
                                    result.pages_scanned,
                                    result.candidates.len(),
                                    result.bytes_scanned,
                                ), format!("Scanned {} pages, {} candidates, {} bytes{truncated_note}{failed_note}; only the selected candidates are added.",
                                    result.pages_scanned,
                                    result.candidates.len(),
                                    result.bytes_scanned,
                                )));
                                EnableWindow(
                                    GetDlgItem(dlg.hwnd, C_ADD),
                                    (!result.candidates.is_empty()) as BOOL,
                                );
                                f.result = Some(result);
                            }
                            Err(error) => set_dialog_error(dlg.hwnd, dlg.error, &error, null_mut()),
                        }
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        f.pending = None;
                        EnableWindow(GetDlgItem(dlg.hwnd, C_SCAN), 1);
                        set_dialog_error(
                            dlg.hwnd,
                            dlg.error,
                            crate::i18n::ui(
                                "Site tarama işçisi kapanmış; işlem tamamlandı sayılmadı.",
                                "The site scan worker exited; the operation was not counted as complete.",
                            ),
                            null_mut(),
                        );
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {}
                }
            }
        }
        DialogKind::Tools(f) => {
            let mut body = String::new();
            for tool in &snapshot.tools {
                body.push_str(&crate::i18n::ui_owned!(
                    format!(
                        "{}\r\n  Durum: {}\r\n  Sürüm: {}\r\n  Konum: {}\r\n\r\n",
                        tool.name,
                        if tool.installed {
                            crate::i18n::ui("Kurulu", "Installed")
                        } else {
                            crate::i18n::ui("Kurulu değil", "Not installed")
                        },
                        if tool.version.is_empty() {
                            "—"
                        } else {
                            &tool.version
                        },
                        tool.path.display()
                    ),
                    format!(
                        "{}\r\n  Status: {}\r\n  Version: {}\r\n  Location: {}\r\n\r\n",
                        tool.name,
                        if tool.installed {
                            crate::i18n::ui("Kurulu", "Installed")
                        } else {
                            crate::i18n::ui("Kurulu değil", "Not installed")
                        },
                        if tool.version.is_empty() {
                            "—"
                        } else {
                            &tool.version
                        },
                        tool.path.display()
                    )
                ));
            }
            if body.is_empty() {
                body.push_str(crate::i18n::ui(
                    "Henüz araç bilgisi bulunmuyor. Eksikleri kur düğmesi güvenli upstream sürümlerini indirir.",
                    "No tool information yet. The install button downloads safe upstream versions for anything missing.",
                ));
            }
            if let Some(p) = &snapshot.tool_progress {
                body.push_str(&crate::i18n::ui_owned!(
                    format!(
                        "\r\n{}: {}\r\nİndirilen: {}{}",
                        p.name,
                        p.message,
                        format_bytes(p.downloaded),
                        p.total
                            .map(|t| format!(" / {}", format_bytes(t)))
                            .unwrap_or_default()
                    ),
                    format!(
                        "\r\n{}: {}\r\nDownloaded: {}{}",
                        p.name,
                        p.message,
                        format_bytes(p.downloaded),
                        p.total
                            .map(|t| format!(" / {}", format_bytes(t)))
                            .unwrap_or_default()
                    )
                ));
                let percent = p
                    .total
                    .filter(|t| *t > 0)
                    .map(|t| (p.downloaded.saturating_mul(100) / t).min(100))
                    .unwrap_or(0);
                SendMessageW(f.progress, PBM_SETPOS, percent as usize, 0);
            } else {
                SendMessageW(
                    f.progress,
                    PBM_SETPOS,
                    if snapshot.installing_tools { 0 } else { 100 },
                    0,
                );
            }
            if body != f.last_render {
                set_text(f.status, &body);
                f.last_render = body;
            }
            EnableWindow(f.install, (!snapshot.installing_tools) as BOOL);
            EnableWindow(f.update, (!snapshot.installing_tools) as BOOL);
            set_text(
                dlg.error,
                if snapshot.installing_tools {
                    crate::i18n::ui(
                        "Araç işlemi arka planda sürüyor; pencereyi açık bırakmanız gerekmez.",
                        "The tool operation is running in the background; you can leave this window open or close it.",
                    )
                } else {
                    ""
                },
            );
        }
        DialogKind::Media(f) => {
            if snapshot.inspect_generation != f.inspect_generation {
                // A newer handoff or analysis replaced this picker: it must not
                // queue or rebind a stale selection. The generation, not the
                // reusable request id, decides which round is current.
                (*dlg_ptr).superseded.set(true);
                DestroyWindow(dlg.hwnd);
                return;
            }
            refresh_media_renew_jobs(f, &snapshot);
            let status = media_status_suffix(f, &snapshot);
            // The report button is invisible, not merely disabled, while there
            // is nothing to report, so the normal picker looks unchanged.
            let failed = snapshot.inspect_error.is_some();
            if f.report_shown != failed {
                f.report_shown = failed;
                layout_dialog(dlg);
            }
            if !failed {
                // A report failure the user has read past is dropped as soon as
                // the picker leaves the failed state.
                f.report_error = None;
            }
            EnableWindow(GetDlgItem(dlg.hwnd, M_REPORT), failed as BOOL);
            if snapshot.inspecting {
                set_text(
                    f.summary,
                    &crate::i18n::ui_owned!(
                        format!("Video ve çözünürlükler hazırlanıyor.{status}"),
                        format!("Preparing videos and resolutions.{status}")
                    ),
                );
                EnableWindow(GetDlgItem(dlg.hwnd, M_QUEUE), 0);
            } else if let Some(info) = snapshot.media.as_ref() {
                // Filled once, from the analysis of the generation this dialog
                // was created for; the enable below also requires that state.
                if !f.populated {
                    populate_media(dlg.hwnd, f, info, &status);
                    f.populated = true;
                    let audio_allowed = f
                        .kind_values
                        .iter()
                        .any(|(kind, _)| *kind == DownloadKind::Audio);
                    EnableWindow(GetDlgItem(dlg.hwnd, M_MP3), audio_allowed as BOOL);
                    if !f.thumbnail_requested {
                        f.thumbnail_requested = true;
                        if let Some(url) = info.thumbnail.clone() {
                            load_thumbnail(dlg.hwnd, dlg.app.clone(), url);
                        }
                    }
                    layout_dialog(dlg);
                    if f.auto_queue {
                        // "Bir daha sorma": the remembered choice goes straight to the queue.
                        PostMessageW(dlg.hwnd, WM_COMMAND, M_QUEUE as usize, 0);
                    }
                }
            } else {
                set_text(
                    f.summary,
                    &crate::i18n::ui_owned!(
                        format!(
                            "Bu kaynak için indirilebilir video seçenekleri doğrulanamadı.{status}"
                        ),
                        format!(
                        "Downloadable video options could not be verified for this source.{status}"
                    )
                    ),
                );
                f.video_formats.clear();
                EnableWindow(GetDlgItem(dlg.hwnd, M_QUEUE), 0);
                // A failed report keeps its own text on the line until the
                // picker leaves this state; the inspection error would
                // otherwise overwrite it on the next tick.
                if let Some(error) = f
                    .report_error
                    .as_deref()
                    .or(snapshot.inspect_error.as_deref())
                {
                    set_dialog_error(dlg.hwnd, dlg.error, error, null_mut());
                }
            }
            update_media_queue_enabled(dlg.hwnd, f, &snapshot);
        }
        DialogKind::Progress(f) => refresh_job_view(dlg.hwnd, dlg.error, f, &snapshot),
        DialogKind::Events(f) => refresh_event_list(f),
        DialogKind::JobLog(f) => refresh_job_log(f),
        _ => {}
    }
    // The same handoff repeated while its window is open, whether it still
    // shows the selection or the running download: come forward instead of
    // analysing again or opening a second window for one request. The counter,
    // not the sequence, decides - a repeat keeps the launch's own sequence
    // number, so only this window knows it happened.
    if let Some(launch) = snapshot.media_launch.as_ref() {
        if dlg.launch == Some(launch.seq) && launch.raise_seq > dlg.raise_seq {
            dlg.raise_seq = launch.raise_seq;
            activate_window(dlg.hwnd);
        }
    }
}
/// The job this window is showing: the entry at the selected row of the exact
/// ids its handoff queued, in the engine's own order. Never a list position of
/// the main window and never the newest job.
pub(super) fn job_view_job<'a>(snapshot: &'a AppSnapshot, f: &ProgressFields) -> Option<&'a Job> {
    let id = f.ids.get(f.selected)?;
    snapshot.jobs.iter().find(|job| &job.id == id)
}

/// Rewrites the job view from the engine's own state.
///
/// Status, progress, speed and the remaining time come from the tracked job
/// itself, the buttons are enabled for the state it reports, and only
/// `JobState::Completed` arms the output actions: a job that reached its last
/// byte while it is still processing is not a finished download. A job that is
/// no longer in the queue - removed from the main window, or cancelled here -
/// is reported as gone instead of being shown as if it were still running.
pub(super) unsafe fn refresh_job_view(
    hwnd: HWND,
    error: HWND,
    f: &mut ProgressFields,
    snapshot: &AppSnapshot,
) {
    if f.ids.is_empty() {
        set_text(
            error,
            crate::i18n::ui(
                "Bu pencereye bağlı bir indirme yok.",
                "No download is attached to this window.",
            ),
        );
        EnableWindow(f.action, 0);
        EnableWindow(f.cancel, 0);
        EnableWindow(f.open_file, 0);
        EnableWindow(f.open_folder, 0);
        return;
    }
    if f.selected >= f.ids.len() {
        f.selected = f.ids.len() - 1;
    }
    let mut completed = 0usize;
    let mut active = 0usize;
    let mut failed = 0usize;
    let mut cancelled = 0usize;
    let mut missing = 0usize;
    for id in &f.ids {
        match snapshot.jobs.iter().find(|job| &job.id == id) {
            None => missing += 1,
            Some(job) => match job.state {
                JobState::Completed => completed += 1,
                JobState::Failed => failed += 1,
                JobState::Cancelled => cancelled += 1,
                state if state.is_active() => active += 1,
                _ => {}
            },
        }
    }
    // One line for the whole handoff, so several queued items are never
    // summarised as whichever one happened to be first.
    let mut summary = String::new();
    if f.ids.len() == 1 {
        match job_view_job(snapshot, f) {
            Some(job) => {
                summary.push_str(&job.name);
                summary.push_str(" — ");
                summary.push_str(&state_label(job));
            }
            None => summary.push_str(crate::i18n::ui(
                "İş artık kuyrukta değil",
                "The job is no longer in the queue",
            )),
        }
    } else {
        summary.push_str(&crate::i18n::ui_owned!(
            format!("{} indirme", f.ids.len()),
            format!("{} downloads", f.ids.len())
        ));
        for (count, label) in [
            (completed, crate::i18n::ui("tamamlandı", "completed")),
            (active, crate::i18n::ui("sürüyor", "running")),
            (failed, crate::i18n::ui("başarısız", "failed")),
            (cancelled, crate::i18n::ui("iptal", "cancelled")),
            (missing, crate::i18n::ui("kuyrukta yok", "not in the queue")),
        ] {
            if count > 0 {
                summary.push_str(&format!(" · {count} {label}"));
            }
        }
    }
    set_text(f.summary, &summary);

    let mut detail = String::new();
    let action_label: &str;
    let can_control: bool;
    let can_cancel: bool;
    let can_open: bool;
    let mut percent = 0usize;
    match job_view_job(snapshot, f) {
        None => {
            detail.push_str(&crate::i18n::ui_owned!(format!(
                "Bu iş artık kuyrukta değil.\r\nKimlik: {}\r\n\r\nBaşka bir pencereden kaldırılmış ya da tamamlanmış kayıtlar temizlenmiş olabilir.",
                f.ids[f.selected]
            ), format!(
                "This job is no longer in the queue.\r\nId: {}\r\n\r\nIt may have been removed from another window, or completed entries may have been cleaned up.",
                f.ids[f.selected]
            )));
            action_label = crate::i18n::ui("Duraklat", "Pause");
            can_control = false;
            can_cancel = false;
            can_open = false;
        }
        Some(job) => {
            detail.push_str(&crate::i18n::ui_owned!(
                format!(
                    "{}\r\nDurum: {}\r\nİlerleme: {}\r\nHız: {}\r\nKalan süre: {}\r\n",
                    job.name,
                    state_label(job),
                    format_progress(job),
                    format_speed(job.speed),
                    format_eta(job.eta)
                ),
                format!(
                    "{}\r\nState: {}\r\nProgress: {}\r\nSpeed: {}\r\nRemaining: {}\r\n",
                    job.name,
                    state_label(job),
                    format_progress(job),
                    format_speed(job.speed),
                    format_eta(job.eta)
                )
            ));
            if !job.phase.trim().is_empty() {
                detail.push_str(&crate::i18n::ui_owned!(
                    format!("Aşama: {}\r\n", job.phase),
                    format!("Phase: {}\r\n", job.phase)
                ));
            }
            if let Some(message) = job.error.as_deref() {
                // The failure and its own text, so the reason is readable here
                // instead of only in the main window's log.
                detail.push_str(&crate::i18n::ui_owned!(
                    format!("Hata: {message}\r\n"),
                    format!("Error: {message}\r\n")
                ));
            }
            if job.state == JobState::Completed {
                detail.push_str(&crate::i18n::ui_owned!(
                    format!("Kayıt: {}\r\n", job.path.display()),
                    format!("Saved: {}\r\n", job.path.display())
                ));
                percent = 1000;
            } else if let Some(total) = job.total.filter(|total| *total > 0) {
                percent = ((job.downloaded.min(total) as u128 * 1000) / total as u128) as usize;
            }
            action_label = match job.state {
                JobState::Failed => crate::i18n::ui("Yeniden dene", "Retry"),
                JobState::Paused | JobState::Cancelled => crate::i18n::ui("Sürdür", "Resume"),
                _ => crate::i18n::ui("Duraklat", "Pause"),
            };
            can_control = can_pause_or_resume(job);
            can_cancel = job.remove_requested.is_none()
                && !matches!(job.state, JobState::Completed | JobState::Cancelled);
            can_open = job.state == JobState::Completed && job.remove_requested.is_none();
        }
    }
    // The detail names the components the way the rest of the surface does; a
    // phase or a failure reason can carry an upstream name.
    let detail = display_text(snapshot.settings.debug_mode, &detail);
    if detail != f.rendered {
        set_text(f.status, &detail);
        f.rendered = detail;
    }
    if action_label != f.action_label {
        set_text(f.action, action_label);
        f.action_label = action_label.to_string();
    }
    SendMessageW(f.progress, PBM_SETPOS, percent, 0);
    EnableWindow(f.action, can_control as BOOL);
    EnableWindow(f.cancel, can_cancel as BOOL);
    EnableWindow(f.open_file, can_open as BOOL);
    EnableWindow(f.open_folder, can_open as BOOL);
    // A failure is the only thing worth reporting, so the button is invisible -
    // not merely disabled - for every other state of the watched job.
    let reportable = job_view_job(snapshot, f)
        .is_some_and(|job| job.state == JobState::Failed || job.error.is_some());
    ShowWindow(f.report, if reportable { SW_SHOW } else { SW_HIDE });
    EnableWindow(f.report, reportable as BOOL);

    // A playlist keeps its own rows in step with the same state the detail
    // shows, so several items can be followed at once. The rows are rewritten
    // only when their text changed, and the row the user picked is selected
    // again afterwards.
    if f.ids.len() > 1 {
        let mut rows = String::new();
        for id in &f.ids {
            match snapshot.jobs.iter().find(|job| job.id == *id) {
                Some(job) => rows.push_str(&format!(
                    "{} — {} · {}\r\n",
                    job.name,
                    state_label(job),
                    format_progress(job)
                )),
                None => rows.push_str(&crate::i18n::ui_owned!(
                    format!("{id} — kuyrukta değil\r\n"),
                    format!("{id} — not in the queue\r\n")
                )),
            }
        }
        if rows != f.rendered_rows {
            SendMessageW(f.list, LB_RESETCONTENT, 0, 0);
            for line in rows.split("\r\n").filter(|line| !line.is_empty()) {
                let value = wide(line);
                SendMessageW(f.list, LB_ADDSTRING, 0, value.as_ptr() as isize);
            }
            f.rendered_rows = rows;
        }
        SendMessageW(f.list, LB_SETCURSEL, f.selected, 0);
    }

    // One raise per job that reached its own completion, the first time that
    // state is seen: the same finished state is rendered on every tick, so
    // nothing here may turn a re-render into a new completion, and a window the
    // user closed is gone rather than resurrected.
    let mut finished = false;
    for index in 0..f.ids.len() {
        if snapshot
            .jobs
            .iter()
            .any(|job| job.id == f.ids[index] && job.state == JobState::Completed)
            && f.raised.insert(f.ids[index].clone())
        {
            finished = true;
        }
    }
    if finished {
        activate_window(hwnd);
    }
}
pub(super) fn media_stream_present(codec: Option<&str>) -> bool {
    codec.is_some_and(|value| value != "none" && value != "unknown")
}

/// Extra picker status lines: the browser session state this handoff carried and
/// the pending source refresh it can serve. Kept out of the layout so the
/// recorded 640x420 geometry stays intact.
pub(super) fn media_status_suffix(f: &MediaFields, snapshot: &AppSnapshot) -> String {
    let mut parts: Vec<String> = Vec::new();
    match f.session_consent {
        Some(true) => {
            parts.push(crate::i18n::ui("Site oturumu: dahil", "Site session: included").to_string())
        }
        Some(false) => {
            parts.push(crate::i18n::ui("Site oturumu: yok", "Site session: none").to_string())
        }
        None => {}
    }
    if let Some(job) = snapshot
        .jobs
        .iter()
        .find(|job| f.renew_ids.iter().any(|id| id == &job.id))
    {
        parts.push(crate::i18n::ui_owned!(
            format!(
                "Bekleyen iş: {} · kaynağı yenilemek için Gelişmiş seçenekler",
                job.name
            ),
            format!(
                "Pending job: {} · use Advanced options to refresh the source",
                job.name
            )
        ));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("\r\n{}", parts.join(" · "))
    }
}

/// Pending source refresh jobs this picker can rebind, in snapshot order.
pub(super) fn media_renew_jobs(snapshot: &AppSnapshot) -> Vec<(String, String)> {
    snapshot
        .jobs
        .iter()
        .filter(|job| job.state == JobState::AwaitingSource && source_refresh_available(job))
        .map(|job| (job.id.clone(), job.name.clone()))
        .collect()
}

pub(super) fn media_format_label(format: &MediaFormat) -> String {
    let dimensions = match (format.width, format.height) {
        (Some(width), Some(height)) => format!("{width}×{height}"),
        (_, Some(height)) => format!("{height}p"),
        _ => crate::i18n::ui("çözünürlük bilinmiyor", "resolution unknown").to_string(),
    };
    let fps = format
        .fps
        .map(|value| format!(" · {value:.2} FPS"))
        .unwrap_or_else(|| crate::i18n::ui(" · FPS bilinmiyor", " · FPS unknown").to_string());
    let codec = format
        .video_codec
        .as_deref()
        .filter(|value| media_stream_present(Some(value)))
        .map(|value| format!(" · {value}"))
        .unwrap_or_default();
    let range = format
        .dynamic_range
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(|value| format!(" · {value}"))
        .unwrap_or_default();
    let size = format
        .filesize
        .map(|value| format!(" · {}", format_bytes(value)))
        .unwrap_or_default();
    format!(
        "{} — {} · {}{}{}{}{}",
        format.id, dimensions, format.extension, fps, codec, range, size
    )
}

pub(super) unsafe fn refresh_external_subtitle_list(f: &MediaFields) {
    SendMessageW(f.external_list, LB_RESETCONTENT, 0, 0);
    for subtitle in &f.external_values {
        let label = format!(
            "{} · {} · {}{}",
            subtitle.language,
            subtitle.kind.to_ascii_uppercase(),
            if subtitle.label.trim().is_empty() {
                &subtitle.url
            } else {
                &subtitle.label
            },
            if subtitle.is_default {
                crate::i18n::ui(" · varsayılan", " · default")
            } else {
                ""
            }
        );
        let value = wide(&label);
        SendMessageW(f.external_list, LB_ADDSTRING, 0, value.as_ptr() as isize);
    }
}

pub(super) unsafe fn selected_list_indexes(list: HWND) -> Vec<usize> {
    let count = SendMessageW(list, LB_GETSELCOUNT, 0, 0) as i32;
    if count <= 0 {
        return Vec::new();
    }
    let mut indexes = vec![0i32; count as usize];
    SendMessageW(
        list,
        LB_GETSELITEMS,
        count as usize,
        indexes.as_mut_ptr() as isize,
    );
    indexes
        .into_iter()
        .filter(|index| *index >= 0)
        .map(|index| index as usize)
        .collect()
}

/// A live/processing broadcast has no finished caption track to require by default.
/// For finite media, prefer authored captions, then the extractor's original-language
/// ASR track rather than an alphabetically first on-demand translation (e.g. Afar).
pub(super) fn subtitle_default_index(tracks: &[SubtitleTrack], is_live: bool) -> Option<usize> {
    if is_live {
        return None;
    }
    tracks
        .iter()
        .position(|track| !track.automatic)
        .or_else(|| {
            tracks
                .iter()
                .position(|track| track.language.ends_with("-orig"))
        })
        .or_else(|| (!tracks.is_empty()).then_some(0))
}

/// Explicit session choices take precedence over automatic preselection.
pub(super) fn subtitle_default_selected(
    subtitle_none: bool,
    previous: &[SubtitleSelection],
    track: &SubtitleTrack,
    is_default: bool,
) -> bool {
    if subtitle_none {
        return false;
    }
    if previous.is_empty() {
        return is_default;
    }
    previous
        .iter()
        .any(|current| current.language == track.language && current.automatic == track.automatic)
}

/// Extension a name without one ends with, for the kind currently chosen.
pub(super) unsafe fn media_extension(f: &MediaFields) -> String {
    let kind = f
        .kind_values
        .get(combo_index(f.kind).max(0) as usize)
        .map(|(kind, _)| *kind);
    match kind {
        Some(DownloadKind::Audio) => f.base.audio_format.clone().unwrap_or_else(|| "m4a".into()),
        _ => f.base.container.clone().unwrap_or_else(|| "mp4".into()),
    }
}

/// The picker's editable name. An empty field keeps the derived default; a name
/// without an extension gets the selected container's. A separator is refused
/// the same way the desktop's own add dialog refuses it.
pub(super) unsafe fn edited_media_name(
    f: &MediaFields,
    extension: &str,
) -> std::result::Result<Option<String>, (String, HWND)> {
    let value = text(f.name).trim().to_string();
    if value.is_empty() {
        return Ok(None);
    }
    if value
        .chars()
        .any(|character| matches!(character, '\\' | '/'))
    {
        return Err((
            crate::i18n::ui(
                "Dosya adı klasör ayırıcı içeremez.",
                "The file name cannot contain a folder separator.",
            )
            .to_string(),
            f.name,
        ));
    }
    let leaf = value.rsplit(['/', '\\']).next().unwrap_or("");
    if leaf.contains('.') {
        Ok(Some(value))
    } else {
        Ok(Some(format!("{value}.{extension}")))
    }
}

pub(super) unsafe fn populate_media(
    hwnd: HWND,
    f: &mut MediaFields,
    info: &MediaInfo,
    status: &str,
) {
    f.loaded_title = info.title.clone();
    if info
        .formats
        .iter()
        .any(|format| media_stream_present(format.audio_codec.as_deref()))
    {
        f.base.expected_audio = Some(true);
    }
    let duration = info
        .duration
        .map(|d| {
            crate::i18n::ui_owned!(
                format!(
                    " · Süre {:02}:{:02}:{:02}",
                    d as u64 / 3600,
                    (d as u64 / 60) % 60,
                    d as u64 % 60
                ),
                format!(
                    " · Duration {:02}:{:02}:{:02}",
                    d as u64 / 3600,
                    (d as u64 / 60) % 60,
                    d as u64 % 60
                )
            )
        })
        .unwrap_or_else(|| {
            crate::i18n::ui(" · Süre bilinmiyor", " · Duration unknown").to_string()
        });
    let tolerance = info
        .duration_tolerance
        .map(|value| {
            crate::i18n::ui_owned!(
                format!(" · doğrulama toleransı ±{value:.1} sn"),
                format!(" · verification tolerance ±{value:.1} s")
            )
        })
        .unwrap_or_default();
    let live_note = if info.is_live {
        crate::i18n::ui(" · Canlı yayın", " · Live")
    } else {
        ""
    };
    let playlist_note = info
        .playlist_count
        .map(|n| {
            crate::i18n::ui_owned!(
                format!(" · Playlist: {n} öğe"),
                format!(" · Playlist: {n} items")
            )
        })
        .unwrap_or_default();
    set_text(
        f.summary,
        &crate::i18n::ui_owned!(
            format!(
                "{}{}{}\r\nKaynak: {}{live_note}{playlist_note}{status}",
                info.title, duration, tolerance, info.extractor,
            ),
            format!(
                "{}{}{}\r\nSource: {}{live_note}{playlist_note}{status}",
                info.title, duration, tolerance, info.extractor,
            )
        ),
    );

    SendMessageW(f.format, CB_RESETCONTENT, 0, 0);
    f.video_formats.clear();
    // The popup this replaces asked for the saved name; the field starts from
    // whatever the handoff suggested, or the source title plus the selected
    // container's extension.
    let fallback = format!(
        "{}.{}",
        crate::engine::safe_filename(&info.title, "video"),
        media_extension(f)
    );
    let chosen = f
        .base
        .filename
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .unwrap_or(fallback);
    set_text(f.name, &chosen);
    combo_add(
        f.format,
        crate::i18n::ui("En iyi kalite (otomatik)", "Best quality (automatic)"),
    );
    f.video_formats.push(None);
    let mut formats: Vec<MediaFormat> = info
        .formats
        .iter()
        .filter(|value| !value.has_drm && media_stream_present(value.video_codec.as_deref()))
        .cloned()
        .collect();
    formats.sort_by(|left, right| {
        right.height.cmp(&left.height).then_with(|| {
            right
                .fps
                .partial_cmp(&left.fps)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    });
    for format in formats {
        combo_add(f.format, &media_format_label(&format));
        f.video_formats.push(Some(format));
    }
    let preferred = f
        .base
        .video_format_id
        .as_deref()
        .and_then(|id| {
            f.video_formats
                .iter()
                .position(|format| format.as_ref().is_some_and(|value| value.id == id))
        })
        .or_else(|| {
            f.base.max_height.and_then(|height| {
                f.video_formats
                    .iter()
                    .position(|format| {
                        format
                            .as_ref()
                            .is_some_and(|value| value.height == Some(height))
                    })
                    // The formats run from the highest down: the first one at or
                    // below the remembered height is the closest match.
                    .or_else(|| {
                        f.video_formats.iter().position(|format| {
                            format
                                .as_ref()
                                .and_then(|value| value.height)
                                .is_some_and(|value| value <= height)
                        })
                    })
            })
        })
        .unwrap_or(0);
    combo_select(f.format, preferred as i32);

    SendMessageW(f.audio_tracks, LB_RESETCONTENT, 0, 0);
    f.audio_values.clear();
    for format in info.formats.iter().filter(|value| {
        !value.has_drm
            && value.video_codec.as_deref() == Some("none")
            && value.audio_codec.as_deref() != Some("none")
    }) {
        if f.audio_values
            .iter()
            .any(|selection| selection.id == format.id)
        {
            continue;
        }
        let selection = AudioSelection {
            id: format.id.clone(),
            language: format.language.clone(),
        };
        let label = format!(
            "{} — {}{}{}",
            format.id,
            format.language.as_deref().unwrap_or("dil bilinmiyor"),
            format
                .audio_codec
                .as_deref()
                .map(|codec| format!(" · {codec}"))
                .unwrap_or_default(),
            format
                .audio_channels
                .map(|channels| format!(" · {channels} kanal"))
                .unwrap_or_default()
        );
        let value = wide(&label);
        let index = SendMessageW(f.audio_tracks, LB_ADDSTRING, 0, value.as_ptr() as isize) as usize;
        if f.base
            .audio_tracks
            .iter()
            .any(|current| current == &selection)
            || (f.base.audio_tracks.is_empty()
                && f.base.audio_format_id.as_deref() == Some(selection.id.as_str()))
        {
            SendMessageW(f.audio_tracks, LB_SETSEL, 1, index as isize);
        }
        f.audio_values.push(selection);
    }

    SendMessageW(f.subtitles, LB_RESETCONTENT, 0, 0);
    f.subtitle_values = if info.subtitle_tracks.is_empty() {
        info.subtitles
            .iter()
            .map(|language| SubtitleTrack {
                language: language.clone(),
                automatic: false,
            })
            .collect()
    } else {
        info.subtitle_tracks.clone()
    };
    let default_subtitle = subtitle_default_index(&f.subtitle_values, info.is_live);
    for (index, subtitle) in f.subtitle_values.iter().enumerate() {
        let value = wide(&format!(
            "{} — {}",
            subtitle.language,
            if subtitle.automatic {
                "otomatik"
            } else {
                "elle"
            }
        ));
        SendMessageW(f.subtitles, LB_ADDSTRING, 0, value.as_ptr() as isize);
        if subtitle_default_selected(
            f.subtitle_none,
            &f.base.subtitle_tracks,
            subtitle,
            default_subtitle == Some(index),
        ) {
            SendMessageW(f.subtitles, LB_SETSEL, 1, index as isize);
        }
    }
    let has_downloadable = info.formats.iter().any(|value| {
        !value.has_drm
            && (value.video_codec.as_deref() != Some("none")
                || value.audio_codec.as_deref() != Some("none"))
    });
    EnableWindow(GetDlgItem(hwnd, M_QUEUE), has_downloadable as BOOL);
    EnableWindow(f.playlist, 1);
}
pub(super) unsafe fn update_media_queue_enabled(
    hwnd: HWND,
    f: &MediaFields,
    snapshot: &AppSnapshot,
) {
    let kind = f
        .kind_values
        .get(combo_index(f.kind).max(0) as usize)
        .map(|(kind, _)| *kind);
    let downloadable = snapshot.media.as_ref().is_some_and(|info| {
        info.formats.iter().any(|format| {
            !format.has_drm
                && (format.video_codec.as_deref() != Some("none")
                    || format.audio_codec.as_deref() != Some("none"))
        })
    });
    // The button offers a selection only once the controls show the analysis
    // this dialog's generation produced.
    let ready = f.populated && f.inspect_generation == snapshot.inspect_generation;
    let enabled = ready
        && downloadable
        && kind.is_some_and(|kind| mode_allows_new_job(snapshot.settings.usage_modes, kind));
    EnableWindow(GetDlgItem(hwnd, M_QUEUE), enabled as BOOL);
}
/// Queues the selection this picker shows.
///
/// `Some(job_ids)` carries the exact ids the engine returned for this request,
/// in its own order: a playlist hands back one entry per item, and the caller
/// tracks those, never a list position or the newest job. `None` means nothing
/// was queued - the selection stays on screen with the reason in the error
/// line - and the window is left alone on every one of those paths.
pub(super) unsafe fn queue_media(
    hwnd: HWND,
    error: HWND,
    app: App,
    f: &MediaFields,
) -> Option<Vec<String>> {
    let Some((kind, container)) = f
        .kind_values
        .get(combo_index(f.kind).max(0) as usize)
        .cloned()
    else {
        set_dialog_error(
            hwnd,
            error,
            crate::i18n::ui(
                "Bu kullanım profili için medya çıktısı kapalı.",
                "Media output is disabled for this usage profile.",
            ),
            f.kind,
        );
        return None;
    };
    if !mode_allows_new_job(app.snapshot().settings.usage_modes, kind) {
        set_dialog_error(hwnd, error, mode_error(kind), f.kind);
        return None;
    }
    let mut request = f.base.clone();
    request.kind = kind;
    request.format_id = None;
    request.video_format_id = None;
    request.max_height = None;
    request.exact_height = None;
    request.audio_format_id = None;
    request.audio_language = None;
    request.audio_tracks.clear();
    request.subtitle_languages.clear();
    request.subtitle_mode = None;
    request.subtitle_tracks.clear();
    request.external_subtitles.clear();

    let audio_indexes = selected_list_indexes(f.audio_tracks);
    if kind == DownloadKind::Video {
        let index = combo_index(f.format).max(0) as usize;
        let selected_format = f
            .video_formats
            .get(index)
            .and_then(|format| format.as_ref());
        request.video_format_id = selected_format.map(|format| format.id.clone());
        request.exact_height = selected_format.and_then(|format| format.height);
        request.max_height = request.exact_height;
        request.container = container;
        request.audio_format = None;
        let selected_audio: Vec<AudioSelection> = audio_indexes
            .into_iter()
            .filter_map(|index| f.audio_values.get(index).cloned())
            .collect();
        if selected_audio.len() == 1 {
            let selection = &selected_audio[0];
            request.audio_format_id = Some(selection.id.clone());
            request.audio_language = selection.language.clone();
        } else {
            request.audio_tracks = selected_audio;
        }
        let selected_subtitles: Vec<SubtitleSelection> = selected_list_indexes(f.subtitles)
            .into_iter()
            .filter_map(|index| f.subtitle_values.get(index))
            .map(|track| SubtitleSelection {
                language: track.language.clone(),
                automatic: track.automatic,
            })
            .collect();
        if selected_subtitles.len() == 1 {
            let subtitle = &selected_subtitles[0];
            request.subtitle_languages = vec![subtitle.language.clone()];
            let mode = if subtitle.automatic {
                "automatic"
            } else {
                "manual"
            };
            request.subtitle_mode = Some(mode.into());
        } else {
            request.subtitle_tracks = selected_subtitles;
        }
        request.external_subtitles = f.external_values.clone();
        if (!request.subtitle_tracks.is_empty() || !request.subtitle_languages.is_empty())
            && !request.external_subtitles.is_empty()
        {
            set_dialog_error(
                hwnd,
                error,
                crate::i18n::ui(
                    "Yerleşik ve harici altyazılar aynı istekte karıştırılamaz.",
                    "Embedded and external subtitles cannot be mixed in one request.",
                ),
                f.subtitles,
            );
            return None;
        }
    } else {
        if audio_indexes.len() > 1 {
            set_dialog_error(
                hwnd,
                error,
                crate::i18n::ui(
                    "Yalnız ses çıktısında tam olarak bir ses parçası seçilebilir.",
                    "Exactly one audio track can be selected for audio-only output.",
                ),
                f.audio_tracks,
            );
            return None;
        }
        request.container = None;
        if let Some(selection) = audio_indexes
            .first()
            .and_then(|index| f.audio_values.get(*index))
        {
            request.audio_format_id = Some(selection.id.clone());
            request.audio_language = selection.language.clone();
        }
        request.audio_format = match combo_index(f.audio) {
            1 => Some("mp3".into()),
            2 => Some("m4a".into()),
            3 => Some("opus".into()),
            4 => Some("flac".into()),
            _ => None,
        };
    }
    request.full_verification = check(f.full_verification);
    request.playlist = check(f.playlist);
    let items = text(f.playlist_items).trim().replace(' ', "");
    if items.is_empty() {
        request.playlist_items = None;
    } else {
        if !items
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, ',' | '-' | ':'))
            || items.len() > 200
        {
            set_dialog_error(
                hwnd,
                error,
                crate::i18n::ui(
                    "Playlist öğeleri 1-10,15 biçiminde olmalı.",
                    "Playlist items must look like 1-10,15.",
                ),
                f.playlist_items,
            );
            return None;
        }
        request.playlist = true;
        request.playlist_items = Some(items);
    }
    if request.playlist {
        // A playlist takes the height bound for every item; item-specific
        // format and track ids do not apply across items.
        request.video_format_id = None;
        request.exact_height = None;
        request.audio_format_id = None;
        request.audio_tracks.clear();
        request.external_subtitles.clear();
    }
    remember_site_choice(&app, f, &request);
    request.request_id = Some(
        f.base
            .request_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
    );
    // The editable name wins; the derived title name is the fallback when the
    // field was cleared.
    let extension = request
        .container
        .as_deref()
        .or(request.audio_format.as_deref())
        .unwrap_or("m4a")
        .to_owned();
    match edited_media_name(f, &extension) {
        Err((message, control)) => {
            set_dialog_error(hwnd, error, &message, control);
            return None;
        }
        Ok(Some(name)) => request.filename = Some(name),
        Ok(None) => {
            if !f.loaded_title.is_empty() {
                let title = crate::engine::safe_filename(&f.loaded_title, "video");
                request.filename = Some(format!("{title}.{extension}"));
            }
        }
    }
    let preference = if request.kind == DownloadKind::Video {
        Some((
            request.container.clone().unwrap_or_else(|| "mp4".into()),
            f.video_formats
                .get(combo_index(f.format).max(0) as usize)
                .and_then(|format| format.as_ref())
                .and_then(|format| format.height),
        ))
    } else {
        None
    };
    let video_request = request.kind == DownloadKind::Video;
    let subtitles_chosen = !request.subtitle_tracks.is_empty()
        || !request.subtitle_languages.is_empty()
        || !request.external_subtitles.is_empty();
    // The result carries the exact job ids the engine created, so the window
    // that takes this selection over follows those jobs and no others.
    match app.dispatch_result(Action::Add { request }) {
        Ok(result) => {
            if video_request {
                // "Altyazı yok" sticks for the session, exactly like the popup.
                app.set_subtitle_none(!subtitles_chosen);
            }
            if let Some((container, height)) = preference {
                let mut settings = app.snapshot().settings.clone();
                settings.last_video_container = container;
                settings.last_video_height = height;
                if let Err(error) = app.dispatch(Action::SetSettings { settings }) {
                    message(
                        hwnd,
                        &crate::i18n::ui_owned!(
                            format!("İndirme eklendi; tercihler kaydedilemedi: {error}"),
                            format!(
                                "The download was added; preferences could not be saved: {error}"
                            )
                        ),
                        crate::i18n::ui("Video tercihi", "Video preferences"),
                        MB_OK | MB_ICONINFORMATION,
                    );
                }
            }
            Some(result.job_ids)
        }
        Err(e) => {
            set_dialog_error(
                hwnd,
                error,
                &crate::i18n::ui_owned!(
                    format!("Medya kuyruğa eklenemedi: {e:#}"),
                    format!("The media could not be queued: {e:#}")
                ),
                null_mut(),
            );
            None
        }
    }
}

/// Keeps the renewal combo in step with the jobs that await a source refresh.
pub(super) unsafe fn refresh_media_renew_jobs(f: &mut MediaFields, snapshot: &AppSnapshot) {
    let jobs = media_renew_jobs(snapshot);
    let ids: Vec<String> = jobs.iter().map(|(id, _)| id.clone()).collect();
    if ids != f.renew_ids {
        let previous = combo_index(f.renew_job).max(0) as usize;
        let previous_id = f.renew_ids.get(previous).cloned();
        SendMessageW(f.renew_job, CB_RESETCONTENT, 0, 0);
        f.renew_ids = ids;
        if f.renew_ids.is_empty() {
            combo_add(
                f.renew_job,
                crate::i18n::ui("Bekleyen iş yok", "No pending job"),
            );
            combo_select(f.renew_job, 0);
        } else {
            for (_, name) in &jobs {
                combo_add(
                    f.renew_job,
                    &crate::i18n::ui_owned!(
                        format!("Bekleyen iş: {name}"),
                        format!("Pending job: {name}")
                    ),
                );
            }
            let index = previous_id
                .and_then(|id| f.renew_ids.iter().position(|current| current == &id))
                .unwrap_or(0);
            combo_select(f.renew_job, index as i32);
        }
    }
    let ready = !f.renew_ids.is_empty();
    EnableWindow(f.renew_job, ready as BOOL);
    EnableWindow(f.renew_restart, ready as BOOL);
    EnableWindow(GetDlgItem(GetParent(f.renew_job), M_RENEW), ready as BOOL);
}

/// Rebinds the selected awaiting-source job to this handoff.
///
/// `BeginSourceRefresh` hands out the token plus the job's exact stored
/// selection, and `CompleteSourceRefresh` delivers the fresh source URL and
/// browser context under it: the job keeps its contract and no second download
/// is queued for the same selection. The preparation probes the new source, so
/// it runs off the UI thread; the picker closes and the job list reports the
/// outcome.
pub(super) unsafe fn renew_source(hwnd: HWND, error: HWND, app: App, f: &MediaFields) {
    let index = combo_index(f.renew_job).max(0) as usize;
    let Some(id) = f.renew_ids.get(index).cloned() else {
        set_dialog_error(
            hwnd,
            error,
            crate::i18n::ui(
                "Kaynağı yenilenecek bekleyen iş yok.",
                "There is no pending job whose source can be refreshed.",
            ),
            f.renew_job,
        );
        return;
    };
    let restart = check(f.renew_restart);
    let url = f.base.url.clone();
    let headers = f.base.headers.clone();
    let session_cookies = f.base.session_cookies.clone();
    let referer = f.base.referer.clone();
    // The same editable name the queue path forwards: a rebind renames the
    // pending job's target instead of keeping the stale one.
    let extension = f
        .base
        .container
        .as_deref()
        .or(f.base.audio_format.as_deref())
        .unwrap_or("m4a")
        .to_owned();
    let rename = match edited_media_name(f, &extension) {
        Err((message, control)) => {
            set_dialog_error(hwnd, error, &message, control);
            return;
        }
        Ok(value) => value,
    };
    let spawned = std::thread::Builder::new()
        .name("source-refresh-dialog".into())
        .spawn(move || {
            let ticket = match app.dispatch_result(Action::BeginSourceRefresh { id: id.clone() }) {
                Ok(result) => match result.source_refresh {
                    Some(ticket) => ticket,
                    None => {
                        app.message(
                            crate::i18n::ui(
                                "Kaynak yenileme bağlantısı alınamadı.",
                                "The source refresh link could not be obtained.",
                            )
                            .to_string(),
                            true,
                        );
                        return;
                    }
                },
                Err(_) => return,
            };
            let mut request = ticket.request.clone();
            request.url = url;
            request.headers = headers;
            request.session_cookies = session_cookies;
            request.referer = referer;
            if let Some(name) = rename {
                request.filename = Some(name);
            }
            let _ = app.dispatch(Action::CompleteSourceRefresh {
                id,
                token: ticket.token,
                request,
                restart,
            });
        });
    if spawned.is_err() {
        set_dialog_error(
            hwnd,
            error,
            crate::i18n::ui(
                "Kaynak yenileme işçisi başlatılamadı.",
                "The source refresh worker could not be started.",
            ),
            null_mut(),
        );
        return;
    }
    DestroyWindow(hwnd);
}

pub(super) fn direct_file_request(request: &AddRequest) -> bool {
    if request.kind == DownloadKind::File {
        return true;
    }
    if request.kind != DownloadKind::Auto {
        return false;
    }
    url::Url::parse(&request.url).is_ok_and(|url| {
        url.scheme() == "ftp"
            || std::path::Path::new(url.path())
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| {
                    matches!(
                        e.to_ascii_lowercase().as_str(),
                        "zip"
                            | "exe"
                            | "msi"
                            | "pdf"
                            | "rar"
                            | "7z"
                            | "iso"
                            | "dmg"
                            | "tar"
                            | "gz"
                            | "jpg"
                            | "png"
                            | "docx"
                            | "xlsx"
                    )
                })
    })
}

/// Stores or clears this site's remembered choice from the picker's switch.
unsafe fn remember_site_choice(app: &App, f: &MediaFields, request: &AddRequest) {
    if f.host.is_empty() {
        return;
    }
    let mut settings = app.snapshot().settings;
    let before = settings.site_media_defaults.clone();
    settings
        .site_media_defaults
        .retain(|entry| !entry.host.eq_ignore_ascii_case(&f.host));
    if check(f.remember) {
        settings
            .site_media_defaults
            .push(crate::model::SiteMediaDefault {
                host: f.host.clone(),
                height: request.exact_height,
                container: request.container.clone().unwrap_or_default(),
                audio_only: request.kind == DownloadKind::Audio,
                auto: true,
            });
        // Bounded: the oldest remembered sites leave first.
        let excess = settings.site_media_defaults.len().saturating_sub(100);
        settings.site_media_defaults.drain(..excess);
    }
    if settings.site_media_defaults != before {
        let _ = app.update_settings_quiet(settings);
    }
}

/// Fetches a thumbnail off the window thread, converts it with the verified FFmpeg
/// to a small bitmap and posts it to the picker, which shows it next to the summary.
pub(super) fn load_thumbnail(hwnd: HWND, app: App, url: String) {
    let target = hwnd as isize;
    let _ = std::thread::Builder::new()
        .name("media-thumbnail".into())
        .spawn(move || {
            let Some(bitmap) = fetch_thumbnail(&app, &url) else {
                return;
            };
            unsafe {
                if PostMessageW(target as HWND, WM_THUMBNAIL, 0, bitmap as LPARAM) == 0 {
                    DeleteObject(bitmap as HGDIOBJ);
                }
            }
        });
}

fn fetch_thumbnail(app: &App, url: &str) -> Option<HBITMAP> {
    let parsed = url::Url::parse(url).ok()?;
    if !matches!(parsed.scheme(), "https" | "http") {
        return None;
    }
    let control = crate::model::TransferControl::default();
    let mut easy = app
        .network_governor()
        .easy_for_url(&parsed, &control)
        .ok()?;
    easy.url(parsed.as_str()).ok()?;
    easy.follow_location(true).ok()?;
    easy.max_redirections(3).ok()?;
    easy.timeout(Duration::from_secs(10)).ok()?;
    let mut data = Vec::new();
    {
        let mut transfer = easy.transfer();
        transfer
            .write_function(|chunk| {
                if data.len() + chunk.len() > 4 * 1024 * 1024 {
                    return Ok(0);
                }
                data.extend_from_slice(chunk);
                Ok(chunk.len())
            })
            .ok()?;
        transfer.perform().ok()?;
    }
    if easy.response_code().ok()? / 100 != 2 || data.is_empty() {
        return None;
    }
    drop(easy);
    let tools = crate::tools::verified_toolchain(app.paths(), &control).ok()?;
    let folder = std::env::temp_dir().join(format!("ssdownload-thumb-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&folder).ok()?;
    let input = folder.join("thumbnail.img");
    let output = folder.join("thumbnail.bmp");
    let result = (|| {
        std::fs::write(&input, &data).ok()?;
        use std::os::windows::process::CommandExt;
        let status = std::process::Command::new(&tools.ffmpeg)
            .creation_flags(0x0800_0000)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
            .arg(&input)
            .args([
                "-vf",
                "scale=100:56:force_original_aspect_ratio=decrease",
                "-frames:v",
                "1",
                "-pix_fmt",
                "bgr24",
            ])
            .arg(&output)
            .status()
            .ok()?;
        if !status.success() {
            return None;
        }
        let path = wide(&output.to_string_lossy());
        let bitmap = unsafe {
            LoadImageW(
                null_mut(),
                path.as_ptr(),
                IMAGE_BITMAP,
                0,
                0,
                LR_LOADFROMFILE,
            )
        };
        (!bitmap.is_null()).then_some(bitmap as HBITMAP)
    })();
    let _ = std::fs::remove_dir_all(&folder);
    result
}
